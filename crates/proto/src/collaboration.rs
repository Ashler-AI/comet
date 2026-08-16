//! Versioned collaboration records shared by the document, engine, edge, and UI layers.
//!
//! These records describe internal Scaffold collaboration. Authority is an authenticated
//! control-plane principal constrained to project/deployment/session scope; no application
//! tenant or customer database identity participates in room authorization.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const COLLABORATION_SCHEMA_VERSION: u32 = 2;
pub const CAPABILITY_SESSION_READ: &str = "session.read";
pub const CAPABILITY_SESSION_CHAT: &str = "session.chat";
pub const CAPABILITY_SESSION_CONTROL: &str = "session.control";
pub const CAPABILITY_SESSION_ANNOTATE: &str = "session.annotate";
pub const CAPABILITY_SESSION_INVITE: &str = "session.invite";
pub const CAPABILITY_SESSION_FILES: &str = "session.files";
pub const CAPABILITY_SESSION_ENVIRONMENT: &str = "session.environment";

pub type UnknownFields = BTreeMap<String, serde_json::Value>;

/// Verified internal identity and its explicit control-plane scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationPrincipal {
    /// Stable IAP/control-plane subject. Email is display-only and never an authority key.
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Versioned control-plane capability strings.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

impl CollaborationPrincipal {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|value| value == capability)
    }

    /// Scope equality is deliberately strict: a broad project claim cannot silently become a
    /// deployment/session claim and a client cannot widen its verified scope.
    pub fn authorizes_scope(&self, scope: &CollaborationScope) -> bool {
        self.project_id == scope.project_id
            && self.deployment_id == scope.deployment_id
            && self.session_id == scope.session_id
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationScope {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

/// Parse the immutable Scaffold device identity
/// `comet-scaffold-{sandbox_id}-e{lifecycle_epoch}`.
pub fn parse_scaffold_device_id(device_id: &str) -> Option<(&str, u64)> {
    let target = device_id.strip_prefix("comet-scaffold-")?;
    let (sandbox_id, epoch) = target.rsplit_once("-e")?;
    let epoch = epoch.parse::<u64>().ok().filter(|value| *value >= 1)?;
    (!sandbox_id.is_empty()).then_some((sandbox_id, epoch))
}

/// An installed-app invitation routes to one durable chat, independently-owned
/// agent session, and verified grant. The grant id is a lookup key, never a
/// credential; authority still comes from the authenticated host projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CometInvitation {
    pub chat_id: String,
    pub session_id: String,
    pub grant_id: String,
}

impl CometInvitation {
    pub const SCHEME_PREFIX: &'static str = "comet://invite/";

    pub fn new(
        chat_id: impl Into<String>,
        session_id: impl Into<String>,
        grant_id: impl Into<String>,
    ) -> Option<Self> {
        let invitation = Self {
            chat_id: chat_id.into(),
            session_id: session_id.into(),
            grant_id: grant_id.into(),
        };
        invitation.is_valid().then_some(invitation)
    }

    pub fn deep_link(&self) -> String {
        debug_assert!(self.is_valid());
        format!(
            "{}{}/{}/{}",
            Self::SCHEME_PREFIX,
            self.chat_id,
            self.session_id,
            self.grant_id
        )
    }

    pub fn parse_deep_link(value: &str) -> Option<Self> {
        let path = value.strip_prefix(Self::SCHEME_PREFIX)?;
        if path.contains('?') || path.contains('#') {
            return None;
        }
        let mut segments = path.split('/');
        let invitation = Self::new(segments.next()?, segments.next()?, segments.next()?)?;
        segments.next().is_none().then_some(invitation)
    }

    fn is_valid(&self) -> bool {
        [&self.chat_id, &self.session_id, &self.grant_id]
            .into_iter()
            .all(|value| {
                !value.is_empty()
                    && value.len() <= 256
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityGrant {
    pub id: String,
    pub principal_subject: String,
    pub scope: CollaborationScope,
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_epoch: Option<u64>,
    pub granted_by: String,
    pub granted_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

impl CapabilityGrant {
    pub fn permits(&self, principal: &CollaborationPrincipal, capability: &str, now: i64) -> bool {
        self.principal_subject == principal.subject
            && principal.authorizes_scope(&self.scope)
            && self.expires_at.is_none_or(|expires| now < expires)
            && self.revoked_at.is_none_or(|revoked| now < revoked)
            && self.capabilities.iter().any(|value| value == capability)
            && principal.has_capability(capability)
    }
}

/// Exact grant envelope emitted by the edge on the authenticated DeviceRoom
/// host socket (`k=\"grant\"`). It is never accepted from UI/local RPC params
/// and is kept only in host-local authority state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedCapabilityGrantEnvelope {
    pub grant: CapabilityGrant,
    pub room_id: String,
    pub target_device_id: String,
    pub target_session_id: String,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

/// Narrow, revocable enrollment for a headless Scaffold device. The credential used to
/// redeem this grant is intentionally not representable in the shared document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceJoinGrant {
    pub id: String,
    pub principal_subject: String,
    pub scope: CollaborationScope,
    pub sandbox_id: String,
    pub device_id: String,
    pub capabilities: Vec<String>,
    pub issued_by: String,
    pub issued_at: i64,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redeemed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

impl DeviceJoinGrant {
    pub fn active_for(
        &self,
        principal: &CollaborationPrincipal,
        device_id: &str,
        now: i64,
    ) -> bool {
        self.principal_subject == principal.subject
            && self.device_id == device_id
            && principal.authorizes_scope(&self.scope)
            && now < self.expires_at
            && self.revoked_at.is_none_or(|revoked| now < revoked)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParticipantState {
    Active,
    Idle,
    Disconnected,
}

pub const MAX_PRESENCE_TARGET_ID_BYTES: usize = 256;
pub const MAX_PRESENCE_BYTE_OFFSET: u32 = 16 * 1024 * 1024;

/// Ephemeral collaborator caret/selection anchored to a stable publication target.
/// Offsets are UTF-8 bytes; `selection` is half-open and absent for a collapsed caret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantCursor {
    pub target_id: String,
    pub caret: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Utf8ByteRange>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

impl ParticipantCursor {
    pub fn is_bounded(&self) -> bool {
        !self.target_id.is_empty()
            && self.target_id.len() <= MAX_PRESENCE_TARGET_ID_BYTES
            && self.caret <= MAX_PRESENCE_BYTE_OFFSET
            && self.selection.as_ref().is_none_or(|range| {
                range.start <= range.end
                    && range.end <= MAX_PRESENCE_BYTE_OFFSET
                    && self.caret >= range.start
                    && self.caret <= range.end
            })
    }

    pub fn is_valid_for_text(&self, text: &str) -> bool {
        self.is_bounded()
            && usize::try_from(self.caret)
                .ok()
                .is_some_and(|caret| caret <= text.len() && text.is_char_boundary(caret))
            && self.selection.as_ref().is_none_or(|range| {
                let Ok(start) = usize::try_from(range.start) else {
                    return false;
                };
                let Ok(end) = usize::try_from(range.end) else {
                    return false;
                };
                end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
            })
    }
}

/// Ephemeral participant presence. It is typed on the wire but never appended to the Loro oplog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParticipantPresence {
    pub principal_subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub device_id: String,
    pub state: ParticipantState,
    pub last_seen_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ParticipantCursor>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

pub fn participant_presence_key(subject: &str, device_id: &str) -> String {
    format!("participant/{subject}/{device_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentSessionSource {
    Local,
    Scaffold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentRoutingMode {
    Automatic,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRouteFallback {
    Disabled,
}

/// Credential-free inference intent. `account_id` is an opaque Agent Auth
/// account key, never a provider credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRoute {
    pub provider: AgentProvider,
    pub model: String,
    pub fallback: AgentRouteFallback,
    pub routing_mode: AgentRoutingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl AgentRoute {
    pub fn automatic(provider: AgentProvider, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            fallback: AgentRouteFallback::Disabled,
            routing_mode: AgentRoutingMode::Automatic,
            account_id: None,
        }
    }

    pub fn pinned(
        provider: AgentProvider,
        model: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            fallback: AgentRouteFallback::Disabled,
            routing_mode: AgentRoutingMode::Pinned,
            account_id: Some(account_id.into()),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.trim().is_empty() {
            return Err("agent route model is required");
        }
        match (self.routing_mode, self.account_id.as_deref()) {
            (AgentRoutingMode::Automatic, None) => Ok(()),
            (AgentRoutingMode::Automatic, Some(_)) => {
                Err("automatic agent route cannot select an account")
            }
            (AgentRoutingMode::Pinned, Some(account_id)) if !account_id.trim().is_empty() => Ok(()),
            (AgentRoutingMode::Pinned, _) => Err("pinned agent route requires an account id"),
        }
    }
}

/// Owner-scoped inference attribution. It intentionally contains no owner
/// identity, grant token, provider credential, or credential hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRouteReceipt {
    pub logical_session_id: String,
    pub routing_mode: AgentRoutingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_account_id: Option<String>,
    pub route: AgentRouteAttribution,
    pub grant: AgentGrantAttribution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRouteAttribution {
    pub provider: AgentProvider,
    pub model: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_generation: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGrantAttribution {
    pub id: String,
    pub provider: AgentProvider,
    pub model: String,
    pub harness: String,
    pub source: String,
    pub lifecycle_epoch: u64,
    pub environment: String,
    pub routing_mode: AgentRoutingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_account_id: Option<String>,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_generation: Option<u64>,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

/// Lifecycle reported by Scaffold's code-sandbox API. This intentionally has no
/// catch-all variant: an unknown control-plane status must fail decoding rather
/// than being presented as a known safe transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldLifecycle {
    Creating,
    RestoringSnapshot,
    Starting,
    Ready,
    AgentRunning,
    Paused,
    Resuming,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldDatabaseEnvironment {
    #[default]
    Local,
    StagingSnapshot,
    ProductionSnapshot,
}

impl ScaffoldDatabaseEnvironment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::StagingSnapshot => "staging_snapshot",
            Self::ProductionSnapshot => "production_snapshot",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldEnvironmentLinks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

/// Runtime location for an existing session projection. A Scaffold environment
/// enriches the same [`AgentSessionRecord`]; it is not a parallel session model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionEnvironmentSource {
    Local,
    Scaffold {
        sandbox_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        lifecycle: ScaffoldLifecycle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle_epoch: Option<u64>,
        #[serde(default)]
        links: Box<ScaffoldEnvironmentLinks>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEnvironment {
    pub source: SessionEnvironmentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Control-plane-verified owner subject. Display email is not an authority key.
    pub owner_principal: String,
    pub scope: CollaborationScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_environment: Option<ScaffoldDatabaseEnvironment>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldEnvironmentSnapshot {
    #[serde(default)]
    pub environments: Vec<SessionEnvironment>,
    pub refreshed_at: i64,
}

/// Typed environment control accepted by the native RPC surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
pub enum ScaffoldEnvironmentControl {
    Inspect {
        sandbox_id: String,
        scope: CollaborationScope,
    },
    Create {
        scope: CollaborationScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(default)]
        database_environment: ScaffoldDatabaseEnvironment,
        #[serde(rename = "agentRoute")]
        agent_route: AgentRoute,
    },
    Pause {
        sandbox_id: String,
        scope: CollaborationScope,
    },
    Resume {
        sandbox_id: String,
        scope: CollaborationScope,
    },
    Stop {
        sandbox_id: String,
        scope: CollaborationScope,
    },
    Attach {
        sandbox_id: String,
        scope: CollaborationScope,
    },
    /// Copy the exact native OMP session backing a local Crew chat into a
    /// Scaffold host. This only materializes and verifies the native store; the
    /// first remote run performs ACP `session/load` through its ordinary
    /// `RunRequest.resume` path.
    HandoffOmpSession {
        #[serde(rename = "sandboxId")]
        sandbox_id: String,
        scope: CollaborationScope,
        #[serde(rename = "localChatId")]
        local_chat_id: String,
    },
}

/// Exact physical SessionRoom selected by a Scaffold attach. The local controller
/// must carry this trusted result into its document watch; ordinary local watches
/// omit it and remain on the legacy project/session namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRoomProjection {
    pub project_id: String,
    pub deployment_id: String,
    pub session_id: String,
}

/// Non-secret authority metadata returned by a successful Scaffold attach.
/// The opaque device bootstrap credential is consumed only by the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldControlGrant {
    pub id: String,
    pub expires_at: i64,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldEnvironmentControlResult {
    pub environment: SessionEnvironment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_projection: Option<SessionRoomProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_grant: Option<ScaffoldControlGrant>,
    /// Native OMP id to put on the first remote `RunRequest.resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_native_session_id: Option<String>,
    /// Sandbox working directory to use for that first remote run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_cwd: Option<String>,
}

/// One independently-owned agent session contributing to a shared thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecord {
    pub session_id: String,
    pub chat_id: String,
    pub owner_subject: String,
    pub owner_device_id: String,
    pub source: AgentSessionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<SessionEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<crate::HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::SessionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    pub created_at: i64,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

/// Additive provenance for a legacy transcript entry. Keeping this append-only avoids
/// rewriting streamed message maps while allowing multiple sessions to merge in one thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageProvenance {
    pub message_id: String,
    pub session_id: String,
    pub author_subject: String,
    pub owner_device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub source: AgentSessionSource,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnchorTargetKind {
    Message,
    ToolCall,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Utf8ByteRange {
    pub start: u32,
    pub end: u32,
}

/// Stable file identity; never contains a signed URL, bearer, or application DB key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FileTargetReference {
    LocalWorkspacePath {
        #[serde(rename = "workspaceId")]
        workspace_id: String,
        #[serde(rename = "relativePath")]
        relative_path: String,
        #[serde(flatten, default)]
        unknown: UnknownFields,
    },
    ScaffoldArtifact {
        #[serde(rename = "artifactId")]
        artifact_id: String,
        /// Optional stable `ashler://artifact/...` or `scaffold://artifact/...` URI.
        #[serde(
            rename = "artifactUri",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        artifact_uri: Option<String>,
        #[serde(flatten, default)]
        unknown: UnknownFields,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticAnchor {
    pub target_kind: AnchorTargetKind,
    pub target_id: String,
    /// Required for `targetKind: file`; absent for message/tool anchors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileTargetReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<Utf8ByteRange>,
    /// Bounded selected text used for deterministic fallback search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix_hash: Option<String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnnotationState {
    Anchored,
    Reanchored,
    Orphaned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticAnnotation {
    pub id: String,
    pub author_subject: String,
    pub body: String,
    pub anchor: SemanticAnchor,
    pub state: AnnotationState,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentMetadata {
    pub id: String,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Opaque existing R2/blob seam key. Content bytes never enter a Loro document.
    pub blob_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelHandoff {
    pub id: String,
    pub from_model: String,
    pub to_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_id: Option<String>,
    /// Last publication known to the outgoing model. New publications append after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_publication_id: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditResult {
    Applied,
    Rejected,
    Expired,
    Superseded,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: String,
    pub actor_subject: String,
    pub actor_device_id: String,
    pub target_id: String,
    pub action: String,
    pub occurred_at: i64,
    pub result: AuditResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

/// Immutable records in publication order. Unknown future kinds are retained verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum PublicationValue {
    Annotation(SemanticAnnotation),
    Attachment(AttachmentMetadata),
    Audit(AuditEvent),
    AgentSession(Box<AgentSessionRecord>),
    MessageProvenance(MessageProvenance),
    ModelHandoff(ModelHandoff),
    Unknown {
        kind: String,
        value: serde_json::Value,
    },
}

impl Serialize for PublicationValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (kind, value) = match self {
            Self::Annotation(value) => ("annotation", serde_json::to_value(value)),
            Self::Attachment(value) => ("attachment", serde_json::to_value(value)),
            Self::Audit(value) => ("audit", serde_json::to_value(value)),
            Self::AgentSession(value) => ("agentSession", serde_json::to_value(value)),
            Self::MessageProvenance(value) => ("messageProvenance", serde_json::to_value(value)),
            Self::ModelHandoff(value) => ("modelHandoff", serde_json::to_value(value)),
            Self::Unknown { kind, value } => (kind.as_str(), Ok(value.clone())),
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("kind", kind)?;
        map.serialize_entry(
            "value",
            &value.map_err(<S::Error as serde::ser::Error>::custom)?,
        )?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for PublicationValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            kind: String,
            value: serde_json::Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        match wire.kind.as_str() {
            "annotation" => serde_json::from_value::<SemanticAnnotation>(wire.value)
                .map(Self::Annotation)
                .map_err(<D::Error as serde::de::Error>::custom),
            "attachment" => serde_json::from_value::<AttachmentMetadata>(wire.value)
                .map(Self::Attachment)
                .map_err(<D::Error as serde::de::Error>::custom),
            "audit" => serde_json::from_value::<AuditEvent>(wire.value)
                .map(Self::Audit)
                .map_err(<D::Error as serde::de::Error>::custom),
            "agentSession" => serde_json::from_value::<AgentSessionRecord>(wire.value)
                .map(Box::new)
                .map(Self::AgentSession)
                .map_err(<D::Error as serde::de::Error>::custom),
            "messageProvenance" => serde_json::from_value::<MessageProvenance>(wire.value)
                .map(Self::MessageProvenance)
                .map_err(<D::Error as serde::de::Error>::custom),
            "modelHandoff" => serde_json::from_value::<ModelHandoff>(wire.value)
                .map(Self::ModelHandoff)
                .map_err(<D::Error as serde::de::Error>::custom),
            _ => Ok(Self::Unknown {
                kind: wire.kind.clone(),
                value: wire.value,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationRecord {
    pub id: String,
    pub schema_version: u32,
    pub published_at: i64,
    pub published_by: String,
    #[serde(flatten)]
    pub value: PublicationValue,
    #[serde(flatten, default)]
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollaborationSnapshot {
    pub schema_version: u32,
    #[serde(default)]
    pub sessions: Vec<AgentSessionRecord>,
    #[serde(default)]
    pub message_provenance: Vec<MessageProvenance>,
    #[serde(default)]
    pub publications: Vec<PublicationRecord>,
    #[serde(default)]
    pub participants: Vec<ParticipantPresence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<CollaborationPrincipal>,
    #[serde(default)]
    pub grants: Vec<CapabilityGrant>,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentProvider, AgentRoute, CollaborationScope, CometInvitation, ScaffoldEnvironmentControl,
        parse_scaffold_device_id,
    };

    #[test]
    fn omp_handoff_control_uses_additive_camel_case_wire_shape() {
        let control = ScaffoldEnvironmentControl::HandoffOmpSession {
            sandbox_id: "sandbox-a".into(),
            scope: CollaborationScope {
                project_id: "project-a".into(),
                deployment_id: Some("deployment-a".into()),
                session_id: Some("session-a".into()),
                unknown: Default::default(),
            },
            local_chat_id: "local-chat-a".into(),
        };
        let value = serde_json::to_value(control).unwrap();
        assert_eq!(value["operation"], "handoffOmpSession");
        assert_eq!(value["sandboxId"], "sandbox-a");
        assert_eq!(value["localChatId"], "local-chat-a");
        assert_eq!(value["scope"]["sessionId"], "session-a");
    }

    #[test]
    fn agent_routes_use_the_shared_camel_case_wire_shape() {
        let automatic =
            serde_json::to_value(AgentRoute::automatic(AgentProvider::OpenAi, "gpt-5.6-sol"))
                .unwrap();
        assert_eq!(
            automatic,
            serde_json::json!({
                "provider": "openai",
                "model": "gpt-5.6-sol",
                "fallback": "disabled",
                "routingMode": "automatic",
            })
        );

        let pinned = AgentRoute::pinned(
            AgentProvider::Anthropic,
            "claude-opus-5",
            "opaque-account-id",
        );
        assert_eq!(
            serde_json::to_value(&pinned).unwrap(),
            serde_json::json!({
                "provider": "anthropic",
                "model": "claude-opus-5",
                "fallback": "disabled",
                "routingMode": "pinned",
                "accountId": "opaque-account-id",
            })
        );
        assert!(pinned.validate().is_ok());
        assert!(
            AgentRoute::pinned(AgentProvider::OpenAi, "gpt-5.6-sol", " ")
                .validate()
                .is_err()
        );
        let mut invalid_automatic = AgentRoute::automatic(AgentProvider::OpenAi, "gpt-5.6-sol");
        invalid_automatic.account_id = Some("opaque-account-id".into());
        assert!(invalid_automatic.validate().is_err());
    }

    #[test]
    fn invitation_deep_link_round_trips_exact_route() {
        let invitation =
            CometInvitation::new("chat-a", "session-b", "grant_c").expect("valid invitation");
        let link = invitation.deep_link();
        assert_eq!(link, "comet://invite/chat-a/session-b/grant_c");
        assert_eq!(CometInvitation::parse_deep_link(&link), Some(invitation));
    }

    #[test]
    fn invitation_rejects_ambiguous_or_uninstalled_routes() {
        assert!(CometInvitation::parse_deep_link("https://edge/rooms/chat-a").is_none());
        assert!(
            CometInvitation::parse_deep_link("comet://invite/chat-a/session-b/grant-c/extra")
                .is_none()
        );
        assert!(
            CometInvitation::parse_deep_link("comet://invite/chat-a/session-b/grant%2Fc").is_none()
        );
    }

    #[test]
    fn scaffold_device_identity_requires_lifecycle_epoch() {
        assert_eq!(
            parse_scaffold_device_id("comet-scaffold-sandbox-a-e12"),
            Some(("sandbox-a", 12))
        );
        assert_eq!(
            parse_scaffold_device_id("comet-scaffold-sandbox-e2-a-e3"),
            Some(("sandbox-e2-a", 3))
        );
        assert_eq!(parse_scaffold_device_id("comet-scaffold-sandbox-a"), None);
        assert_eq!(
            parse_scaffold_device_id("comet-scaffold-sandbox-a-e0"),
            None
        );
    }
}
