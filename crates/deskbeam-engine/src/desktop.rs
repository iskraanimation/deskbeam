//! Remote desktop over the tunnel (plan §2).
//!
//! - Host: a [`TunnelServer`] on the node's endpoint. Its [`Gate`] lets a paired device
//!   through when this device allowed it ("Can control this desktop"), or after the owner
//!   allows a request on screen. Unpaired peers never get through.
//! - Client: `rdp.connect` keeps a [`Forward`] per device on a local port kept for that
//!   device, so a bookmark in the RDP client keeps working.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use deskbeam_model::{DesktopRequest, Permission};
use deskbeam_tunnel::{ALPN, Dialer, Forward, Gatekeeper, Outcome, Service, TunnelServer};
use iroh::{EndpointId, endpoint::Connection};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::{
    Event,
    command::CommandError,
    engine::{Engine, Inner, paired_id},
};

/// The first local port handed to a device's tunnel. Later devices get the next ones.
pub const FIRST_PORT: u16 = 13390;
/// How long the owner has to answer a request on screen.
const ASK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long "Allow once" lets a device in.
const ALLOW_ONCE_FOR: Duration = Duration::from_secs(30 * 60);
/// The store key of the device → local port map.
const PORTS_KEY: &str = "desktop_ports";

/// Builds the host's tunnel server for the engine behind `inner`.
pub(crate) fn server(inner: Weak<Inner>, target: SocketAddr) -> TunnelServer<Gate> {
    TunnelServer::new(
        Gate { inner },
        HashMap::from([(Service::RemoteDesktop, target)]),
    )
}

/// The host's gatekeeper.
pub(crate) struct Gate {
    inner: Weak<Inner>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

impl Gatekeeper for Gate {
    async fn allow(&self, peer: EndpointId, service: Service) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        match service {
            Service::RemoteDesktop => Engine { inner }.authorize_desktop(peer).await,
        }
    }
}

/// Dials a paired device's tunnel with the node's endpoint as it is at that moment.
struct NodeDialer {
    inner: Weak<Inner>,
    remote: EndpointId,
}

impl Dialer for NodeDialer {
    async fn dial(&self) -> anyhow::Result<Connection> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the engine stopped"))?;
        let node = inner
            .node
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("the node is not running"))?;
        let endpoint = node.endpoint().await;
        Ok(endpoint.connect(self.remote, ALPN).await?)
    }
}

impl Engine {
    /// On the host: may `peer` control this desktop? Waits for the owner when it has to ask.
    pub(crate) async fn authorize_desktop(&self, peer: EndpointId) -> bool {
        let Ok(node) = self.node().await else {
            return false;
        };
        let Ok(id) = paired_id(&node, &peer.to_string()) else {
            debug!(%peer, "desktop: not a paired device");
            return false;
        };
        match self.inner.store.permissions(&id) {
            Ok(p) if p.remote_desktop => return true,
            Ok(_) => {}
            Err(e) => {
                warn!("desktop: reading permissions: {e}");
                return false;
            }
        }
        if let Some(until) = self
            .inner
            .desktop_allowed_until
            .lock()
            .expect("lock")
            .get(&id)
            && Instant::now() < *until
        {
            return true;
        }

        // Ask the owner. Streams arriving while a request is open wait for the same answer.
        let mut answer = {
            let mut waiters = self.inner.desktop_waiters.lock().expect("lock");
            match waiters.get(&id) {
                Some(sender) => sender.subscribe(),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    waiters.insert(id.clone(), sender);
                    let display_name = node
                        .list_paired()
                        .ok()
                        .and_then(|list| list.into_iter().find(|d| d.endpoint_id == id))
                        .map(|d| d.display_name)
                        .unwrap_or_default();
                    self.inner
                        .desktop_requests
                        .lock()
                        .expect("lock")
                        .push(DesktopRequest {
                            endpoint_id: id.clone(),
                            display_name: display_name.clone(),
                        });
                    let _ = self.inner.events.send(Event::DesktopRequested {
                        endpoint_id: id.clone(),
                        display_name,
                    });
                    receiver
                }
            }
        };
        self.refresh().await;

        let decided = tokio::time::timeout(ASK_TIMEOUT, async {
            loop {
                if let Some(allowed) = *answer.borrow_and_update() {
                    return allowed;
                }
                if answer.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await;
        match decided {
            Ok(allowed) => allowed,
            Err(_) => {
                debug!(%id, "desktop request not answered in time");
                self.end_desktop_request(&id);
                self.refresh().await;
                false
            }
        }
    }

    /// Takes a request off the list and its waiters away; returns whether there was one.
    fn end_desktop_request(&self, id: &str) -> Option<watch::Sender<Option<bool>>> {
        self.inner
            .desktop_requests
            .lock()
            .expect("lock")
            .retain(|r| r.endpoint_id != id);
        self.inner.desktop_waiters.lock().expect("lock").remove(id)
    }

    pub(crate) fn answer_desktop(
        &self,
        endpoint_id: &str,
        allow: bool,
        always: bool,
    ) -> Result<Value, CommandError> {
        let id = {
            let requests = self.inner.desktop_requests.lock().expect("lock");
            requests
                .iter()
                .find(|r| r.endpoint_id.eq_ignore_ascii_case(endpoint_id))
                .map(|r| r.endpoint_id.clone())
                .ok_or_else(|| CommandError::NoDesktopRequest(endpoint_id.to_string()))?
        };
        if allow && always {
            let mut permissions = self.inner.store.permissions(&id)?;
            permissions.set(Permission::RemoteDesktop, true);
            self.inner.store.set_permissions(&id, &permissions)?;
        } else if allow {
            self.inner
                .desktop_allowed_until
                .lock()
                .expect("lock")
                .insert(id.clone(), Instant::now() + ALLOW_ONCE_FOR);
        }
        if let Some(sender) = self.end_desktop_request(&id) {
            let _ = sender.send(Some(allow));
        }
        Ok(Value::Null)
    }

    /// On the client: opens (or reuses) the tunnel to a device's desktop.
    pub(crate) async fn connect_desktop(
        &self,
        endpoint_id: &str,
        launch: bool,
    ) -> Result<Value, CommandError> {
        let node = self.node().await?;
        let id = paired_id(&node, endpoint_id)?;
        let device = node
            .list_paired()?
            .into_iter()
            .find(|d| d.endpoint_id == id)
            .ok_or_else(|| CommandError::UnknownDevice(id.clone()))?;
        if !device.online {
            return Err(CommandError::Offline(device.display_name));
        }
        let remote: EndpointId = id
            .parse()
            .map_err(|e| CommandError::Failed(format!("{e}")))?;

        let port = {
            let mut forwards = self.inner.forwards.lock().await;
            match forwards.get(&id) {
                Some(forward) => forward.local_addr().port(),
                None => {
                    let forward = self.start_forward(&id, remote).await?;
                    let port = forward.local_addr().port();
                    forwards.insert(id.clone(), forward);
                    port
                }
            }
        };

        let file = deskbeam_rdp::write_rdp_file(
            &self.inner.data_dir.join("rdp"),
            &device.display_name,
            port,
        )?;
        if launch {
            deskbeam_rdp::launch(&file, port)?;
        }
        Ok(json!({
            "port": port,
            "address": format!("localhost:{port}"),
            "rdpFile": file,
        }))
    }

    /// Starts a forward on the device's kept port, or on the next free one.
    async fn start_forward(&self, id: &str, remote: EndpointId) -> Result<Forward, CommandError> {
        let mut ports: HashMap<String, u16> = self.stored(PORTS_KEY);
        let wanted = ports.get(id).copied().unwrap_or_else(|| {
            ports
                .values()
                .copied()
                .max()
                .map_or(FIRST_PORT, |p| p.saturating_add(1))
        });

        let events = self.inner.events.clone();
        let device = id.to_string();
        let report = move |outcome: Outcome| {
            let endpoint_id = device.clone();
            let _ = events.send(match outcome {
                Outcome::Opened => Event::DesktopOpened { endpoint_id },
                Outcome::Refused(status) => Event::DesktopRefused {
                    endpoint_id,
                    reason: status.to_string(),
                },
                Outcome::Failed(reason) => Event::DesktopRefused {
                    endpoint_id,
                    reason,
                },
            });
        };
        let report = Arc::new(report);

        // A port another program took is skipped; the device keeps the one it gets.
        let mut last_error = None;
        for port in wanted..wanted.saturating_add(50) {
            let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let dialer = NodeDialer {
                inner: Arc::downgrade(&self.inner),
                remote,
            };
            let report = report.clone();
            match Forward::start(bind, Service::RemoteDesktop, dialer, move |o| report(o)).await {
                Ok(forward) => {
                    if ports.get(id) != Some(&port) {
                        ports.insert(id.to_string(), port);
                        let json = serde_json::to_string(&ports).map_err(anyhow::Error::from)?;
                        self.inner.store.set_setting(PORTS_KEY, &json)?;
                    }
                    return Ok(forward);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last_error = Some(e),
                Err(e) => return Err(CommandError::Failed(format!("listen on {bind}: {e}"))),
            }
        }
        Err(CommandError::Failed(format!(
            "no free local port from {wanted}: {}",
            last_error.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    pub(crate) async fn disconnect_desktop(
        &self,
        endpoint_id: &str,
    ) -> Result<Value, CommandError> {
        let mut forwards = self.inner.forwards.lock().await;
        let key = forwards
            .keys()
            .find(|k| k.eq_ignore_ascii_case(endpoint_id))
            .cloned();
        match key {
            Some(key) => {
                forwards.remove(&key);
                Ok(Value::Null)
            }
            None => Err(CommandError::Failed(format!(
                "no open tunnel to {}",
                deskbeam_model::short_id(endpoint_id)
            ))),
        }
    }
}
