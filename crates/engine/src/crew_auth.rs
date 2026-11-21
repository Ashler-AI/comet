//! Crew: Scaffold sign-in in the engine's [`Auth`] (docs/crew/zgx-thin-port.md,
//! slice 2), ported from Crew's `crates/engine/src/auth.rs` (Ashler-AI/comet).
//!
//! A child of `auth`, so upstream's `auth.rs` keeps WorkOS, runner and dev mode
//! and only calls in here where a Scaffold authority (`AuthConfig::scaffold`)
//! or a sandbox host's device grant (`AuthConfig::device`) replaces them:
//!
//! - Signed out at start. A retained `scaffold-session.json` (0600) is verified
//!   with Scaffold and the Edge before anything trusts it ([`Auth::detect`]).
//! - Sign-in is Scaffold's resource-bound OAuth (`zeron_sync::scaffold_auth`:
//!   PKCE, dynamic client registration) redirecting to this engine's loopback
//!   on an ephemeral port; headed and headless alike.
//! - Signed in: `SignedIn { user, orgId: projectScope }`. No organizations,
//!   runner, step-up, Cloud, vault or plugin keys ([`SCAFFOLD_UNSUPPORTED`]).
//! - The bearer is an `sc_rc_` credential, good until revoked, with exec and
//!   lifecycle scopes; it never leaves the engine. Its verification is kept
//!   until sign-out, replacement or rejection; only outages expire.
//!
//! Deferred: Crew's rejection fence (`AccessToken { generation }` through
//! `TokenSource::invalidate`). Without it an Edge 401 doesn't send the engine
//! back to Scaffold; a revoked credential is noticed at the next start.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zeron_rpc::TokenError;
use zeron_sync::scaffold_auth::{
    AuthError as ScaffoldError, DeviceConfig, ScaffoldAuth, ScaffoldSession, load_session,
    profile_key, save_session,
};

use super::{
    Auth, AuthConfig, AuthUser, SignInLifecycle, TOKEN_SLACK, lock, page, state_for,
    write_private,
};
use crate::EngineError;

/// A user's Scaffold credential, in the data dir (0600).
pub(super) const SESSION_FILE: &str = "scaffold-session.json";
// A sandbox host's rotating device credential, or its sign-out tombstone.
use crate::crew_sandbox::DEVICE_SESSION_FILE;

/// What an account RPC answers in Scaffold mode when Scaffold has no part in
/// it (`RpcError::Capability`): `AccessToken` (the `sc_rc_` bearer stays in
/// the engine), `StepUp`, `DeleteAccount`, `CreateOrg` and `SelectOrg`.
pub const SCAFFOLD_UNSUPPORTED: &str = "crew.scaffold_unsupported";

/// The Scaffold side of [`Auth`] (`AuthInner::crew`).
#[derive(Default)]
pub(super) struct CrewAuth {
    /// Scaffold mode: an authority is configured, or this is a device host.
    /// Even when the helper below couldn't be built (fails closed).
    enabled: bool,
    helper: Option<ScaffoldAuth>,
    session: Mutex<Option<ScaffoldSession>>,
    /// Startup workspace intent only, never evidence of verified authority.
    loaded_session: bool,
    device_mode: bool,
}

/// The Scaffold credential's lifecycle, under `AuthInner::sign_in`.
#[derive(Default)]
pub(super) struct Lifecycle {
    /// Its verification must be done again before it's used.
    invalidated: bool,
    /// When a device grant is renewed (epoch ms).
    device_refresh_at: i64,
}

impl CrewAuth {
    /// The authority `config` names and its retained credential, unverified.
    /// A device host's config gains its grant's coordinates when a restart
    /// has only the durable credential.
    pub(super) fn load(config: &mut AuthConfig) -> Self {
        let helper = config.scaffold.clone().and_then(|scaffold| ScaffoldAuth::new(scaffold).ok());
        let device_file = config.data_dir.join(DEVICE_SESSION_FILE);
        let device_mode = config.device.is_some() || device_file.exists();
        let session = if device_mode {
            load_session(&device_file).ok().flatten().filter(|session| {
                session.device.as_ref().is_some_and(|saved| config.device.as_ref().is_none_or(|device| {
                    // A new grant re-enrolls; a consumed grant restores its durable credential.
                    device.join_grant.is_empty() || device.join_grant.strip_prefix("cg1.")
                        .and_then(|grant| grant.split_once('.'))
                        .is_some_and(|(id, _)| id == saved.grant_id)
                }))
            })
        } else if config.scaffold.is_some() {
            load_session(&config.data_dir.join(SESSION_FILE)).ok().flatten().filter(|session| session.device.is_none())
        } else {
            None
        };
        if config.device.is_none() {
            config.device = session.as_ref().and_then(|session| session.device.as_ref()).map(|device| DeviceConfig {
                join_grant: String::new(), device_id: device.device_id.clone(), session_id: device.session_id.clone(),
                deployment_id: device.deployment_id.clone(), sandbox_id: device.sandbox_id.clone(), lifecycle_epoch: device.lifecycle_epoch,
            });
        }
        Self {
            enabled: config.scaffold.is_some() || device_mode,
            helper,
            loaded_session: session.is_some(),
            session: Mutex::new(session),
            device_mode,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }
}

/// A verified Scaffold scope (Crew's `CollaborationScope`, which upstream's
/// proto doesn't have).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldScope {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Verified device authority; never confers access to the personal registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceGrantAuthority {
    pub grant_id: String,
    pub expires_at: i64,
    pub principal_subject: String,
    pub scope: ScaffoldScope,
    pub sandbox_id: String,
    pub device_id: String,
    pub lifecycle_epoch: u64,
    pub capabilities: Vec<String>,
}

impl Auth {
    /// Crew's Scaffold authority (or a sandbox device grant) is this engine's.
    pub fn scaffold_enabled(&self) -> bool {
        self.inner.crew.enabled
    }

    /// A sandbox host, signed in with its device grant (never a user).
    pub fn device_mode(&self) -> bool {
        self.inner.crew.device_mode
    }

    /// Dev mode (signed in, bearer = user id): only when explicitly allowed.
    pub fn dev_enabled(&self) -> bool {
        self.inner.config.allow_dev_auth
            && !self.scaffold_enabled()
            && self.inner.workos.is_none()
            && self.inner.runner.is_none()
    }

    /// An authority someone signs in with: Scaffold or WorkOS.
    pub fn is_authenticated_mode(&self) -> bool {
        self.scaffold_enabled() || self.workos_enabled()
    }

    /// Construction found a retained Scaffold credential. A startup fact
    /// that reserves the synced workspace, like [`Auth::loaded_workos_session`];
    /// sign-out doesn't rewrite it, and it vouches for nobody.
    pub fn loaded_scaffold_session(&self) -> bool {
        self.inner.crew.loaded_session
    }

    /// A retained user credential is waiting on its verification (an outage):
    /// startup may wait for it. Device hosts never do; rejection ends it.
    pub fn pending_scaffold_verification(&self) -> bool {
        let _lifecycle = lock(&self.inner.sign_in);
        !self.device_mode()
            && self.inner.crew.helper.is_some()
            && !self.inner.state_tx.borrow().is_signed_in()
            && lock(&self.inner.crew.session).is_some()
    }

    /// The verified Scaffold session, while signed in with it.
    pub fn verified_scaffold_session(&self) -> Option<ScaffoldSession> {
        let _lifecycle = lock(&self.inner.sign_in);
        if !self.state().is_signed_in() {
            return None;
        }
        lock(&self.inner.crew.session).clone()
    }

    /// The verified identity's profile key (its store's name), while signed in.
    pub fn verified_profile_key(&self) -> Option<String> {
        if !self.state().is_signed_in() {
            return None;
        }
        lock(&self.inner.crew.session).as_ref().map(profile_key)
    }

    /// The verified Scaffold scope, while signed in.
    pub fn verified_scope(&self) -> Option<ScaffoldScope> {
        if !self.state().is_signed_in() {
            return None;
        }
        let session = lock(&self.inner.crew.session);
        let session = session.as_ref()?;
        Some(ScaffoldScope {
            project_id: session.identity.project_scope.clone(),
            deployment_id: session.identity.deployment_id.clone(),
            session_id: session.device.as_ref().map(|device| device.session_id.clone()),
        })
    }

    /// A device host's verified, unexpired grant.
    pub fn device_grant_authority(&self) -> Option<DeviceGrantAuthority> {
        if !self.state().is_signed_in() {
            return None;
        }
        let stored = lock(&self.inner.crew.session);
        let session = stored.as_ref()?;
        let device = session.device.as_ref()?;
        if device.expires_at <= crate::now_ms() {
            return None;
        }
        Some(DeviceGrantAuthority {
            grant_id: device.grant_id.clone(),
            expires_at: device.expires_at,
            principal_subject: session.identity.user_id.clone(),
            scope: ScaffoldScope {
                project_id: session.identity.project_scope.clone(),
                deployment_id: Some(device.deployment_id.clone()),
                session_id: Some(device.session_id.clone()),
            },
            sandbox_id: device.sandbox_id.clone(),
            device_id: device.device_id.clone(),
            lifecycle_epoch: device.lifecycle_epoch,
            capabilities: session.identity.capabilities.clone(),
        })
    }

    /// Whether `method` (an `AuthRpc` method) is refused here: in Scaffold
    /// mode, what only WorkOS or its Edge routes answer ([`SCAFFOLD_UNSUPPORTED`]).
    pub fn refuses_in_scaffold_mode(&self, method: &str) -> bool {
        use zeron_rpc::methods;
        self.scaffold_enabled()
            && matches!(
                method,
                methods::ACCESS_TOKEN
                    | methods::STEP_UP
                    | methods::DELETE_ACCOUNT
                    | methods::CREATE_ORG
                    | methods::SELECT_ORG
            )
    }

    /// Network credentials pinned to this runtime's verified profile.
    pub fn for_profile_tokens(&self) -> Arc<ProfileTokens> {
        Arc::new(ProfileTokens {
            auth: self.clone(),
            scaffold: self.scaffold_enabled(),
            key: self.verified_profile_key(),
        })
    }

    // -- for auth.rs ----------------------------------------------------------

    /// [`Auth::detect`]: verify a retained credential now, so a profile is
    /// chosen with verified identity (or an outage reported as pending).
    pub(crate) async fn verify_retained_scaffold(&self) {
        if !self.scaffold_enabled() {
            return;
        }
        if let Err(error) = self.access_token().await {
            if self.device_mode() {
                tracing::error!(%error, "Crew sandbox device authentication failed; native host startup requires verified authority");
            } else if self.loaded_scaffold_session() {
                match error {
                    TokenError::TemporarilyUnavailable(_) => tracing::warn!("initial managed session verification unavailable; synced profile requires verified authority"),
                    TokenError::SignedOut => tracing::warn!("initial managed session verification rejected; synced workspace remains unavailable"),
                }
            }
        }
    }

    /// Upstream's dev mode has nothing to sign in to; with no authority and
    /// no dev auth allowed, there's nothing to sign in with (Crew). A Cloud
    /// runner's identity is its key, as upstream.
    pub(super) fn no_sign_in_authority(&self) -> Result<(), EngineError> {
        if self.dev_enabled() || self.is_runner() {
            Ok(())
        } else {
            Err(EngineError::Other("no sign-in authority is configured".into()))
        }
    }

    /// The bearer, and the token epoch it was read at, under the lifecycle lock.
    pub(super) async fn scaffold_credential(&self) -> Result<(String, u64), TokenError> {
        let generation = lock(&self.inner.sign_in).generation;
        let result = self.refresh(None).await;
        let lifecycle = lock(&self.inner.sign_in);
        // Never hand a late flight's bearer/error to a replacement profile.
        if lifecycle.generation != generation {
            return if lock(&self.inner.crew.session).is_none() {
                Err(TokenError::SignedOut)
            } else {
                Err(TokenError::TemporarilyUnavailable("session changed during verification; retry".into()))
            };
        }
        if lock(&self.inner.crew.session).as_ref().and_then(|session| session.device.as_ref())
            .is_some_and(|device| device.expires_at <= crate::now_ms())
        {
            return Err(TokenError::TemporarilyUnavailable("device grant expired; renewal required".into()));
        }
        let token = result
            .map_err(|error| TokenError::TemporarilyUnavailable(error.to_string()))?
            .ok_or(TokenError::SignedOut)?;
        Ok((token, *self.inner.token_tx.borrow()))
    }

    /// [`Auth::refresh_serialized`]'s Scaffold leg, under the refresh gate:
    /// its outcome, and how long the authority said to wait after a failure.
    pub(super) async fn scaffold_refresh_locked(&self) -> (Result<Option<String>, EngineError>, Option<Duration>) {
        let result = self.scaffold_refresh().await;
        let retry_after = match &result {
            Err(ScaffoldError::Unavailable { retry_after, .. }) => *retry_after,
            _ => None,
        };
        (result.map_err(|error| EngineError::Other(error.to_string())), retry_after)
    }

    /// Verify the user credential, or exchange/renew the device grant. The
    /// commit is fenced against sign-out and replacement by the same
    /// lifecycle as OAuth callbacks.
    async fn scaffold_refresh(&self) -> Result<Option<String>, ScaffoldError> {
        let helper = self.inner.crew.helper.as_ref().ok_or_else(|| ScaffoldError::Unavailable {
            message: "Scaffold auth is not configured correctly".into(), retry_after: None,
        })?;
        let (generation, invalidated, refresh_at, current) = {
            let lifecycle = lock(&self.inner.sign_in);
            (lifecycle.generation, lifecycle.crew.invalidated, lifecycle.crew.device_refresh_at, lock(&self.inner.crew.session).clone())
        };
        let result = match (&self.inner.config.device, current.as_ref()) {
            (Some(device), None) if generation == 0 && !device.join_grant.is_empty() => helper.exchange_device(device).await,
            (Some(device), Some(session)) => {
                // Use the grant's real deadline, including wall time elapsed in suspend.
                let now = crate::now_ms();
                if !invalidated && self.state().is_signed_in() && now < refresh_at {
                    if session.device.as_ref().is_some_and(|grant| grant.expires_at > now) {
                        return Ok(Some(session.access_token.clone()));
                    }
                    return Err(ScaffoldError::Unavailable {
                        message: "device grant expired; renewal is cooling down".into(),
                        retry_after: Some(Duration::from_millis((refresh_at - now) as u64)),
                    });
                }
                helper.refresh_device(session, device).await
            }
            (None, Some(session)) if !self.device_mode() => helper.verify(session).await,
            _ => return Ok(None),
        };
        let mut lifecycle = lock(&self.inner.sign_in);
        if lifecycle.generation != generation {
            // Rejection feedback can arrive while a rotating grant is in flight.
            // Preserve that replacement secret, but require a new verification;
            // sign-out/profile replacement must never be resurrected here.
            if lifecycle.crew.invalidated && current.as_ref().is_some_and(|old| {
                old.device.is_some() && lock(&self.inner.crew.session).as_ref()
                    .is_some_and(|saved| saved.access_token == old.access_token)
            }) {
                match result {
                    Ok(session) => {
                        let persistence = save_session(&self.inner.config.data_dir.join(DEVICE_SESSION_FILE), &session);
                        *lock(&self.inner.crew.session) = Some(session);
                        persistence?;
                    }
                    // Feedback against this same credential cannot erase the
                    // authority's concurrent request to stop retrying it.
                    Err(error @ ScaffoldError::Unavailable { .. }) => return Err(error),
                    Err(ScaffoldError::Rejected) => {}
                }
            }
            return Ok(None);
        }
        match result {
            Ok(session) => {
                let token = session.access_token.clone();
                let persistence = if session.device.is_some() {
                    save_session(&self.inner.config.data_dir.join(DEVICE_SESSION_FILE), &session)
                } else {
                    Ok(())
                };
                if let Err(error) = persistence {
                    lifecycle.crew.invalidated = true;
                    // Retain the rotated token in memory, but never complete bootstrap
                    // until its restart credential is durable.
                    *lock(&self.inner.crew.session) = Some(session);
                    return Err(error);
                }
                self.install_scaffold_session(session, &mut lifecycle);
                Ok(Some(token))
            }
            Err(ScaffoldError::Rejected) => {
                tracing::warn!("auth: Scaffold rejected the credential; signing out");
                self.clear_session(&mut lifecycle);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Signed in as the verified identity: `SignedIn { orgId: projectScope }`.
    fn install_scaffold_session(&self, session: ScaffoldSession, lifecycle: &mut SignInLifecycle) {
        let user = AuthUser {
            id: session.identity.user_id.clone(),
            email: session.identity.email.clone(),
            name: session.identity.name.clone(),
        };
        let state = state_for(user, Some(session.identity.project_scope.clone()));
        let changed = lifecycle.crew.invalidated
            || lock(&self.inner.crew.session).as_ref().is_none_or(|old| old.access_token != session.access_token);
        lifecycle.crew.invalidated = false;
        lifecycle.crew.device_refresh_at = session.device.as_ref().map_or(0, |device| {
            // Short or unchanged grants must not make the background loop spin.
            device.expires_at.saturating_sub(TOKEN_SLACK.as_millis() as i64)
                .max(crate::now_ms().saturating_add(1_000))
        });
        *lock(&self.inner.crew.session) = Some(session);
        self.inner.state_tx.send_if_modified(|old| {
            if *old == state {
                false
            } else {
                *old = state;
                true
            }
        });
        if changed {
            self.inner.token_tx.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        }
    }

    /// [`Auth::clear_session`]'s Scaffold part: cancel sign-ins in flight and
    /// forget the credential, in memory and on disk. A device host keeps a
    /// tombstone, so a restart stays a device host (and signed out).
    pub(super) fn clear_scaffold_session(&self, lifecycle: &mut SignInLifecycle) {
        lifecycle.crew.invalidated = false;
        if !self.scaffold_enabled() {
            return;
        }
        if let Some(helper) = &self.inner.crew.helper {
            helper.cancel_sign_in();
        }
        *lock(&self.inner.crew.session) = None;
        if self.device_mode() {
            // Preserve a restrictive role tombstone across restart, never a usable secret.
            let path = self.inner.config.data_dir.join(DEVICE_SESSION_FILE);
            if std::fs::create_dir_all(&self.inner.config.data_dir).and_then(|()| write_private(&path, b"{}")).is_err() {
                tracing::warn!("auth: could not clear device credential");
            }
        } else {
            match std::fs::remove_file(self.inner.config.data_dir.join(SESSION_FILE)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => tracing::warn!("auth: could not remove Scaffold credential"),
            }
        }
    }

    fn scaffold_helper(&self) -> Result<&ScaffoldAuth, EngineError> {
        self.inner.crew.helper.as_ref()
            .ok_or_else(|| EngineError::Other("Scaffold auth is not configured correctly".into()))
    }

    /// Scaffold's authorize URL, redirecting to this engine's loopback.
    pub(super) async fn start_scaffold_sign_in(&self) -> Result<String, EngineError> {
        if self.device_mode() {
            return Err(EngineError::Other("device hosts cannot start user sign-in".into()));
        }
        let helper = self.scaffold_helper()?;
        let port = self.ensure_loopback().await?;
        helper.begin_sign_in(&format!("http://127.0.0.1:{port}/callback")).await
            .map_err(|error| EngineError::Other(error.to_string()))
    }

    /// Exchange the callback (its URL, or `state.code`), verify the bearer,
    /// keep it (0600) and sign in, unless a sign-out came first.
    pub(super) async fn complete_scaffold_sign_in(&self, pasted: &str) -> Result<(), EngineError> {
        if self.device_mode() {
            return Err(EngineError::Other("device hosts cannot complete user sign-in".into()));
        }
        let generation = lock(&self.inner.sign_in).generation;
        let helper = self.scaffold_helper()?;
        let session = helper.complete_sign_in(pasted).await.map_err(|error| EngineError::Other(error.to_string()))?;
        let mut lifecycle = lock(&self.inner.sign_in);
        if lifecycle.generation != generation {
            return Err(EngineError::Other("sign-in was canceled".into()));
        }
        save_session(&self.inner.config.data_dir.join(SESSION_FILE), &session)
            .map_err(|error| EngineError::Other(error.to_string()))?;
        lifecycle.generation = lifecycle.generation.wrapping_add(1);
        lifecycle.crew.invalidated = true;
        self.reset_refresh_retry(true);
        tracing::info!(project = %session.identity.project_scope, "auth: signed in with Scaffold");
        self.install_scaffold_session(session, &mut lifecycle);
        Ok(())
    }

    /// [`Auth::spawn_refresh_loop`] in Scaffold mode: verify a retained
    /// credential until it lands, renew a device grant before it ends, and
    /// try again on wake and connectivity (never inside a Retry-After).
    pub(super) async fn scaffold_refresh_loop(&self) {
        let mut wake = zeron_sync::wake::subscribe();
        let mut online = zeron_sync::wake::subscribe_online();
        let mut tokens = self.inner.token_tx.subscribe();
        let mut state = self.watch_state();
        let mut retries = self.inner.retry_tx.subscribe();
        loop {
            let wait = {
                let lifecycle = lock(&self.inner.sign_in);
                let session = lock(&self.inner.crew.session);
                let active = session.is_some() || (self.device_mode() && lifecycle.generation == 0
                    && self.inner.config.device.as_ref().is_some_and(|device| !device.join_grant.is_empty()));
                if !active {
                    None
                } else if let Some(failure) = &lock(&self.inner.refresh_retry).failure {
                    Some(failure.remaining())
                } else if lifecycle.crew.invalidated || !self.state().is_signed_in() {
                    Some(Duration::ZERO)
                } else {
                    session.as_ref().and_then(|session| session.device.as_ref()).map(|_| {
                        Duration::from_millis(lifecycle.crew.device_refresh_at.saturating_sub(crate::now_ms()).max(0) as u64)
                    })
                }
            };
            tokio::select! {
                _ = async {
                    match wait {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                _ = wake.recv() => { self.retry_refresh(); }
                _ = online.recv() => { self.retry_refresh(); }
                changed = tokens.changed() => { if changed.is_err() { return; } continue; }
                changed = state.changed() => { if changed.is_err() { return; } continue; }
                changed = retries.changed() => { if changed.is_err() { return; } continue; }
            }
            let _ = self.access_token().await;
        }
    }
}

/// The loopback callback in Scaffold mode: the whole URL goes to Scaffold's
/// helper, which checks it against the redirect it registered.
pub(super) async fn scaffold_callback(auth: &Auth, method: Option<&str>, callback: &str) -> (&'static str, String) {
    if method != Some("GET") {
        return ("405 Method Not Allowed", page("Use the sign-in callback link."));
    }
    match auth.complete_sign_in(callback).await {
        Ok(()) => ("200 OK", page("Signed in. You can return to Crew.")),
        Err(error) => {
            tracing::warn!(%error, "auth: Scaffold sign-in callback failed");
            ("400 Bad Request", page("Sign-in failed or expired. Start again from Crew."))
        }
    }
}

/// Network credentials pinned to one runtime's verified profile, never
/// retargeted: a Scaffold runtime's bearer is refused once its identity
/// isn't the one the runtime opened with. Elsewhere, [`Auth`]'s own.
pub struct ProfileTokens {
    auth: Auth,
    scaffold: bool,
    key: Option<String>,
}

impl ProfileTokens {
    async fn bearer(&self) -> Result<String, TokenError> {
        if !self.scaffold {
            return zeron_rpc::TokenSource::token(&self.auth).await;
        }
        let (token, epoch) = self.auth.scaffold_credential().await?;
        let _lifecycle = lock(&self.auth.inner.sign_in);
        if epoch != *self.auth.inner.token_tx.borrow() {
            return Err(TokenError::TemporarilyUnavailable("session changed during verification; retry".into()));
        }
        if self.key.is_none() || self.auth.verified_profile_key() != self.key {
            return Err(TokenError::SignedOut);
        }
        Ok(token)
    }
}

#[async_trait::async_trait]
impl zeron_rpc::TokenSource for ProfileTokens {
    async fn token(&self) -> Result<String, TokenError> {
        self.bearer().await
    }

    fn subscribe(&self) -> Option<watch::Receiver<u64>> {
        Some(self.auth.inner.token_tx.subscribe())
    }
}

#[async_trait::async_trait]
impl zeron_preview::signaling::TokenSource for ProfileTokens {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.bearer().await?)
    }
}
