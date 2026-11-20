//! Crew: managed inference, "Agent Auth" (docs/crew/zgx-thin-port.md,
//! slice 5), on upstream's [`RunEnvironment`] hook rather than Crew's route
//! context in `RunControls`.
//!
//! A Scaffold engine (`crew-runtime.json`, slice 2) runs every agent on Agent
//! Auth's shared accounts and nothing else. [`CrewInference`] is then the
//! registry's only run environment, from before boot recovery to shutdown:
//!
//! - Each spawn of Claude Code or Codex (a turn, an idle start, a warm start, a
//!   title) gets a route on the loopback relay ([`relay`]) for its chat, the
//!   chat's `crewAccountId` model option (else Agent Auth's own choice) and its
//!   provider, and only that route's variables in `RunRequest::env`. The harness
//!   drops every inherited provider credential and keeps the CLI off its own
//!   login (`zeron_harness::crew_inference`). Titles run in a titling session
//!   of their own per harness.
//! - Fails closed: any other harness, a signed-out engine, or Agent Auth out of
//!   reach stops the spawn (`Err`), never falling back to this device's logins.
//! - [`RunEnvironment::credential`] is a fingerprint of the chat's account
//!   choice and its route, so choosing another account replaces a parked session
//!   before its next turn.
//! - Upstream's other ways to a provider refuse while this is on: account
//!   switching and pins (plugins' too, and their `run.preparing` pins never
//!   apply) and Codex voice.
//! - Model lists are each harness's own (its CLI's, probed as a managed run
//!   would launch: `zeron_harness::crew_inference::set_managed_discovery`),
//!   less what Agent Auth can't route, each with the account choice
//!   ([`CrewInference::offer`]). Nothing here names a model: one a CLI release
//!   adds is offered as it arrives, and a chat that names none runs its
//!   harness's own default.
//!
//! The authority is the engine's verified Scaffold session only
//! ([`crate::auth::ProfileTokens`]); nothing here persists a credential, and
//! the environment is read for one thing, Crew's development switch
//! (`ZERON_INFERENCE_MODE=local`: this device's own logins, as upstream). An
//! engine with no Scaffold authority is unchanged.

#[path = "crew_agent_auth.rs"]
mod agent_auth;
#[path = "crew_inference_relay.rs"]
mod relay;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use zeron_harness::crew_inference::{ManagedRoute, Provider};
use zeron_proto::{ChatConfig, HarnessId, Model, ModelOption, ModelOptionChoice, RunRequest};

use crate::registry::{HarnessRegistry, RunEnvironment};
use agent_auth::{AgentAuthClient, RemoteAgentAccount};
use relay::{InferenceRelay, PrepareError};

/// The model option a chat's account choice rides in (its value an Agent
/// Auth account id), as in Crew.
pub const ACCOUNT_OPTION: &str = "crewAccountId";
/// That option's default: Agent Auth chooses from the shared pool.
pub const AUTOMATIC: &str = "auto";
/// How long a listed set of accounts serves model lists.
const ACCOUNTS_FRESH: Duration = Duration::from_secs(60);
/// A model list waits this long for the accounts, then goes without.
const ACCOUNTS_DEADLINE: Duration = Duration::from_secs(5);

/// What the paths around the managed route answer while it's on.
pub(crate) const OWN_LOGINS_UNUSED: &str = "Crew runs agents on its shared accounts, so this device's own agent logins aren't used. Choose a shared account with a chat's Account option.";
const SIGNED_OUT: &str = "Sign in to Crew to run agents: they run on Crew's shared accounts.";
const NOT_CONFIGURED: &str = "Crew's shared accounts aren't available on this engine, so agents can't run here.";

type ChatConfigs = Arc<dyn Fn(&str) -> Option<ChatConfig> + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Crew's development switch, as its launcher reads it: `ZERON_INFERENCE_MODE`
/// (or `COMET_INFERENCE_MODE`) `local` runs a Scaffold engine's agents on this
/// device's own logins, as an upstream engine's. The zgx app hands it to the
/// engine it starts (apps/zgx/src/crew.rs `local_inference`).
fn local_inference() -> bool {
    local_inference_in(|name| std::env::var(name).ok())
}

fn local_inference_in(var: impl Fn(&str) -> Option<String>) -> bool {
    ["ZERON_INFERENCE_MODE", "COMET_INFERENCE_MODE"]
        .iter()
        .any(|name| var(name).is_some_and(|mode| mode.trim().eq_ignore_ascii_case("local")))
}

/// A Scaffold engine's managed inference (see the module docs).
pub struct CrewInference {
    /// `None`: no Agent Auth to reach (no authority origin, or the relay
    /// couldn't start). Every spawn then fails.
    relay: Option<InferenceRelay>,
    client: Option<AgentAuthClient>,
    /// A chat's current configuration (its account choice), once the
    /// workspace is up.
    chat_configs: Mutex<Option<ChatConfigs>>,
    /// The logical session each harness's title runs share.
    title_sessions: Mutex<HashMap<HarnessId, String>>,
    accounts: tokio::sync::Mutex<Option<(Instant, Vec<RemoteAgentAccount>)>>,
    /// The registry whose harnesses are asked for their catalogs, when a
    /// run names no model before any list was asked for.
    registry: OnceLock<Weak<HarnessRegistry>>,
    /// Each harness's own default model, from its last catalog.
    defaults: Mutex<HashMap<HarnessId, String>>,
}

impl CrewInference {
    /// The engine's managed inference: `Some` exactly when a Scaffold
    /// authority is this engine's, signed in or not. `tokens` is the verified
    /// profile's credential.
    pub(crate) fn for_engine(
        config: &crate::EngineConfig,
        auth: &crate::Auth,
        tokens: Arc<dyn zeron_rpc::TokenSource>,
    ) -> Option<Arc<Self>> {
        if !auth.scaffold_enabled() {
            return None;
        }
        if local_inference() {
            tracing::warn!("crew: inference mode local: agents run on this device's own logins");
            return None;
        }
        let Some(scaffold) = config.scaffold.as_ref() else {
            tracing::warn!("crew: no Scaffold authority origin; agent runs are refused");
            return Some(Arc::new(Self::closed()));
        };
        match Self::start(&scaffold.control_origin, tokens) {
            Ok(crew) => Some(Arc::new(crew)),
            Err(error) => {
                tracing::error!(%error, "crew: Agent Auth relay unavailable; agent runs are refused");
                Some(Arc::new(Self::closed()))
            }
        }
    }

    /// Agent Auth at `origin` (the Scaffold authority), as `tokens` signs in.
    pub(crate) fn start(origin: &str, tokens: Arc<dyn zeron_rpc::TokenSource>) -> Result<Self, String> {
        let client = AgentAuthClient::new(origin, tokens).map_err(|error| error.to_string())?;
        let relay = InferenceRelay::start(client.clone()).map_err(|error| error.to_string())?;
        tracing::info!(relay = %relay.base_url(), "crew: managed inference on Agent Auth");
        Ok(Self {
            relay: Some(relay),
            client: Some(client),
            ..Self::closed()
        })
    }

    /// Managed, with nothing to reach: every spawn is refused.
    fn closed() -> Self {
        Self {
            relay: None,
            client: None,
            chat_configs: Mutex::new(None),
            title_sessions: Mutex::new(HashMap::new()),
            accounts: tokio::sync::Mutex::new(None),
            registry: OnceLock::new(),
            defaults: Mutex::new(HashMap::new()),
        }
    }

    /// Where a chat's configuration is read (the workspace's chat rows).
    pub(crate) fn read_chat_configs(&self, configs: impl Fn(&str) -> Option<ChatConfig> + Send + Sync + 'static) {
        *lock(&self.chat_configs) = Some(Arc::new(configs));
    }

    /// Read chats' account choices from `workspace` (once the core is up).
    pub(crate) fn follow_workspace(&self, workspace: crate::workspace_host::WorkspaceHost) {
        self.read_chat_configs(move |chat| workspace.chat_config(chat));
    }

    /// Stop the relay: every route ends. The workspace is let go, so a
    /// replaced runtime's graph can be dropped.
    pub(crate) fn shutdown(&self) {
        if let Some(relay) = &self.relay {
            relay.shutdown();
        }
        lock(&self.chat_configs).take();
    }

    fn title_session(&self, harness: HarnessId) -> String {
        lock(&self.title_sessions)
            .entry(harness)
            .or_insert_with(crate::new_id)
            .clone()
    }

    /// The account a chat chose, from its configuration now (`Some(None)`:
    /// Automatic); `None` while it has no configuration to read.
    fn chat_account(&self, chat_id: &str) -> Option<Option<String>> {
        let configs = lock(&self.chat_configs).clone()?;
        Some(account_choice(&configs(chat_id)?.model_options))
    }

    /// Whether `harness` runs here at all (its catalog is then worth asking).
    pub(crate) fn serves(&self, harness: HarnessId) -> Result<(), String> {
        if Provider::managed_harness(harness) { Ok(()) } else { Err(unsupported(harness)) }
    }

    /// `harness`'s own catalog (its CLI's) as a managed engine offers it:
    /// what Agent Auth can route, each with the account choice of its
    /// provider (OMP's models span two). Its default is kept for a run that
    /// names no model.
    pub(crate) async fn offer(&self, harness: HarnessId, mut models: Vec<Model>) -> Result<Vec<Model>, String> {
        self.serves(harness)?;
        models.retain(|model| relay::routable(harness, &model.id));
        self.mark_default(harness, &mut models).await;
        self.keep_default(harness, &models);
        let accounts = self.accounts().await;
        let mut options: HashMap<&'static str, ModelOption> = HashMap::new();
        for model in &mut models {
            let Some(provider) = Provider::for_model(harness, &model.id) else {
                continue;
            };
            let option = options
                .entry(provider.as_str())
                .or_insert_with(|| account_option(provider, &accounts))
                .clone();
            model.options.retain(|existing| existing.id != ACCOUNT_OPTION);
            model.options.push(option);
        }
        Ok(models)
    }

    /// OMP marks no model its default: a Crew one that names none runs what
    /// a Claude Code one would, Claude Code's own default (its CLI's), on
    /// OMP's Anthropic provider, when OMP lists it; else OMP's first.
    async fn mark_default(&self, harness: HarnessId, models: &mut [Model]) {
        if harness != HarnessId::Omp || models.iter().any(|model| model.default) {
            return;
        }
        let Ok(claude) = Box::pin(self.default_model(HarnessId::ClaudeCode)).await else {
            return;
        };
        let wanted = format!("anthropic/{claude}");
        if let Some(model) = models.iter_mut().find(|model| model.id == wanted) {
            model.default = true;
        }
    }

    fn keep_default(&self, harness: HarnessId, models: &[Model]) {
        if let Some(model) = models.iter().find(|model| model.default).or(models.first()) {
            lock(&self.defaults).insert(harness, model.id.clone());
        }
    }

    /// The model a run of `harness` that names none runs: its own default,
    /// from its last catalog, else from its catalog now.
    async fn default_model(&self, harness: HarnessId) -> Result<String, String> {
        if let Some(model) = lock(&self.defaults).get(&harness).cloned() {
            return Ok(model);
        }
        // (A spawn holds the harness's execution lease already: its catalog is
        // asked of the harness directly, as the registry's discovery would.)
        let registry = self.registry.get().and_then(Weak::upgrade).ok_or_else(|| NOT_CONFIGURED.to_owned())?;
        let harness_impl = registry.resolve(harness).map_err(|error| error.to_string())?;
        let mut models = harness_impl.models().await.map_err(|error| error.to_string())?;
        models.retain(|model| relay::routable(harness, &model.id));
        Box::pin(self.mark_default(harness, &mut models)).await;
        self.keep_default(harness, &models);
        lock(&self.defaults)
            .get(&harness)
            .cloned()
            .ok_or_else(|| format!("{} offers no model Crew's shared accounts can run.", harness.as_str()))
    }

    /// The principal's shared accounts, kept a minute; on a failure the last
    /// ones known (or none: the choice is then Automatic alone).
    async fn accounts(&self) -> Vec<RemoteAgentAccount> {
        let Some(client) = &self.client else {
            return Vec::new();
        };
        let mut cached = self.accounts.lock().await;
        if let Some((listed, accounts)) = cached.as_ref()
            && listed.elapsed() < ACCOUNTS_FRESH
        {
            return accounts.clone();
        }
        match tokio::time::timeout(ACCOUNTS_DEADLINE, client.list_accounts()).await {
            Ok(Ok(accounts)) => {
                *cached = Some((Instant::now(), accounts.clone()));
                accounts
            }
            failed => {
                let error = match failed {
                    Ok(Err(error)) => error.to_string(),
                    _ => "timed out".into(),
                };
                tracing::warn!(%error, "crew: shared accounts not listed");
                cached.as_ref().map(|(_, accounts)| accounts.clone()).unwrap_or_default()
            }
        }
    }
}

#[async_trait::async_trait]
impl RunEnvironment for CrewInference {
    async fn prepare(
        &self,
        harness: HarnessId,
        chat_id: Option<&str>,
        request: &mut RunRequest,
    ) -> Result<(), String> {
        if !Provider::managed_harness(harness) {
            return Err(unsupported(harness));
        }
        let relay = self.relay.as_ref().ok_or_else(|| NOT_CONFIGURED.to_owned())?;
        let (session, account) = match chat_id {
            Some(chat) => (chat.to_owned(), account_choice(&request.model_options)),
            // A run of no chat (a title): Agent Auth's choice, in this
            // harness's titling session.
            None => (self.title_session(harness), None),
        };
        // A route is for a model: one the run names none of runs its
        // harness's own default.
        let named = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty() && *model != "default")
            .map(str::to_owned);
        let model = match named {
            Some(model) => model,
            None => self.default_model(harness).await?,
        };
        let route = relay
            .prepare(&session, harness, Some(&model), account.as_deref())
            .await
            .map_err(|error| refused(harness, error))?;
        // The CLI runs the route's model, without a provider prefix.
        if request.model.as_deref() != Some(route.model.as_str()) {
            request.model = Some(route.model.clone());
        }
        let route = ManagedRoute {
            provider: route.provider,
            base_url: route.base_url,
            token: route.token,
        };
        request.env.extend(route.env());
        Ok(())
    }

    async fn credential(&self, harness: HarnessId, chat_id: Option<&str>) -> Option<String> {
        if !Provider::managed_harness(harness) {
            return None;
        }
        let relay = self.relay.as_ref()?;
        let Some(chat) = chat_id else {
            let session = self.title_session(harness);
            return Some(fingerprint(harness, None, relay.current_serial(&session, harness, None)));
        };
        // The chat's row says which account. One without a row yet (a draft's
        // warm start) is on whichever route it has.
        let account = match self.chat_account(chat) {
            Some(account) => account,
            None => relay.current_route(chat, harness).and_then(|(account, _)| account),
        };
        let serial = relay.current_serial(chat, harness, account.as_deref());
        Some(fingerprint(harness, account.as_deref(), serial))
    }
}

/// A chat's account choice in its model options (`None`: Automatic).
fn account_choice(options: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    options
        .get(ACCOUNT_OPTION)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|account| !account.is_empty() && *account != AUTOMATIC)
        .map(str::to_owned)
}

/// An opaque credential: the account chosen and the route serving it. Never
/// the token.
fn fingerprint(harness: HarnessId, account: Option<&str>, serial: Option<u64>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(harness.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(account.unwrap_or(AUTOMATIC).as_bytes());
    hasher.update(b"\0");
    hasher.update(serial.map(|serial| serial.to_string()).unwrap_or_default().as_bytes());
    let hex: String = hasher.finalize().iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("crew:{hex}")
}

/// The `crewAccountId` option for `provider`'s models: Automatic, then each
/// healthy shared login of that provider once (Crew's settings list).
fn account_option(provider: Provider, accounts: &[RemoteAgentAccount]) -> ModelOption {
    let mut choices = vec![ModelOptionChoice {
        id: AUTOMATIC.into(),
        label: "Automatic · shared pool".into(),
    }];
    let mut logins: Vec<String> = Vec::new();
    for account in accounts {
        let healthy = matches!(account.status.as_str(), "connected" | "active" | "ready");
        if !healthy || !account.provider.eq_ignore_ascii_case(provider.as_str()) || account.id.trim().is_empty() {
            continue;
        }
        // One person in one workspace is one login, however often an older
        // sign-in path saved it.
        let key = login_key(account);
        if logins.contains(&key) {
            continue;
        }
        logins.push(key);
        let label = [account.email.as_deref(), account.display_name.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|label| !label.is_empty())
            .unwrap_or("Shared account");
        choices.push(ModelOptionChoice {
            id: account.id.clone(),
            label: label.to_owned(),
        });
    }
    ModelOption {
        id: ACCOUNT_OPTION.into(),
        label: "Account".into(),
        choices,
        default_choice: AUTOMATIC.into(),
    }
}

/// Crew's `login_key`: a ChatGPT login is one email in one workspace.
fn login_key(account: &RemoteAgentAccount) -> String {
    let provider = account.provider.to_ascii_lowercase();
    let identity = account.provider_account_id.as_deref().unwrap_or(&account.id);
    match account.email.as_deref().map(str::trim).filter(|email| !email.is_empty()) {
        Some(email) if provider == "openai" => {
            let workspace = identity.split_once("::").map_or(identity, |(_, workspace)| workspace);
            format!("openai\u{1f}{workspace}\u{1f}{}", email.to_lowercase())
        }
        _ => format!("{provider}\u{1f}{identity}"),
    }
}

fn agent_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Fx => "fx",
        HarnessId::Pi => "Pi",
        HarnessId::Omp => "OMP",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Antigravity => "Antigravity",
        HarnessId::Muse => "Muse",
        HarnessId::Mock => "The test agent",
        HarnessId::Registry(_) => "This agent",
    }
}

fn unsupported(harness: HarnessId) -> String {
    format!(
        "Crew runs Claude Code and Codex on its shared accounts. {} isn't available with them yet.",
        agent_name(harness)
    )
}

/// What a spawn that got no route says. Never a credential or a response
/// body: reasons and causes are typed.
fn refused(harness: HarnessId, error: PrepareError) -> String {
    match error {
        PrepareError::Unsupported => format!(
            "Choose one of Crew's {} models: this one isn't served by its shared accounts.",
            agent_name(harness)
        ),
        PrepareError::InvalidAccount => "This chat's account choice is empty. Choose an account, or Automatic.".into(),
        PrepareError::Stopped => "This engine is stopping: Crew's shared accounts are closed to new runs.".into(),
        PrepareError::Authority(error) if error.signed_out => SIGNED_OUT.into(),
        PrepareError::Authority(error) => {
            tracing::warn!(reason = error.reason, cause = error.cause.as_deref(), "crew: no Agent Auth route");
            match error.retry_after {
                Some(wait) => format!(
                    "Crew's shared accounts can't be reached right now ({}). Try again in {} s.",
                    error.reason,
                    wait.as_secs().max(1)
                ),
                None => format!("Crew's shared accounts refused this run ({}).", error.reason),
            }
        }
    }
}

impl HarnessRegistry {
    /// Make `crew` this registry's only run environment (see the module docs).
    /// Before any run can start: boot recovery included.
    pub(crate) fn set_crew_inference(&self, crew: Arc<CrewInference>) {
        if self.crew.set(crew).is_err() {
            tracing::warn!("crew: managed inference was already installed");
        }
    }

    /// The managed inference, when this is a Scaffold engine's registry.
    pub(crate) fn crew_inference(&self) -> Option<&Arc<CrewInference>> {
        self.crew.get()
    }

    /// `Err` while managed: switching this device's own agent login, or
    /// pinning a chat to one.
    pub(crate) fn refuse_own_logins(&self) -> Result<(), String> {
        match self.crew_inference() {
            Some(_) => Err(OWN_LOGINS_UNUSED.into()),
            None => Ok(()),
        }
    }
}

/// [`crate::Engine`]'s runtime assembly: a Scaffold engine's `registry` gets
/// its managed inference before any run can start (`None`: not a Scaffold
/// engine, nothing changes).
pub(crate) fn install(
    config: &crate::EngineConfig,
    auth: &crate::Auth,
    tokens: Arc<dyn zeron_rpc::TokenSource>,
    registry: &Arc<HarnessRegistry>,
) -> Option<Arc<CrewInference>> {
    let crew = CrewInference::for_engine(config, auth, tokens)?;
    let _ = crew.registry.set(Arc::downgrade(registry));
    // Its harnesses' model probes run as its managed runs launch.
    zeron_harness::crew_inference::set_managed_discovery(true);
    registry.set_crew_inference(crew.clone());
    Some(crew)
}

/// [`crate::EngineCore::shutdown`]: the routes end with the runtime.
pub(crate) fn shutdown(registry: &HarnessRegistry) {
    if let Some(crew) = registry.crew_inference() {
        crew.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::relay::tests::{AuthorityFixture, ProfileToken, static_token, test_control_plane_with_authority};
    use super::*;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;
    use zeron_harness::crew_inference::{ROUTE_ENV, STRIPPED, TOKEN_ENV, URL_ENV};

    const CHAT: &str = "7b0e6a52-7f7e-4a39-9a2c-1b8c0f6f3d11";

    fn request(model: Option<&str>, account: Option<&str>) -> RunRequest {
        let mut request: RunRequest = serde_json::from_value(serde_json::json!({
            "prompt": "p", "model": model, "reasoning": null, "cwd": "/tmp",
            "sandbox": "workspace-write", "resume": null,
        }))
        .unwrap();
        if let Some(account) = account {
            request.model_options.insert(ACCOUNT_OPTION.into(), account.into());
        }
        request
            .env
            .insert(zeron_proto::ORIGIN_ENV.into(), zeron_proto::chat_origin(CHAT));
        request
    }

    async fn agent_auth() -> (String, Arc<AuthorityFixture>, mpsc::UnboundedReceiver<super::relay::tests::CapturedRequest>) {
        let fixture = AuthorityFixture::new(chrono::Utc::now() + chrono::TimeDelta::minutes(5));
        let (tx, rx) = mpsc::unbounded_channel();
        let origin = test_control_plane_with_authority(tx, fixture.clone()).await;
        (origin, fixture, rx)
    }

    fn model(id: &str, default: bool) -> Model {
        Model {
            id: id.into(),
            label: id.into(),
            description: None,
            provider: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
            default,
        }
    }

    /// A harness as its CLI would list its models (counting each ask); it
    /// never runs.
    struct CatalogHarness {
        id: HarnessId,
        models: Vec<Model>,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl CatalogHarness {
        fn new(id: HarnessId, models: Vec<Model>) -> Arc<Self> {
            Arc::new(Self { id, models, asked: Default::default() })
        }
    }

    #[async_trait::async_trait]
    impl zeron_harness::Harness for CatalogHarness {
        fn id(&self) -> HarnessId {
            self.id
        }
        fn display_name(&self) -> &str {
            "Catalog"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> zeron_proto::SteeringMode {
            zeron_proto::SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[zeron_proto::ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, zeron_harness::HarnessError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(self.models.clone())
        }
        async fn run(
            &self,
            _: RunRequest,
            _: zeron_harness::RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<zeron_proto::AgentEvent, zeron_harness::HarnessError>>,
            zeron_harness::HarnessError,
        > {
            panic!("a catalog harness never runs")
        }
    }

    /// Claude Code's CLI, as it lists its models: a default, a small one and
    /// another provider's (a gateway's), which Agent Auth can't route.
    fn claude_cli() -> Arc<CatalogHarness> {
        CatalogHarness::new(
            HarnessId::ClaudeCode,
            vec![model("claude-fable-5-2", false), model("claude-opus-5-5", true), model("claude-haiku-5-5", false), model("gateway/sol", false)],
        )
    }

    /// Codex's, as its `model/list` answers (a model a CLI release just
    /// added among them).
    fn codex_cli() -> Arc<CatalogHarness> {
        CatalogHarness::new(HarnessId::Codex, vec![model("gpt-6.2-sol", true), model("gpt-6-luna", false), model("gpt-7-nova", false)])
    }

    /// [`install`]'s wiring, less the process-wide probe switch: the
    /// registry's managed inference, and the registry it asks for catalogs.
    fn attach(crew: &Arc<CrewInference>, registry: &Arc<HarnessRegistry>) {
        let _ = crew.registry.set(Arc::downgrade(registry));
        registry.set_crew_inference(crew.clone());
    }

    /// An account environment that would put the user's own Claude login in
    /// a pinned chat's spawn (as `PinRunEnvironment` does).
    struct PinnedLogin;

    #[async_trait::async_trait]
    impl RunEnvironment for PinnedLogin {
        async fn prepare(&self, _: HarnessId, _: Option<&str>, request: &mut RunRequest) -> Result<(), String> {
            request.env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), "own-login-secret".into());
            Ok(())
        }
        async fn credential(&self, _: HarnessId, _: Option<&str>) -> Option<String> {
            Some("claude:own-login".into())
        }
    }

    /// (a) A managed spawn gets the route's variables and nothing else, even
    /// with the device's account environment (pins, Muse) installed; and the
    /// route reaches Agent Auth with the shared authority.
    #[tokio::test]
    async fn managed_spawns_get_only_the_relay_env() {
        let (origin, fixture, mut captured) = agent_auth().await;
        let registry = Arc::new(HarnessRegistry::new());
        registry.set_account_environment(Arc::new(PinnedLogin));
        let claude_cli = claude_cli();
        registry.register(claude_cli.clone());
        let crew = Arc::new(CrewInference::start(&origin, static_token()).unwrap());
        attach(&crew, &registry);

        let mut claude = request(None, None);
        registry.prepare_run(HarnessId::ClaudeCode, Some(CHAT), &mut claude).await.unwrap();
        let route = ManagedRoute::from_env(&claude.env).expect("a managed route");
        assert_eq!(route.provider, Provider::Anthropic);
        let keys: std::collections::BTreeSet<&str> = claude.env.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_VERTEX",
            TOKEN_ENV,
            ROUTE_ENV,
            URL_ENV,
            zeron_proto::ORIGIN_ENV,
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, expected, "the route's variables and the chat's origin, nothing else");
        assert!(claude.env.values().all(|value| value != "own-login-secret"));
        // Every credential the child gets is the route's opaque token.
        for key in STRIPPED.iter().filter(|key| claude.env.contains_key(**key)) {
            assert!(
                [route.token.as_str(), route.base_url.as_str()].contains(&claude.env[*key].as_str()),
                "{key} is the route's"
            );
        }
        assert_eq!(claude.env["ANTHROPIC_AUTH_TOKEN"], route.token);
        assert!(route.base_url.starts_with("http://127.0.0.1:"));
        // The chat named no model: its harness's own default, asked of its CLI
        // (no list was asked for yet), and kept.
        assert_eq!(claude.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_cli.asked.load(Ordering::SeqCst), 1);
        let mut again = request(None, None);
        registry.prepare_run(HarnessId::ClaudeCode, Some(CHAT), &mut again).await.unwrap();
        assert_eq!(again.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_cli.asked.load(Ordering::SeqCst), 1, "the default is kept");
        assert_eq!(registry.run_credential(HarnessId::ClaudeCode, Some(CHAT)).await, crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await);

        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages?beta=true", route.base_url))
            .bearer_auth(&route.token)
            .json(&serde_json::json!({ "model": "claude-opus-5-5", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let seen = captured.recv().await.unwrap();
        assert_eq!(seen.authorization, "Bearer remote-agent-auth-authority-1");
        assert_eq!(seen.conversation_id, CHAT);
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 1);

        let mut codex = request(Some("gpt-5.6-sol"), Some("acct-openai-1"));
        registry.prepare_run(HarnessId::Codex, Some("another-chat"), &mut codex).await.unwrap();
        let route = ManagedRoute::from_env(&codex.env).unwrap();
        assert_eq!(route.provider, Provider::Openai);
        assert_eq!(codex.env["OPENAI_BASE_URL"], format!("{}/v1", route.base_url));
        assert!(!codex.env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        let response = reqwest::Client::new()
            .post(format!("{}/v1/responses", route.base_url))
            .bearer_auth(&route.token)
            .json(&serde_json::json!({ "input": "hello" }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(captured.recv().await.unwrap().account_id.as_deref(), Some("acct-openai-1"));
        crew.shutdown();
    }

    /// (b) No route, no run: signed out, Agent Auth refusing or unreachable,
    /// or no authority at all. Nothing is put in the spawn's environment.
    #[tokio::test]
    async fn without_a_route_the_run_errors() {
        let (origin, fixture, _captured) = agent_auth().await;
        let token = Arc::new(ProfileToken::default());
        let crew = CrewInference::start(&origin, token.clone()).unwrap();

        token.signed_out.store(true, Ordering::SeqCst);
        let mut signed_out = request(Some("claude-opus-5-5"), None);
        let error = crew.prepare(HarnessId::ClaudeCode, Some(CHAT), &mut signed_out).await.unwrap_err();
        assert_eq!(error, SIGNED_OUT);
        assert!(!zeron_harness::crew_inference::managed(&signed_out.env));

        token.signed_out.store(false, Ordering::SeqCst);
        fixture.fail_issuance.store(true, Ordering::SeqCst);
        let mut refused = request(Some("gpt-6.2-sol"), None);
        let error = crew.prepare(HarnessId::Codex, Some(CHAT), &mut refused).await.unwrap_err();
        assert!(error.contains("can't be reached"), "{error}");
        assert!(!zeron_harness::crew_inference::managed(&refused.env));
        // A title (no chat) gets no route either: no title rather than a login.
        let mut title = request(Some("claude-haiku-5-5"), None);
        assert!(crew.prepare(HarnessId::ClaudeCode, None, &mut title).await.is_err());
        assert!(!zeron_harness::crew_inference::managed(&title.env));
        assert_eq!(fixture.forwarded.load(Ordering::SeqCst), 0);

        // An authority nothing serves.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let nowhere = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let unreachable = CrewInference::start(&nowhere, static_token()).unwrap();
        let mut run = request(Some("claude-opus-5-5"), None);
        assert!(unreachable.prepare(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.is_err());
        assert!(run.env.keys().all(|key| key == zeron_proto::ORIGIN_ENV));
        // A run naming no model, with no harness to ask for its default:
        // refused, nothing in its environment either.
        let mut unnamed = request(None, None);
        assert!(crew.prepare(HarnessId::Codex, Some(CHAT), &mut unnamed).await.is_err());
        assert!(unnamed.env.keys().all(|key| key == zeron_proto::ORIGIN_ENV));

        // Managed with no authority at all: the registry refuses every spawn.
        let registry = HarnessRegistry::new();
        registry.set_crew_inference(Arc::new(CrewInference::closed()));
        let mut run = request(None, None);
        assert_eq!(
            registry.prepare_run(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.unwrap_err(),
            NOT_CONFIGURED
        );
        crew.shutdown();
        unreachable.shutdown();
    }

    /// (c) The other ways to a provider refuse while managed: other harnesses,
    /// pins and account switching; titles go through Agent Auth too. A model
    /// list is the harness's own (its CLI's), less what Agent Auth can't
    /// route, each model with the account choice.
    #[tokio::test]
    async fn the_other_paths_refuse_in_managed_mode() {
        let (origin, fixture, _captured) = agent_auth().await;
        let crew = Arc::new(CrewInference::start(&origin, static_token()).unwrap());
        let registry = Arc::new(HarnessRegistry::new());
        let claude_cli = claude_cli();
        registry.register(claude_cli.clone());
        registry.register(codex_cli());
        assert!(registry.refuse_own_logins().is_ok(), "unmanaged: as upstream");
        attach(&crew, &registry);

        for harness in [HarnessId::Cursor, HarnessId::Opencode, HarnessId::Pi, HarnessId::Muse, HarnessId::Mock] {
            let mut run = request(None, None);
            let error = registry.prepare_run(harness, Some(CHAT), &mut run).await.unwrap_err();
            assert!(error.contains("Claude Code and Codex"), "{error}");
            assert!(!zeron_harness::crew_inference::managed(&run.env));
            assert!(crew.serves(harness).is_err());
            assert!(crew.offer(harness, vec![model("any", true)]).await.is_err());
            let listed = registry.discover_models(harness).await.unwrap_err();
            assert!(listed.to_string().contains("Claude Code and Codex"), "{listed}");
        }
        assert_eq!(registry.refuse_own_logins().unwrap_err(), OWN_LOGINS_UNUSED);

        let lease = Arc::new(registry.execution_lease(HarnessId::ClaudeCode).await);
        let models = registry.discover_models_with_lease(HarnessId::ClaudeCode, lease).await.unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["claude-fable-5-2", "claude-opus-5-5", "claude-haiku-5-5"], "the CLI's, less a gateway's");
        assert_eq!(claude_cli.asked.load(Ordering::SeqCst), 1, "asked of its CLI");
        let accounts = &models[0].options.iter().find(|option| option.id == ACCOUNT_OPTION).unwrap().choices;
        let ids: Vec<&str> = accounts.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(ids, ["auto", "acct-claude-1", "acct-claude-2"], "healthy Anthropic logins only");
        assert_eq!(accounts[2].label, "Team Two");
        let codex = registry.discover_models(HarnessId::Codex).await.unwrap();
        // A model a CLI release adds is offered as it arrives.
        assert!(codex.iter().any(|model| model.id == "gpt-7-nova"));
        assert!(codex.iter().any(|model| model.id == "gpt-6.2-sol" && model.default));
        let choices = &codex[0].options.last().unwrap().choices;
        let ids: Vec<&str> = choices.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(ids, ["auto", "acct-openai-1"], "one ChatGPT login per email and workspace");
        assert_eq!(fixture.accounts_listed.load(Ordering::SeqCst), 1, "listed once a minute");
        // A run naming none runs the default its list gave.
        let mut unnamed = request(None, None);
        registry.prepare_run(HarnessId::Codex, Some(CHAT), &mut unnamed).await.unwrap();
        assert_eq!(unnamed.model.as_deref(), Some("gpt-6.2-sol"));

        // A title (no chat) is routed too, in its harness's titling session,
        // on Agent Auth's choice.
        let mut title = request(Some("claude-haiku-5-5"), Some("acct-claude-1"));
        registry.prepare_run(HarnessId::ClaudeCode, None, &mut title).await.unwrap();
        let first = ManagedRoute::from_env(&title.env).unwrap();
        let mut again = request(Some("claude-haiku-5-5"), None);
        registry.prepare_run(HarnessId::ClaudeCode, None, &mut again).await.unwrap();
        assert_eq!(ManagedRoute::from_env(&again.env).unwrap().token, first.token, "titles share a route");
        let session = crew.title_session(HarnessId::ClaudeCode);
        assert!(uuid::Uuid::parse_str(&session).is_ok());
        assert_ne!(session, crew.title_session(HarnessId::Codex));
        // Once the runtime stops, nothing is routed: a title neither.
        crew.shutdown();
        let mut closed = request(None, None);
        assert!(registry.prepare_run(HarnessId::Codex, None, &mut closed).await.is_err());
        assert!(!zeron_harness::crew_inference::managed(&closed.env));
    }

    /// The parked session's credential: the chat's account choice and the
    /// route serving it. Another account (or route) is another credential.
    #[tokio::test]
    async fn the_credential_follows_the_chats_account_choice() {
        let (origin, _fixture, _captured) = agent_auth().await;
        let crew = CrewInference::start(&origin, static_token()).unwrap();
        let chosen = Arc::new(Mutex::new(None::<String>));
        let source = chosen.clone();
        crew.read_chat_configs(move |chat| {
            (chat == CHAT).then(|| ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: lock(&source)
                    .iter()
                    .map(|account| (ACCOUNT_OPTION.to_owned(), serde_json::Value::from(account.clone())))
                    .collect(),
                sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
                access: Default::default(),
                plan: false,
            })
        });
        let before = crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        let mut run = request(Some("claude-opus-5-5"), None);
        crew.prepare(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.unwrap();
        let started = crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        assert_ne!(before, started, "a route now serves the chat");
        assert_eq!(crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap(), started, "stable while unchanged");
        assert!(started.starts_with("crew:") && !started.contains(&ManagedRoute::from_env(&run.env).unwrap().token));

        // Choosing an account replaces the parked session before its next turn.
        *lock(&chosen) = Some("acct-claude-2".into());
        let switched = crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        assert_ne!(switched, started);
        let mut run = request(Some("claude-opus-5-5"), Some("acct-claude-2"));
        crew.prepare(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.unwrap();
        let on_account = crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        assert_ne!(on_account, switched);
        // Automatic again: the earlier route was retired, so it's new.
        *lock(&chosen) = Some(AUTOMATIC.into());
        assert_ne!(crew.credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap(), started);
        // No credential for what managed inference doesn't run.
        assert_eq!(crew.credential(HarnessId::Cursor, Some(CHAT)).await, None);

        // A draft (no row yet: a warm start) is on the route it was given, so
        // its run, prepared the same, finds the warm runtime alike.
        let mut warm = request(Some("claude-opus-5-5"), Some("acct-claude-1"));
        crew.prepare(HarnessId::ClaudeCode, Some("draft"), &mut warm).await.unwrap();
        let parked = crew.credential(HarnessId::ClaudeCode, Some("draft")).await.unwrap();
        let mut run = request(Some("claude-opus-5-5"), Some("acct-claude-1"));
        crew.prepare(HarnessId::ClaudeCode, Some("draft"), &mut run).await.unwrap();
        assert_eq!(run.env, warm.env, "the same route, token and all");
        assert_eq!(crew.credential(HarnessId::ClaudeCode, Some("draft")).await.unwrap(), parked);
        crew.shutdown();
    }

    /// (d) Without a Scaffold authority nothing changes: the registry keeps
    /// upstream's environments, and spawns get no route.
    #[tokio::test]
    async fn an_engine_without_a_scaffold_authority_is_unchanged() {
        let registry = HarnessRegistry::new();
        registry.set_account_environment(Arc::new(PinnedLogin));
        assert!(registry.crew_inference().is_none());
        let mut run = request(None, None);
        registry.prepare_run(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.unwrap();
        assert_eq!(run.env["CLAUDE_CODE_OAUTH_TOKEN"], "own-login-secret");
        assert!(!zeron_harness::crew_inference::managed(&run.env));
        assert_eq!(run.model, None);
        assert_eq!(
            registry.run_credential(HarnessId::ClaudeCode, Some(CHAT)).await.as_deref(),
            Some("claude:own-login")
        );
        shutdown(&registry);

        let dir = tempfile::tempdir().unwrap();
        let config = crate::EngineConfig {
            data_dir: dir.path().to_path_buf(),
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            ipc_port: 0,
            default_harness: HarnessId::ClaudeCode,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            scaffold: None,
            device: None,
        };
        let auth = crate::Engine::build_auth(&config).await;
        assert!(CrewInference::for_engine(&config, &auth, static_token()).is_none());
    }

    /// Through the engine's own surface: `ListModels` answers the harness's
    /// own models with the account choice, `ActivateAgentAccount` refuses, a
    /// chat's account comes from its workspace row, and the routes end with
    /// the core.
    #[tokio::test]
    async fn the_engine_lists_its_harnesses_models_and_refuses_its_own_logins() {
        let (origin, _fixture, _captured) = agent_auth().await;
        let crew = Arc::new(CrewInference::start(&origin, static_token()).unwrap());
        // Codex's CLI alone: nothing here reaches beyond loopback.
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(codex_cli());
        attach(&crew, &registry);
        let dir = tempfile::tempdir().unwrap();
        let core = crate::EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mock, None).unwrap();
        crew.follow_workspace(core.workspace.clone());
        let client = zeron_rpc::memory_client(core.rpc_service());

        let models = client
            .call(zeron_rpc::methods::LIST_MODELS, serde_json::json!({"harness": "codex", "force": true}))
            .await
            .unwrap();
        let models: Vec<Model> = serde_json::from_value(models).unwrap();
        assert!(models.iter().any(|model| model.id == "gpt-6.2-sol" && model.default));
        assert!(models.iter().any(|model| model.id == "gpt-7-nova"));
        let option = models[0].options.iter().find(|option| option.id == ACCOUNT_OPTION).unwrap();
        assert_eq!(option.default_choice, AUTOMATIC);
        assert!(option.choices.iter().any(|choice| choice.id == "acct-openai-1"));
        let other = client
            .call(zeron_rpc::methods::LIST_MODELS, serde_json::json!({"harness": "cursor"}))
            .await
            .unwrap_err();
        assert!(other.to_string().contains("Claude Code and Codex"), "{other}");
        let switched = client
            .call(
                zeron_rpc::methods::ACTIVATE_AGENT_ACCOUNT,
                serde_json::json!({"harness": "claude-code", "accountId": "own-login"}),
            )
            .await
            .unwrap_err();
        assert!(switched.to_string().contains("shared accounts"), "{switched}");

        // The chat's row says which account; the parked session's credential
        // follows it.
        let config = |account: &str| ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::from_iter([(ACCOUNT_OPTION.to_owned(), account.into())]),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            access: Default::default(),
            plan: false,
        };
        core.workspace
            .create_chat(CHAT, None, Some(&core.device_id), Some(config(AUTOMATIC)), None)
            .unwrap();
        let automatic = registry.run_credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        core.workspace.set_chat_config(CHAT, &config("acct-claude-1")).unwrap();
        let chosen = registry.run_credential(HarnessId::ClaudeCode, Some(CHAT)).await.unwrap();
        assert_ne!(automatic, chosen);

        core.shutdown().await;
        let mut run = request(None, None);
        assert!(registry.prepare_run(HarnessId::ClaudeCode, Some(CHAT), &mut run).await.is_err());
        assert!(lock(&crew.chat_configs).is_none(), "the workspace is let go");
    }

    /// OMP's list spans two providers and marks no default: what Agent Auth
    /// can route is offered, each with its provider's account choice, and its
    /// default is Claude Code's own, on OMP's Anthropic provider.
    #[tokio::test]
    async fn omp_offers_both_providers_and_defaults_to_claude_code_s_default() {
        let crew = CrewInference::closed();
        crew.offer(HarnessId::ClaudeCode, vec![model("claude-fable-5-2", false), model("claude-opus-5-5", true)])
            .await
            .unwrap();
        let omp = crew
            .offer(
                HarnessId::Omp,
                vec![
                    model("anthropic/claude-fable-5", false),
                    model("anthropic/claude-opus-5-5", false),
                    model("openai/gpt-6.2-sol", false),
                    model("google/gemini-3-pro", false),
                ],
            )
            .await
            .unwrap();
        let ids: Vec<&str> = omp.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["anthropic/claude-fable-5", "anthropic/claude-opus-5-5", "openai/gpt-6.2-sol"], "routable only");
        assert!(omp.iter().all(|model| model.options.iter().any(|option| option.id == ACCOUNT_OPTION)));
        assert!(omp.iter().any(|model| model.id == "anthropic/claude-opus-5-5" && model.default));
        assert_eq!(crew.default_model(HarnessId::Omp).await.unwrap(), "anthropic/claude-opus-5-5");
    }

    /// The development switch is `local` by either of Crew's names, and only
    /// that: managed is every other value, or none.
    #[test]
    fn only_the_local_switch_turns_managed_inference_off() {
        let local = |pairs: &[(&str, &str)]| {
            local_inference_in(|name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned()))
        };
        assert!(local(&[("ZERON_INFERENCE_MODE", "local")]));
        assert!(local(&[("COMET_INFERENCE_MODE", " Local ")]));
        assert!(!local(&[]));
        assert!(!local(&[("ZERON_INFERENCE_MODE", "managed")]));
        assert!(!local(&[("COMET_INFERENCE_MODE", "")]));
    }

    #[test]
    fn account_choice_reads_only_a_named_account() {
        let options = |value: serde_json::Value| {
            serde_json::Map::from_iter([(ACCOUNT_OPTION.to_owned(), value)])
        };
        assert_eq!(account_choice(&options("acct-1".into())).as_deref(), Some("acct-1"));
        assert_eq!(account_choice(&options(" acct-1 ".into())).as_deref(), Some("acct-1"));
        assert_eq!(account_choice(&options(AUTOMATIC.into())), None);
        assert_eq!(account_choice(&options("".into())), None);
        assert_eq!(account_choice(&options(serde_json::Value::Bool(true))), None);
        assert_eq!(account_choice(&serde_json::Map::new()), None);
    }
}
