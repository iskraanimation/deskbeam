//! LAN mode (plan §2, option B): this device and the paired devices it lets in share a
//! virtual network.
//!
//! - `lan.start` picks this device's address, makes the network adapter (asking for admin
//!   rights through the helper when the app doesn't have them) and joins the LAN.
//! - The LAN's members are the paired devices with "LAN mode" allowed. Both devices must
//!   allow each other: each side's [`deskbeam_lan::Peers::allow`] asks its own store.
//! - `lan.stop`, or the app quitting, closes every link and removes the adapter.

use std::{path::PathBuf, sync::Weak};

use deskbeam_lan::{Device, LanService, MemoryOs, PeerState, Peers};
use deskbeam_model::{DeviceLan, LanMode, PairingState};
use iroh::{EndpointId, endpoint::Connection};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::warn;

use crate::{
    command::CommandError,
    engine::{Engine, Inner, paired_id},
};

/// Where LAN mode's network adapter comes from.
#[derive(Debug, Clone)]
pub enum LanBackend {
    /// A real adapter: made in this process when it has admin rights, else by the helper
    /// started from `helper` (the app's own program) through the OS's prompt.
    System { helper: Option<PathBuf> },
    /// A memory device per start, whose OS side is sent here. For tests.
    Memory(mpsc::UnboundedSender<MemoryOs>),
}

impl Default for LanBackend {
    fn default() -> Self {
        Self::System {
            helper: std::env::current_exe().ok(),
        }
    }
}

pub(crate) type Lan = LanService<EnginePeers>;

/// The engine's answers to the LAN's questions.
pub(crate) struct EnginePeers {
    pub(crate) inner: Weak<Inner>,
}

impl Peers for EnginePeers {
    async fn allow(&self, peer: EndpointId) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let Some(node) = inner.node.read().await.clone() else {
            return false;
        };
        let Ok(id) = paired_id(&node, &peer.to_string()) else {
            return false;
        };
        match inner.store.permissions(&id) {
            Ok(p) => p.join_lan,
            Err(e) => {
                warn!("LAN: reading permissions: {e}");
                false
            }
        }
    }

    async fn dial(&self, peer: EndpointId) -> anyhow::Result<Connection> {
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
        Ok(endpoint.connect(peer, deskbeam_lan::ALPN).await?)
    }

    fn online(&self, peer: EndpointId) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let peer = peer.to_string();
        inner
            .snapshot
            .borrow()
            .devices
            .iter()
            .any(|d| d.online && d.endpoint_id.eq_ignore_ascii_case(&peer))
    }
}

impl Engine {
    pub(crate) async fn lan_start(&self) -> Result<Value, CommandError> {
        if let Some(status) = self.inner.lan.status() {
            return Ok(json!({ "address": status.address.to_string() }));
        }
        {
            let mut mode = self.inner.lan_mode.lock().expect("lock");
            if *mode == LanMode::Starting {
                return Err(CommandError::Failed("LAN mode is starting".into()));
            }
            *mode = LanMode::Starting;
        }
        let node = match self.node().await {
            Ok(node) => node,
            Err(e) => {
                *self.inner.lan_mode.lock().expect("lock") = LanMode::Off;
                return Err(e);
            }
        };
        self.refresh().await;

        let started = async {
            let me: EndpointId = node
                .device_info()
                .endpoint_id
                .parse()
                .map_err(|e| anyhow::anyhow!("this device's id: {e}"))?;
            let paired: Vec<EndpointId> = node
                .list_paired()?
                .iter()
                .filter_map(|d| d.endpoint_id.parse().ok())
                .collect();
            let address = deskbeam_lan::addr::choose(&me, &paired);
            let device = match &self.inner.lan_backend {
                LanBackend::Memory(sink) => {
                    let (device, os) = Device::memory();
                    let _ = sink.send(os);
                    device
                }
                LanBackend::System { helper } => Device::open(address, helper.as_deref()).await?,
            };
            let adapter = device.name().to_string();
            self.inner.lan.start(me, address, device)?;
            anyhow::Ok((address, adapter))
        }
        .await;

        match started {
            Ok((address, adapter)) => {
                *self.inner.lan_mode.lock().expect("lock") = LanMode::On {
                    address: address.to_string(),
                    adapter,
                };
                Ok(json!({ "address": address.to_string() }))
            }
            Err(e) => {
                let reason = format!("{e:#}");
                warn!("LAN mode did not start: {reason}");
                *self.inner.lan_mode.lock().expect("lock") = LanMode::Failed {
                    reason: reason.clone(),
                };
                Err(CommandError::Failed(reason))
            }
        }
    }

    pub(crate) fn lan_stop(&self) -> Value {
        self.inner.lan.stop();
        *self.inner.lan_mode.lock().expect("lock") = LanMode::Off;
        Value::Null
    }

    /// Tells the LAN who its members are now, and returns each member's state by id.
    pub(crate) fn lan_members(
        &self,
        devices: &[deskbeam_model::Device],
    ) -> std::collections::HashMap<String, DeviceLan> {
        if !self.inner.lan.is_running() {
            return Default::default();
        }
        let members: Vec<(String, EndpointId)> = devices
            .iter()
            .filter(|d| d.permissions.join_lan && d.pairing == PairingState::Active)
            .filter_map(|d| Some((d.endpoint_id.clone(), d.endpoint_id.parse().ok()?)))
            .collect();
        self.inner.lan.set_wanted(members.iter().map(|(_, id)| *id));
        let Some(status) = self.inner.lan.status() else {
            return Default::default();
        };
        members
            .into_iter()
            .map(|(name, id)| {
                let state = match status.peers.get(&id) {
                    Some(PeerState::Connected { address }) => DeviceLan::Connected {
                        address: address.to_string(),
                    },
                    Some(PeerState::Refused(refusal)) => DeviceLan::Refused {
                        reason: refusal.to_string(),
                    },
                    Some(PeerState::Connecting | PeerState::Failed(_)) | None => {
                        DeviceLan::Connecting
                    }
                };
                (name, state)
            })
            .collect()
    }
}
