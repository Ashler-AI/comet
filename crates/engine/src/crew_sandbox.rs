//! Crew: a Scaffold sandbox host (docs/crew/zgx-thin-port.md, "Scaffold
//! sandboxes"), ported from Crew's (Ashler-AI/comet `crates/engine/src/rpc/crew.rs`,
//! `lib.rs` and `apps/zeron/src/crew_cli.rs`).
//!
//! A sandbox host is an engine launched with a device grant
//! ([`DeviceLaunch`], from `headless --device-bootstrap-file`), or restarted
//! with the durable credential that grant became (`scaffold-device-session.json`).
//! It runs as the grant's device, `comet-scaffold-{sandbox}-e{epoch}`, for one
//! session of one owner. Here:
//!
//! - [`consume_bootstrap`]: the one-use bootstrap file goes once its grant is
//!   this engine's verified authority, never before.
//! - [`admit`], `rpc.rs`'s one seam: `ScaffoldHostAuthority` (the desktop's
//!   attach probe, over local IPC only), and what a host serves and refuses.
//!   Calls name its own session or none; uploads need `session.files`; this
//!   device's credentials and agent installs are never managed from here; a
//!   relayed call carries the Edge's verified `__crewAuthority`, which must be
//!   this host's owner, project and deployment (and, scoped, its session).
//! - [`install`]: OMP runs as Scaffold's host (its `scaffold-host` profile).
//! - [`inference_allowed`]: managed inference, on the device's own token, only
//!   for its session and the harnesses Agent Auth serves.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use zeron_proto::HarnessId;
use zeron_rpc::{Caller, RpcError, RpcReply, methods};
use zeron_sync::scaffold_auth::DeviceConfig;

use crate::auth::{Auth, ScaffoldScope};
use crate::registry::HarnessRegistry;

/// The desktop's attach probe: this host's verified grant (`DeviceGrantAuthority`).
pub const SCAFFOLD_HOST_AUTHORITY: &str = "ScaffoldHostAuthority";

/// A sandbox host's durable device credential in its data dir (0600): what
/// Scaffold's native runner checks before a restart.
pub const DEVICE_SESSION_FILE: &str = "scaffold-device-session.json";

/// The capability a host's uploads need.
const SESSION_FILES: &str = "session.files";

/// What a sandbox host never manages: this device's own credentials and
/// agent installs (Crew's list, and upstream's newer ones of the same kind).
const LOCAL_MANAGEMENT: &[&str] = &[
    methods::LIST_AGENT_ACCOUNTS,
    methods::ACTIVATE_AGENT_ACCOUNT,
    methods::FORGET_AGENT_ACCOUNT,
    methods::ADD_AGENT_API_KEY,
    methods::RENAME_AGENT_ACCOUNT,
    methods::START_AGENT_LOGIN,
    methods::RELAY_AGENT_LOGIN_CALLBACK,
    methods::COMPLETE_AGENT_LOGIN,
    methods::POLL_AGENT_LOGIN,
    methods::CANCEL_AGENT_LOGIN,
    methods::CONSUME_CODEX_RESET_CREDIT,
    methods::VAULT_PUT_API_KEY,
    methods::INSTALL_HARNESS,
    methods::APPLY_HARNESS_UPDATE,
    methods::ADD_ACP_AGENT,
    methods::REMOVE_ACP_AGENT,
    methods::ADD_OWN_AGENT,
    methods::SET_ACP_AGENT_ENV,
    methods::SET_ACP_AGENT_KEY,
    methods::SIGN_OUT_ACP_AGENT,
];

/// A sandbox host's launch: the grant its bootstrap carried, and that file.
#[derive(Debug, Clone)]
pub struct DeviceLaunch {
    pub grant: DeviceConfig,
    /// The private, one-use bootstrap the grant was read from.
    pub bootstrap_file: Option<PathBuf>,
}

impl From<DeviceConfig> for DeviceLaunch {
    fn from(grant: DeviceConfig) -> Self {
        Self { grant, bootstrap_file: None }
    }
}

/// Remove the bootstrap once its grant is this engine's verified authority
/// (exchanged now, or restored from the credential it became). An enrollment
/// that wasn't verified stays where it is.
pub fn consume_bootstrap(auth: &Auth, launch: Option<&DeviceLaunch>) {
    let Some(launch) = launch else { return };
    let Some(path) = &launch.bootstrap_file else { return };
    let Some(authority) = auth.device_grant_authority() else { return };
    let grant_id = launch.grant.join_grant.split('.').nth(1);
    if grant_id != Some(authority.grant_id.as_str()) {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!("crew: the device bootstrap is consumed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "crew: could not remove the consumed device bootstrap"),
    }
}

/// A sandbox host runs OMP as Scaffold's host. Before any run can start.
pub(crate) fn install(auth: &Auth, registry: &HarnessRegistry) {
    if auth.device_mode() {
        registry.register(Arc::new(zeron_harness::OmpHarness::scaffold_host()));
    }
}

/// Whether a sandbox host may run `harness` for `chat` on Agent Auth: only its
/// own session, and only what Agent Auth serves (a title has no chat: none).
pub(crate) fn inference_allowed(auth: &Auth, chat: Option<&str>, harness: HarnessId) -> Result<(), String> {
    let scope = auth.verified_scope();
    let bound = scope.as_ref().and_then(|scope| scope.session_id.as_deref());
    if chat.is_none() || chat != bound || !matches!(harness, HarnessId::Omp | HarnessId::ClaudeCode | HarnessId::Codex) {
        return Err("Sandbox inference is bound to its managed session".into());
    }
    Ok(())
}

/// `rpc.rs`'s seam: answer the attach probe, or refuse what a sandbox host
/// doesn't serve; else the call goes on (a relayed one without the Edge's
/// envelope, once checked).
pub(crate) fn admit(
    auth: Option<&Auth>,
    own_device: &str,
    method: &str,
    mut params: Value,
) -> ControlFlow<Result<RpcReply, RpcError>, Value> {
    if method == SCAFFOLD_HOST_AUTHORITY {
        return ControlFlow::Break(host_authority(auth, own_device, &zeron_rpc::caller(), &params));
    }
    let Some(auth) = auth.filter(|auth| auth.device_mode()) else {
        return ControlFlow::Continue(params);
    };
    let host = Host::of(auth, own_device);
    match guard(host.as_ref(), &zeron_rpc::caller(), method, &mut params) {
        Ok(()) => ControlFlow::Continue(params),
        Err(error) => ControlFlow::Break(Err(error)),
    }
}

fn denied(message: &str) -> RpcError {
    RpcError::Failed(message.into())
}

/// The attach probe: local IPC on this device only, and only a verified,
/// unexpired grant.
fn host_authority(auth: Option<&Auth>, own_device: &str, caller: &Caller, params: &Value) -> Result<RpcReply, RpcError> {
    if !caller.is_local() {
        return Err(denied("Device bootstrap probes require local IPC"));
    }
    if params
        .get("targetDeviceId")
        .and_then(Value::as_str)
        .is_some_and(|device| device != own_device)
    {
        return Err(denied("This operation requires local IPC on the explicitly selected device"));
    }
    let authority = auth
        .and_then(Auth::device_grant_authority)
        .ok_or_else(|| denied("Verified sandbox authority unavailable"))?;
    RpcReply::value(&authority)
}

/// What a sandbox host is: its verified scope and owner, and its device.
struct Host<'a> {
    scope: ScaffoldScope,
    owner: String,
    capabilities: Vec<String>,
    device: &'a str,
}

impl<'a> Host<'a> {
    /// `None` until the grant is verified (and after it's revoked).
    fn of(auth: &Auth, device: &'a str) -> Option<Self> {
        let session = auth.verified_scaffold_session()?;
        Some(Self {
            scope: auth.verified_scope()?,
            owner: auth.user_id()?,
            capabilities: session.identity.capabilities,
            device,
        })
    }
}

/// The Edge's envelope on a relayed call (`edge/src/crew/authority.ts`
/// `stampRpcAuthority`): who the relay verified, never what a caller claimed.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayAuthority {
    subject: String,
    project_id: String,
    deployment_id: Option<String>,
    session_id: Option<String>,
    device_id: Option<String>,
    grant_id: Option<String>,
    expires_at: Option<i64>,
    peer: Option<PeerAuthority>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerAuthority {
    source_session_id: String,
    source_device_id: String,
    target_session_id: String,
    target_device_id: String,
}

/// Crew's sandbox-host guard (Crew's `crew_guard`, its device-mode part and
/// its relay checks).
fn guard(host: Option<&Host>, caller: &Caller, method: &str, params: &mut Value) -> Result<(), RpcError> {
    let relayed = matches!(caller, Caller::Peer { .. });
    if !relayed && params.get("__crewAuthority").is_some() {
        return Err(denied("Relay authority cannot be supplied over local IPC"));
    }
    let host = host.ok_or_else(|| denied("Sandbox authority unavailable"))?;
    if relayed {
        relay(host, method, params)?;
    }
    let chat_id = params.get("chatId").and_then(Value::as_str);
    if chat_id.is_some_and(|chat| Some(chat) != host.scope.session_id.as_deref()) {
        return Err(denied("Sandbox host is bound to a different session"));
    }
    if matches!(method, methods::UPLOAD_CHUNK | methods::UPLOAD_COMMIT)
        && (chat_id.is_none() || !host.capabilities.iter().any(|capability| capability == SESSION_FILES))
    {
        return Err(denied("Sandbox file access requires its exact session and session.files authority"));
    }
    if LOCAL_MANAGEMENT.contains(&method) {
        return Err(denied("Sandbox hosts cannot manage local credentials or agent installations"));
    }
    Ok(())
}

/// A relayed call: the Edge's envelope is checked against this host, then
/// taken off, so no handler reads a parameter it doesn't know.
fn relay(host: &Host, method: &str, params: &mut Value) -> Result<(), RpcError> {
    let envelope = params
        .as_object_mut()
        .and_then(|params| params.remove("__crewAuthority"))
        .ok_or_else(|| denied("Missing verified relay authority"))?;
    let authority: RelayAuthority =
        serde_json::from_value(envelope).map_err(|_| denied("Invalid verified relay authority"))?;
    if authority.project_id != host.scope.project_id || authority.deployment_id != host.scope.deployment_id {
        return Err(denied("Relay authority belongs to another deployment"));
    }
    if authority.subject != host.owner {
        return Err(denied("Relay authority belongs to another account"));
    }
    let chat_id = params.get("chatId").and_then(Value::as_str);
    if let Some(peer) = &authority.peer
        && (!matches!(method, "DeliverPeerMessage" | methods::LOCAL_DEVICE)
            || peer.target_device_id != host.device
            || (method == "DeliverPeerMessage" && chat_id != Some(peer.target_session_id.as_str())))
    {
        return Err(denied("Peer transport cannot dispatch general engine operations"));
    }
    if let Some(session) = &authority.session_id {
        let bound = match &authority.peer {
            Some(peer) => {
                &peer.source_session_id == session && authority.device_id.as_deref() == Some(peer.source_device_id.as_str())
            }
            None => authority.device_id.as_deref() == Some(host.device) && chat_id.is_none_or(|chat| chat == session),
        };
        if !bound
            || authority.expires_at.is_none_or(|expires| expires <= crate::now_ms())
            || authority.grant_id.as_deref().is_none_or(str::is_empty)
        {
            return Err(denied("Scoped relay authority is expired or belongs to another host/session"));
        }
        if params
            .get("targetDeviceId")
            .and_then(Value::as_str)
            .is_some_and(|device| device != host.device)
        {
            return Err(denied("A scoped relay cannot delegate to another device"));
        }
    }
    if crate::rpc::AuthRpc::handles(method) && method != methods::AUTH_STATUS {
        return Err(denied("Crew authentication changes require local consent"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const DEVICE: &str = "comet-scaffold-sbx-e3";

    fn host() -> Host<'static> {
        Host {
            scope: ScaffoldScope {
                project_id: "project".into(),
                deployment_id: Some("deployment".into()),
                session_id: Some("session".into()),
            },
            owner: "owner@example.com".into(),
            capabilities: ["session.read", "session.chat", "session.files"].map(String::from).to_vec(),
            device: DEVICE,
        }
    }

    fn local() -> Caller {
        Caller::Local
    }

    fn relayed() -> Caller {
        Caller::Peer { device: None }
    }

    fn owner_envelope() -> Value {
        json!({"subject": "owner@example.com", "projectId": "project", "deploymentId": "deployment",
            "capabilities": ["session.read"]})
    }

    fn check(caller: Caller, method: &str, params: Value) -> Result<Value, String> {
        let mut params = params;
        guard(Some(&host()), &caller, method, &mut params).map_err(|error| error.to_string())?;
        Ok(params)
    }

    /// Crew's device-mode guard: its own session or none; uploads with
    /// `session.files`; no credential or agent management, however asked.
    #[test]
    fn a_sandbox_host_serves_its_own_session_and_nothing_of_this_device_s() {
        for (method, params) in [
            (methods::WATCH_DOC_MESSAGES, json!({"chatId": "session"})),
            (methods::QUEUE_COMMAND, json!({"chatId": "session", "command": {"kind": "run"}})),
            (methods::WATCH_CHATS, json!({})),
            (methods::LIST_MODELS, json!({"harness": "omp"})),
            (methods::UPLOAD_CHUNK, json!({"chatId": "session", "uploadId": "u", "seq": 0, "data": ""})),
        ] {
            assert!(check(local(), method, params.clone()).is_ok(), "{method}");
        }
        let other = check(local(), methods::WATCH_DOC_MESSAGES, json!({"chatId": "other"}));
        assert_eq!(other.unwrap_err(), "Sandbox host is bound to a different session");
        assert!(check(local(), methods::UPLOAD_COMMIT, json!({"uploadId": "u"})).is_err(), "an upload names its session");
        let mut no_files = host();
        no_files.capabilities.retain(|capability| capability != SESSION_FILES);
        assert!(guard(Some(&no_files), &local(), methods::UPLOAD_CHUNK, &mut json!({"chatId": "session"})).is_err());
        for method in LOCAL_MANAGEMENT {
            assert_eq!(
                check(local(), method, json!({})).unwrap_err(),
                "Sandbox hosts cannot manage local credentials or agent installations",
                "{method}"
            );
        }
        // Not verified (yet, or any more): nothing is served.
        assert!(guard(None, &local(), methods::WATCH_CHATS, &mut json!({})).is_err());
    }

    /// A relayed call carries the Edge's envelope, which must be this host's
    /// owner, project and deployment; it's taken off before any handler.
    #[test]
    fn a_relayed_call_is_the_owner_s_and_loses_its_envelope() {
        let served = check(relayed(), methods::WATCH_DOC_MESSAGES, json!({"chatId": "session", "__crewAuthority": owner_envelope()}))
            .unwrap();
        assert_eq!(served, json!({"chatId": "session"}));
        assert_eq!(
            check(relayed(), methods::WATCH_CHATS, json!({})).unwrap_err(),
            "Missing verified relay authority"
        );
        for (field, value, why) in [
            ("subject", json!("someone@example.com"), "Relay authority belongs to another account"),
            ("projectId", json!("other"), "Relay authority belongs to another deployment"),
            ("deploymentId", json!("other"), "Relay authority belongs to another deployment"),
            ("deploymentId", json!(null), "Relay authority belongs to another deployment"),
        ] {
            let mut envelope = owner_envelope();
            envelope[field] = value;
            assert_eq!(check(relayed(), methods::WATCH_CHATS, json!({"__crewAuthority": envelope})).unwrap_err(), why, "{field}");
        }
        // Only the relay supplies it.
        assert_eq!(
            check(local(), methods::WATCH_CHATS, json!({"__crewAuthority": owner_envelope()})).unwrap_err(),
            "Relay authority cannot be supplied over local IPC"
        );
        // Sign-in changes are this host's alone; its status isn't.
        let envelope = json!({"__crewAuthority": owner_envelope()});
        assert!(check(relayed(), methods::SIGN_OUT, envelope.clone()).is_err());
        assert!(check(relayed(), methods::AUTH_STATUS, envelope).is_ok());
    }

    /// A session-scoped credential (a viewer, crew-web's) reaches only this
    /// host's session, while its grant lasts.
    #[test]
    fn a_scoped_relay_is_bound_to_this_host_and_its_session() {
        let scoped = |patch: Value| {
            let mut envelope = owner_envelope();
            envelope["sessionId"] = json!("session");
            envelope["deviceId"] = json!(DEVICE);
            envelope["grantId"] = json!("grant");
            envelope["expiresAt"] = json!(crate::now_ms() + 60_000);
            for (key, value) in patch.as_object().unwrap() {
                envelope[key] = value.clone();
            }
            envelope
        };
        let call = |envelope: Value, chat: &str| {
            check(relayed(), methods::WATCH_DOC_MESSAGES, json!({"chatId": chat, "__crewAuthority": envelope}))
        };
        assert!(call(scoped(json!({})), "session").is_ok());
        for patch in [
            json!({"deviceId": "comet-scaffold-other-e1"}),
            json!({"expiresAt": crate::now_ms() - 1}),
            json!({"grantId": ""}),
        ] {
            assert!(call(scoped(patch.clone()), "session").is_err(), "{patch}");
        }
        let delegated = check(
            relayed(),
            methods::LIST_MODELS,
            json!({"targetDeviceId": "elsewhere", "__crewAuthority": scoped(json!({}))}),
        );
        assert_eq!(delegated.unwrap_err(), "A scoped relay cannot delegate to another device");
        // A peer's transport carries peer messages and nothing else.
        let peer = json!({"peer": {"sourceSessionId": "session", "sourceDeviceId": DEVICE,
            "targetSessionId": "session", "targetDeviceId": DEVICE}});
        assert_eq!(
            call(scoped(peer), "session").unwrap_err(),
            "Peer transport cannot dispatch general engine operations"
        );
    }

    /// The probe answers local IPC only, on this device; a non-host has no
    /// authority to give.
    #[test]
    fn the_attach_probe_is_local_and_this_device_s() {
        let refused = |caller: Caller, params: Value| host_authority(None, DEVICE, &caller, &params).err().unwrap().to_string();
        assert_eq!(refused(relayed(), json!({})), "Device bootstrap probes require local IPC");
        assert_eq!(refused(Caller::Agent { chat: None }, json!({})), "Device bootstrap probes require local IPC");
        assert_eq!(
            refused(local(), json!({"targetDeviceId": "elsewhere"})),
            "This operation requires local IPC on the explicitly selected device"
        );
        assert_eq!(refused(local(), json!({})), "Verified sandbox authority unavailable");
    }

    #[test]
    fn inference_is_the_host_s_session_s_alone() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(crate::auth::AuthConfig::new("http://127.0.0.1:9", dir.path()));
        // Not a verified host: nothing runs.
        assert!(inference_allowed(&auth, Some("session"), HarnessId::Omp).is_err());
        assert!(inference_allowed(&auth, None, HarnessId::Omp).is_err());
    }

    const GRANT: &str = "0123456789abcdef0123456789abcdef";
    const REJECTED: &str = "ffffffffffffffffffffffffffffffff";

    /// A Scaffold authority and its Edge on one loopback origin: the device
    /// grant exchange (a grant `REJECTED` names is refused, 401) and Agent
    /// Auth's authority, whose bearers it keeps.
    async fn fake_scaffold() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let bearers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bearers.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 8192];
                    // Headers, then the body its length names.
                    loop {
                        let Ok(read) = stream.read(&mut buffer).await else { return };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&request).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned()))
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    let bearer = text
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().starts_with("authorization:").then(|| line[14..].trim().to_owned()))
                        .unwrap_or_default();
                    let (status, body) = if text.starts_with("POST /auth/device-grants/exchange ") {
                        if text.contains(REJECTED) {
                            ("401 Unauthorized", json!({"error": "grant_consumed"}))
                        } else {
                            ("200 OK", json!({
                                "accessToken": format!("cs1.{GRANT}.rotating"),
                                "expiresAt": crate::now_ms() + 12 * 3_600_000,
                                "userId": "owner@example.com", "email": "owner@example.com",
                                "projectId": "project", "deploymentId": "deployment", "sandboxId": "sbx",
                                "targetDeviceId": DEVICE, "sessionId": "session", "lifecycleEpoch": 3,
                                "capabilities": ["session.read", "session.sync.write", "session.chat",
                                    "session.control", "session.annotate", "session.files", "session.environment"],
                            }))
                        }
                    } else if text.starts_with("POST /api/agent-auth/v2/authority ") {
                        seen.lock().unwrap().push(bearer);
                        ("201 Created", json!({
                            "contractVersion": 2, "token": "agent-auth-authority", "tokenType": "Bearer",
                            "authorityId": "authority-1", "principalId": "principal", "authorityScope": "scope",
                            "expiresAt": (chrono::Utc::now() + chrono::TimeDelta::minutes(5)).to_rfc3339(),
                        }))
                    } else {
                        ("404 Not Found", json!({}))
                    };
                    let body = body.to_string();
                    let _ = stream
                        .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                        .await;
                });
            }
        });
        (origin, bearers)
    }

    fn device_config(dir: &std::path::Path, origin: &str, grant: &str, bootstrap: &std::path::Path) -> crate::EngineConfig {
        let mut scaffold = zeron_sync::scaffold_auth::ScaffoldConfig::new(origin, origin, "project");
        scaffold.deployment_id = Some("deployment".into());
        crate::EngineConfig {
            data_dir: dir.to_path_buf(),
            edge_url: origin.into(),
            edge_token: None,
            ipc_port: 0,
            default_harness: HarnessId::Omp,
            org_id: None,
            workos_client_id: None,
            scaffold: Some(scaffold),
            device: Some(DeviceLaunch {
                grant: DeviceConfig {
                    join_grant: format!("cg1.{grant}.enrollment-secret"),
                    device_id: DEVICE.into(),
                    session_id: "session".into(),
                    deployment_id: "deployment".into(),
                    sandbox_id: "sbx".into(),
                    lifecycle_epoch: 3,
                },
                bootstrap_file: Some(bootstrap.to_path_buf()),
            }),
        }
    }

    /// A host's start, as the engine does it: the grant is exchanged at the
    /// Edge, its bootstrap is gone, the durable credential is what Scaffold's
    /// runner restarts from, the probe answers the grant, and managed
    /// inference asks Agent Auth with the device's own token, for its session
    /// alone.
    #[tokio::test]
    async fn a_host_enrolls_consumes_its_bootstrap_and_runs_on_its_device_token() {
        use crate::registry::RunEnvironment as _;

        let (origin, bearers) = fake_scaffold().await;
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = dir.path().join("enrollment.json");
        std::fs::write(&bootstrap, b"{}").unwrap();
        let config = device_config(&dir.path().join("crew-data"), &origin, GRANT, &bootstrap);
        let auth = crate::Engine::build_auth(&config).await;
        assert!(auth.device_mode() && auth.state().is_signed_in());
        assert!(!bootstrap.exists(), "a verified grant's bootstrap is consumed");

        // native-crew-runner.mjs validateDeviceCredential(), field for field.
        let path = dir.path().join("crew-data").join(DEVICE_SESSION_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_file() && metadata.nlink() == 1);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        let credential: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let token = credential["accessToken"].as_str().unwrap();
        assert_eq!(token.split('.').collect::<Vec<_>>(), ["cs1", GRANT, "rotating"]);
        assert_eq!(credential["device"]["grantId"], GRANT);
        assert!(credential["device"]["expiresAt"].is_i64());
        assert_eq!(credential["identity"]["userId"], "owner@example.com");
        assert_eq!(credential["identity"]["projectScope"], "project");
        assert_eq!(credential["identity"]["deploymentId"], "deployment");
        assert_eq!(credential["resource"], origin.as_str());
        assert_eq!(credential["edgeOrigin"], origin.as_str());
        for (key, value) in [("deviceId", json!(DEVICE)), ("sessionId", json!("session")),
            ("deploymentId", json!("deployment")), ("sandboxId", json!("sbx")), ("lifecycleEpoch", json!(3))]
        {
            assert_eq!(credential["device"][key], value, "{key}");
        }

        let RpcReply::Value(authority) = host_authority(Some(&auth), DEVICE, &Caller::Local, &json!({})).unwrap() else {
            panic!("the probe answers a value");
        };
        assert_eq!(authority["grantId"], GRANT);
        assert_eq!(authority["principalSubject"], "owner@example.com");
        assert_eq!(authority["scope"], json!({"projectId": "project", "deploymentId": "deployment", "sessionId": "session"}));
        assert_eq!((authority["sandboxId"].clone(), authority["deviceId"].clone()), (json!("sbx"), json!(DEVICE)));
        assert_eq!(authority["lifecycleEpoch"], 3);
        assert_eq!(authority["capabilities"].as_array().unwrap().len(), 7);

        let crew = crate::crew_inference::CrewInference::for_engine(&config, &auth, auth.for_profile_tokens()).unwrap();
        let run = || -> zeron_proto::RunRequest {
            serde_json::from_value(json!({"prompt": "p", "model": "claude-opus-5-5", "reasoning": null, "cwd": "/tmp",
                "sandbox": "workspace-write", "resume": null}))
            .unwrap()
        };
        crew.prepare(HarnessId::ClaudeCode, Some("session"), &mut run()).await.unwrap();
        assert_eq!(*bearers.lock().unwrap(), [format!("Bearer {token}")], "Agent Auth sees the device's own token");
        for (harness, chat) in [(HarnessId::ClaudeCode, Some("other")), (HarnessId::ClaudeCode, None), (HarnessId::Omp, Some("other"))] {
            let refused = crew.prepare(harness, chat, &mut run()).await.unwrap_err();
            assert!(refused.contains("bound to its managed session"), "{chat:?}: {refused}");
        }
        crew.shutdown();

        // A grant Scaffold refuses: no host, and its bootstrap stays.
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = dir.path().join("enrollment.json");
        std::fs::write(&bootstrap, b"{}").unwrap();
        let refused = crate::Engine::build_auth(&device_config(&dir.path().join("crew-data"), &origin, REJECTED, &bootstrap)).await;
        assert!(refused.device_mode() && refused.device_grant_authority().is_none());
        assert!(bootstrap.exists(), "an unverified enrollment is never removed");
    }
}
