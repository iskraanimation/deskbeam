//! DeskBeam's additions to DashBeam's `NodeService`.
//!
//! They live in this file, not in `node.rs`, so DashBeam's own files stay as upstream wrote
//! them and its fixes still apply as patches.

use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, DynProtocolHandler, ProtocolHandler, RouterBuilder};

use crate::node::NodeService;

/// Protocols DeskBeam serves on the node's endpoint besides DashBeam's control protocol,
/// such as the remote-desktop tunnel. The node registers them each time it (re)builds its
/// endpoint. Each handler checks for itself who may use it: the endpoint's access hook only
/// guards the control protocol.
#[derive(Clone, Default)]
pub struct ExtraProtocols(Vec<(Vec<u8>, Arc<dyn DynProtocolHandler>)>);

impl ExtraProtocols {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, alpn: &[u8], handler: impl ProtocolHandler) -> Self {
        self.0.push((alpn.to_vec(), Arc::new(handler)));
        self
    }

    pub(crate) fn with_alpns(&self, mut alpns: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        alpns.extend(self.0.iter().map(|(alpn, _)| alpn.clone()));
        alpns
    }

    pub(crate) fn register(&self, mut router: RouterBuilder) -> RouterBuilder {
        for (alpn, handler) in &self.0 {
            router = router.accept(alpn, Shared(handler.clone()));
        }
        router
    }
}

impl std::fmt::Debug for ExtraProtocols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(alpn, _)| String::from_utf8_lossy(alpn)))
            .finish()
    }
}

/// One handler shared by every endpoint the node builds.
#[derive(Debug)]
struct Shared(Arc<dyn DynProtocolHandler>);

impl ProtocolHandler for Shared {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.0.accept(connection).await
    }

    async fn shutdown(&self) {
        self.0.shutdown().await
    }
}

/// How the live connection to a paired device runs (plan §2b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Straight between the two devices, after hole punching or on the LAN.
    Direct,
    /// Through a relay, which only sees encrypted traffic.
    Relayed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub kind: LinkKind,
    /// The round-trip time on the path in use, in milliseconds.
    pub rtt_ms: u32,
}

impl NodeService {
    /// The node's iroh endpoint now. It is rebuilt when network settings change, so dial
    /// with a fresh one each time instead of keeping it.
    pub async fn endpoint(&self) -> iroh::Endpoint {
        self.runtime.lock().await.endpoint.clone()
    }

    /// This device's key, for signing what it writes (chat messages, groups).
    pub fn secret_key(&self) -> iroh::SecretKey {
        self.identity.secret_key.clone()
    }

    /// The path the control connection to a paired device uses now, or `None` while there is
    /// no live connection to it.
    pub async fn link_to(&self, endpoint_id: &str) -> Option<Link> {
        let conn = self.paired_connections.live_session(endpoint_id).await?;
        let paths = conn.paths();
        let selected = paths.iter().find(|path| path.is_selected())?;
        let kind = if selected.is_ip() {
            LinkKind::Direct
        } else {
            LinkKind::Relayed
        };
        let rtt_ms = u32::try_from(selected.rtt().as_millis()).unwrap_or(u32::MAX);
        Some(Link { kind, rtt_ms })
    }
}
