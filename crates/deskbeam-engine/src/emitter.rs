//! Receives DashBeam's node events and hands them to the engine.
//!
//! `NodeService` reports through the `EventEmitter` trait with string names and JSON-string
//! payloads (it used to drive Tauri's event bus). This forwards them into a channel the
//! engine drains, so the node never calls into the engine directly.

use protocol::EventEmitter;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub(crate) struct NodeEvent {
    pub name: String,
    pub payload: Option<String>,
}

pub(crate) struct ChannelEmitter {
    pub tx: UnboundedSender<NodeEvent>,
}

impl EventEmitter for ChannelEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.tx
            .send(NodeEvent {
                name: event_name.to_string(),
                payload: None,
            })
            .map_err(|e| e.to_string())
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.tx
            .send(NodeEvent {
                name: event_name.to_string(),
                payload: Some(payload.to_string()),
            })
            .map_err(|e| e.to_string())
    }
}
