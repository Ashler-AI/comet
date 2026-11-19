//! Crew: a managed run's launch contract (docs/crew/zgx-thin-port.md, slice 5),
//! ported from Crew's `crates/harness/src/run_context.rs` and the managed-route
//! launch overrides in its Claude and Codex drivers (Ashler-AI/comet).
//!
//! Crew runs Claude Code and Codex on Agent Auth's shared accounts. The engine's
//! Crew run environment (`zeron_engine::crew_inference`) puts a [`ManagedRoute`]
//! in [`zeron_proto::RunRequest::env`]: a loopback relay and an opaque token
//! for it. That map is host-resolved and never serialized, so only the engine
//! sets it. With a route in it, the drivers:
//!
//! - drop every provider credential the child would otherwise inherit
//!   ([`STRIPPED`]) before the run's own environment is applied
//!   ([`crate::apply_run_env`]);
//! - keep Claude Code off its own login: no `apiKeyHelper`, console login
//!   forced, and the relay as `ANTHROPIC_BASE_URL` with the token;
//! - point Codex at a `crew` model provider that reads the token from the
//!   environment and never uses the ChatGPT login, for new and resumed threads;
//! - never attach Codex's realtime voice, which would use the ChatGPT login.
//!
//! A route that doesn't fit its harness stops the launch. Without a route in
//! the map, nothing here changes a launch.

use std::collections::BTreeMap;

use serde_json::Value;
use zeron_proto::{HarnessId, Model, ReasoningLevel};

use crate::HarnessError;
use crate::process::Command;

/// The provider a managed run's route serves (`anthropic` or `openai`). Its
/// presence alone makes a launch managed, valid or not.
pub const ROUTE_ENV: &str = "ZERON_CREW_INFERENCE";
/// The relay's loopback origin (`http://127.0.0.1:<port>`).
pub const URL_ENV: &str = "ZERON_CREW_INFERENCE_URL";
/// The opaque relay token: Codex's `crew` provider reads it (`env_key`).
/// Crew's name for it, kept so managed OMP can follow.
pub const TOKEN_ENV: &str = "COMET_INFERENCE_TOKEN";

/// Inherited variables a managed child never gets: other provider credentials,
/// endpoints and headers, and a nested Crew run's identity (Crew's list).
pub const STRIPPED: &[&str] = &[
    "COMET_SESSION_ID",
    "COMET_IPC_PORT",
    "COMET_SUPERVISION_TOKEN",
    "COMET_INFERENCE_TOKEN",
    "PRIME_AGENT_AUTH_GATEWAY_URL",
    "OMP_AUTH_GATEWAY_URL",
    "OMP_AUTH_GATEWAY_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT_ID",
    "CODEX_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
    "ANTHROPIC_CUSTOM_HEADERS",
    "OPENAI_CUSTOM_HEADERS",
    "OMP_AUTH_BROKER_URL",
    "OMP_AUTH_BROKER_TOKEN",
    "OMP_AUTH_BROKER_TOKEN_FILE",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_BEARER_TOKEN_BEDROCK",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "XAI_API_KEY",
    "GOOGLE_GEMINI_BASE_URL",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "AGY_ACP_CCPA_BASE_URL",
    "AGY_ACP_CCPA_PROJECT",
    "AGY_LLM_GATEWAY_URL",
    "AGY_LLM_GATEWAY_API_KEY",
    "AGY_LLM_GATEWAY_HEADERS",
    "AGY_LLM_GATEWAY_MODELS",
    "AGY_LLM_GATEWAY_PROXY_URL",
    "AGY_LLM_GATEWAY_CA_CERT",
    "AGY_ACP_ENABLE_GATEWAY_AUTH",
    "AGY_GATEWAY_URL",
    "AGY_GATEWAY_API_KEY",
    "AGY_GATEWAY_HEADERS",
    "AGY_CA_CERT",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "OMP_GOOGLE_CREDENTIALS",
];

/// Which wire a managed route speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Messages (`/v1/messages`): Claude Code.
    Anthropic,
    /// Responses (`/v1/responses`): Codex.
    Openai,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::Openai),
            _ => None,
        }
    }

    /// The provider `harness` reaches through Agent Auth, if it has one.
    pub fn for_harness(harness: HarnessId) -> Option<Self> {
        match harness {
            HarnessId::ClaudeCode => Some(Provider::Anthropic),
            HarnessId::Codex => Some(Provider::Openai),
            _ => None,
        }
    }
}

/// A managed run's route: the loopback relay and the token it accepts.
#[derive(Clone, PartialEq, Eq)]
pub struct ManagedRoute {
    pub provider: Provider,
    /// `http://127.0.0.1:<port>`, no path.
    pub base_url: String,
    pub token: String,
}

/// Never prints the token.
impl std::fmt::Debug for ManagedRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedRoute")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl ManagedRoute {
    /// What a managed child's environment gets: the route, and the provider
    /// variables its CLI reads.
    pub fn env(&self) -> BTreeMap<String, String> {
        let base = self.base_url.trim_end_matches('/').to_owned();
        let mut env = BTreeMap::from([
            (ROUTE_ENV.to_owned(), self.provider.as_str().to_owned()),
            (URL_ENV.to_owned(), base.clone()),
            (TOKEN_ENV.to_owned(), self.token.clone()),
        ]);
        let mut set = |key: &str, value: String| {
            env.insert(key.to_owned(), value);
        };
        match self.provider {
            Provider::Anthropic => {
                set("ANTHROPIC_BASE_URL", base);
                set("ANTHROPIC_AUTH_TOKEN", self.token.clone());
                set("CLAUDE_CODE_USE_BEDROCK", "0".into());
                set("CLAUDE_CODE_USE_VERTEX", "0".into());
                set("CLAUDE_CODE_USE_FOUNDRY", "0".into());
                set("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into());
            }
            Provider::Openai => {
                set("OPENAI_BASE_URL", format!("{base}/v1"));
                set("OPENAI_API_KEY", self.token.clone());
            }
        }
        env
    }

    /// The route in a run's environment, if there is one and it's sound: a
    /// known provider, a plain loopback HTTP origin and a token with no
    /// whitespace or control characters.
    pub fn from_env(env: &BTreeMap<String, String>) -> Option<Self> {
        let provider = Provider::parse(env.get(ROUTE_ENV)?)?;
        let base_url = env.get(URL_ENV)?.trim_end_matches('/').to_owned();
        let token = env.get(TOKEN_ENV)?.clone();
        let loopback = reqwest::Url::parse(&base_url).ok().is_some_and(|url| {
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
                && url.port().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none()
        });
        let token_ok = !token.is_empty()
            && !token.chars().any(|c| c.is_control() || c.is_whitespace());
        (loopback && token_ok).then_some(Self {
            provider,
            base_url,
            token,
        })
    }
}

/// Whether a run's environment makes it a managed one (see [`ROUTE_ENV`]).
pub fn managed(env: &BTreeMap<String, String>) -> bool {
    env.contains_key(ROUTE_ENV)
}

/// A managed launch of `harness` needs a sound route for its own provider;
/// anything else must not start. An unmanaged one passes.
pub fn validate(harness: HarnessId, env: &BTreeMap<String, String>) -> Result<(), HarnessError> {
    if !managed(env) {
        return Ok(());
    }
    match (ManagedRoute::from_env(env), Provider::for_harness(harness)) {
        (Some(route), Some(provider)) if route.provider == provider => Ok(()),
        _ => Err(HarnessError::Protocol(
            "This agent can't use the Crew Agent Auth route it was given".into(),
        )),
    }
}

/// [`crate::apply_run_env`]'s first step: a managed child inherits none of
/// [`STRIPPED`]. The run's own environment is applied after, so the route's
/// variables are the ones the child sees.
pub(crate) fn strip_inherited(cmd: &mut Command, env: &BTreeMap<String, String>) {
    if !managed(env) {
        return;
    }
    for key in STRIPPED {
        cmd.env_remove(key);
    }
}

/// Claude Code's flag settings for a managed run: no API key helper, and the
/// console login method, so neither a helper nor the CLI's own claude.ai
/// login can take precedence over the route's token.
pub(crate) fn claude_settings(
    env: &BTreeMap<String, String>,
    settings: &mut serde_json::Map<String, Value>,
) {
    if managed(env) {
        settings.insert("apiKeyHelper".into(), Value::String(String::new()));
        settings.insert("forceLoginMethod".into(), Value::String("console".into()));
    }
}

/// `codex app-server`'s config overrides for a managed run: the `crew` model
/// provider on the relay, its token from [`TOKEN_ENV`], no OpenAI login.
/// Refused when the run is managed but its route isn't sound or isn't
/// OpenAI's.
pub(crate) fn codex_config_args(env: &BTreeMap<String, String>) -> Result<Vec<String>, HarnessError> {
    validate(HarnessId::Codex, env)?;
    let Some(route) = ManagedRoute::from_env(env) else {
        return Ok(Vec::new());
    };
    let base_url = serde_json::to_string(&format!("{}/v1", route.base_url.trim_end_matches('/')))
        .expect("a URL serializes");
    Ok([
        "model_provider=\"crew\"".to_owned(),
        "model_providers.crew.name=\"Crew Agent Auth\"".to_owned(),
        format!("model_providers.crew.base_url={base_url}"),
        format!("model_providers.crew.env_key=\"{TOKEN_ENV}\""),
        "model_providers.crew.wire_api=\"responses\"".to_owned(),
        "model_providers.crew.requires_openai_auth=false".to_owned(),
        "model_providers.crew.supports_websockets=false".to_owned(),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_owned(), setting])
    .collect())
}

/// A managed run's `thread/start` and `thread/resume` parameters: resuming
/// must not restore the rollout's earlier provider (the user's own login).
pub(crate) fn codex_thread_params(
    env: &BTreeMap<String, String>,
    params: &mut serde_json::Map<String, Value>,
) {
    if managed(env) {
        params.insert("modelProvider".into(), "crew".into());
    }
}

/// The models a managed `harness` offers, without launching its CLI: Agent
/// Auth decides at run time whether an account can serve one. `None`: the
/// harness has no managed route.
pub fn managed_models(harness: HarnessId) -> Option<Vec<Model>> {
    match harness {
        HarnessId::ClaudeCode => Some(crate::claude::catalog::static_models()),
        HarnessId::Codex => Some(codex_models()),
        _ => None,
    }
}

/// The model a managed run of `harness` uses when its chat names none.
pub fn managed_default_model(harness: HarnessId) -> Option<String> {
    let models = managed_models(harness)?;
    models
        .iter()
        .find(|model| model.default)
        .or(models.first())
        .map(|model| model.id.clone())
}

/// Crew's managed Codex catalog: the OpenAI rows of its bundled Scaffold
/// catalog (`omp/scaffold-models.json` in Crew), on upstream's row shape.
fn codex_models() -> Vec<Model> {
    use ReasoningLevel::{High, Low, Max, Medium, XHigh};
    const FULL: &[ReasoningLevel] = &[Low, Medium, High, XHigh, Max];
    const SHORT: &[ReasoningLevel] = &[Low, Medium, High];
    let rows: [(&str, &str, &[ReasoningLevel]); 11] = [
        ("gpt-6-sol", "GPT-6-Sol", FULL),
        ("gpt-6.2-sol", "GPT-6.2-Sol", FULL),
        ("gpt-6.1-sol", "GPT-6.1-Sol", FULL),
        ("gpt-6.2-astra", "GPT-6.2-Astra", FULL),
        ("gpt-6-astra", "GPT-6-Astra", SHORT),
        ("gpt-6.1-terra", "GPT-6.1-Terra", SHORT),
        ("gpt-6-luna", "GPT-6-Luna", FULL),
        ("gpt-5.6-sol", "GPT-5.6-Sol", SHORT),
        ("gpt-5.6-terra", "GPT-5.6-Terra", SHORT),
        ("gpt-5.6-luna", "GPT-5.6-Luna", SHORT),
        ("gpt-5.3-codex-spark", "GPT-5.3-Codex-Spark", SHORT),
    ];
    rows.into_iter()
        .map(|(id, label, ladder)| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            provider: None,
            reasoning_levels: ladder.to_vec(),
            options: Vec::new(),
            // Crew's managed Codex default (its relay's `inference_binding`).
            default: id == "gpt-6-sol",
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(provider: Provider) -> ManagedRoute {
        ManagedRoute {
            provider,
            base_url: "http://127.0.0.1:9876".into(),
            token: "opaque-local-token".into(),
        }
    }

    fn envs(cmd: &mut Command) -> BTreeMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn a_route_round_trips_through_the_environment_and_never_prints_its_token() {
        for provider in [Provider::Anthropic, Provider::Openai] {
            let route = route(provider);
            let env = route.env();
            assert_eq!(ManagedRoute::from_env(&env), Some(route.clone()));
            assert!(!format!("{route:?}").contains("opaque-local-token"));
        }
        let anthropic = route(Provider::Anthropic).env();
        assert_eq!(anthropic["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9876");
        assert_eq!(anthropic["ANTHROPIC_AUTH_TOKEN"], "opaque-local-token");
        assert_eq!(anthropic["CLAUDE_CODE_USE_BEDROCK"], "0");
        assert!(!anthropic.contains_key("OPENAI_API_KEY"));
        let openai = route(Provider::Openai).env();
        assert_eq!(openai["OPENAI_BASE_URL"], "http://127.0.0.1:9876/v1");
        assert_eq!(openai[TOKEN_ENV], "opaque-local-token");
        assert!(!openai.contains_key("ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn only_a_sound_loopback_route_fits_its_own_harness() {
        let mut env = route(Provider::Anthropic).env();
        assert!(validate(HarnessId::ClaudeCode, &env).is_ok());
        assert!(validate(HarnessId::Codex, &env).is_err());
        assert!(validate(HarnessId::Pi, &env).is_err());
        for (key, value) in [
            (URL_ENV, "https://127.0.0.1:9876"),
            (URL_ENV, "http://agent-auth.example:9876"),
            (URL_ENV, "http://127.0.0.1:9876/v1"),
            (URL_ENV, "http://user@127.0.0.1:9876"),
            (TOKEN_ENV, ""),
            (TOKEN_ENV, "two words"),
            (ROUTE_ENV, "antigravity"),
        ] {
            let mut bad = env.clone();
            bad.insert(key.into(), value.into());
            assert!(ManagedRoute::from_env(&bad).is_none(), "{key}={value}");
            assert!(validate(HarnessId::ClaudeCode, &bad).is_err(), "{key}={value}");
        }
        // Managed is decided by the marker alone: a broken route still strips.
        env.remove(URL_ENV);
        assert!(managed(&env) && ManagedRoute::from_env(&env).is_none());
        // Unmanaged launches pass untouched.
        assert!(validate(HarnessId::Pi, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn a_managed_launch_drops_inherited_credentials_and_keeps_the_route() {
        let mut cmd = Command::new("claude");
        // As if inherited: set on the command before the run's env.
        cmd.env("ANTHROPIC_API_KEY", "inherited-secret")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "inherited-secret")
            .env("OPENAI_API_KEY", "inherited-secret")
            .env("AWS_SECRET_ACCESS_KEY", "inherited-secret")
            .env("CLAUDE_CODE_FORWARD_SUBAGENT_TEXT", "1");
        let mut env = route(Provider::Anthropic).env();
        env.insert("ZERON_ORIGIN".into(), "chat".into());
        crate::apply_run_env(&mut cmd, &env);
        let seen = envs(&mut cmd);
        for key in ["CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY", "AWS_SECRET_ACCESS_KEY", "ANTHROPIC_API_KEY"] {
            assert_eq!(seen.get(key), Some(&None), "{key} must be removed");
        }
        assert_eq!(seen["ANTHROPIC_AUTH_TOKEN"].as_deref(), Some("opaque-local-token"));
        assert_eq!(seen["ANTHROPIC_BASE_URL"].as_deref(), Some("http://127.0.0.1:9876"));
        assert_eq!(seen["ZERON_ORIGIN"].as_deref(), Some("chat"));
        // The driver's own settings stay.
        assert_eq!(seen["CLAUDE_CODE_FORWARD_SUBAGENT_TEXT"].as_deref(), Some("1"));
        assert!(!seen.values().flatten().any(|value| value == "inherited-secret"));
    }

    #[test]
    fn an_unmanaged_launch_is_untouched() {
        let mut cmd = Command::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "the-users-own");
        let env = BTreeMap::from([("GH_TOKEN".to_owned(), "gh".to_owned())]);
        crate::apply_run_env(&mut cmd, &env);
        let seen = envs(&mut cmd);
        assert_eq!(seen["ANTHROPIC_API_KEY"].as_deref(), Some("the-users-own"));
        assert_eq!(seen["GH_TOKEN"].as_deref(), Some("gh"));
        assert!(!seen.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        let mut settings = serde_json::Map::new();
        claude_settings(&env, &mut settings);
        assert!(settings.is_empty());
        assert!(codex_config_args(&env).unwrap().is_empty());
        let mut params = serde_json::Map::new();
        codex_thread_params(&env, &mut params);
        assert!(params.is_empty());
    }

    /// The real child sees it: run `env` with the parent's environment set as
    /// an inheriting shell would, through the same seam.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_managed_child_process_inherits_no_provider_credential() {
        let mut cmd = Command::new("/usr/bin/env");
        cmd.env("ANTHROPIC_API_KEY", "inherited-secret")
            .env("OPENAI_API_KEY", "inherited-secret")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "inherited-secret");
        crate::apply_run_env(&mut cmd, &route(Provider::Openai).env());
        let output = cmd.output().await.unwrap();
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(!printed.contains("inherited-secret"), "{printed}");
        assert!(printed.lines().any(|line| line == "OPENAI_API_KEY=opaque-local-token"));
        assert!(printed.lines().any(|line| line == "OPENAI_BASE_URL=http://127.0.0.1:9876/v1"));
        assert!(!printed.lines().any(|line| line.starts_with("ANTHROPIC_API_KEY=")));
    }

    #[test]
    fn managed_claude_and_codex_launches_never_use_their_own_logins() {
        let anthropic = route(Provider::Anthropic).env();
        let mut settings = serde_json::Map::new();
        claude_settings(&anthropic, &mut settings);
        assert_eq!(settings["apiKeyHelper"], "");
        assert_eq!(settings["forceLoginMethod"], "console");

        let openai = route(Provider::Openai).env();
        let args = codex_config_args(&openai).unwrap();
        assert!(args.chunks(2).all(|pair| pair[0] == "-c"));
        let settings: Vec<&str> = args.chunks(2).map(|pair| pair[1].as_str()).collect();
        assert!(settings.contains(&"model_provider=\"crew\""));
        assert!(settings.contains(&"model_providers.crew.base_url=\"http://127.0.0.1:9876/v1\""));
        assert!(settings.contains(&"model_providers.crew.env_key=\"COMET_INFERENCE_TOKEN\""));
        assert!(settings.contains(&"model_providers.crew.requires_openai_auth=false"));
        assert!(!args.iter().any(|arg| arg.contains("opaque-local-token")), "the token never rides argv");
        let mut params = serde_json::Map::new();
        codex_thread_params(&openai, &mut params);
        assert_eq!(params["modelProvider"], "crew");
        // Codex refuses another provider's route rather than run on its login.
        assert!(codex_config_args(&anthropic).is_err());
    }

    #[test]
    fn managed_catalogs_are_offline_and_name_a_default() {
        let claude = managed_models(HarnessId::ClaudeCode).unwrap();
        assert!(!claude.is_empty());
        assert_eq!(managed_default_model(HarnessId::ClaudeCode).as_deref(), Some("claude-opus-5-5"));
        let codex = managed_models(HarnessId::Codex).unwrap();
        assert!(codex.iter().all(|model| !model.id.contains('/')));
        assert_eq!(managed_default_model(HarnessId::Codex).as_deref(), Some("gpt-6-sol"));
        assert!(managed_models(HarnessId::Cursor).is_none());
        assert!(managed_default_model(HarnessId::Opencode).is_none());
    }
}
