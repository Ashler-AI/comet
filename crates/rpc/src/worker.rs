//! Local native worker orchestration. Messaging remains SendPeerMessage / ReplyPeerMessage.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EnsureWorkerSessionParams {
    /// Caller-persisted UUID, reused verbatim after a lost response.
    pub chat_id: String,
    pub owner_chat_id: String,
    pub project_path: String,
    pub base_ref: String,
    pub title: String,
    pub model: String,
    pub effort: comet_proto::ReasoningLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkerSessionParams {
    pub chat_id: String,
    pub owner_chat_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerSessionAction {
    Interrupt,
    Recover,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ControlWorkerSessionParams {
    pub chat_id: String,
    pub owner_chat_id: String,
    pub action: WorkerSessionAction,
}
