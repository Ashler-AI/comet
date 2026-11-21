//! Crew: a Scaffold sandbox session's chat and host rows, as its controller
//! publishes them (crew_cloud.rs; zeron-doc's `registry/crew.rs`).

use serde_json::Value;
use zeron_proto::{Chat, Device};

use super::WorkspaceHost;
use crate::EngineError;

impl WorkspaceHost {
    /// A chat row's Crew metadata (none on an upstream chat).
    pub fn crew_chat_metadata(&self, chat_id: &str) -> Option<Value> {
        self.read(|doc| doc.crew_chat_metadata(chat_id))
    }

    /// Publish a sandbox session's chat (whole when new) with its Crew
    /// metadata, and its host's device row when there's none: one batch.
    pub fn crew_publish_chat(&self, chat: &Chat, crew: &Value, whole: bool, device: Option<&Device>) -> Result<(), EngineError> {
        Ok(self.mutate(|doc| {
            doc.crew_publish_chat(chat, crew, whole)?;
            if let Some(device) = device {
                doc.upsert_device(device)?;
            }
            Ok::<_, zeron_doc::DocError>(())
        })?)
    }
}
