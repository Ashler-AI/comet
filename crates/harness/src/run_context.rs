use crate::{HarnessError, RunContext, process::Command};
use zeron_proto::HarnessId;

pub(crate) fn validate_inference_route(
    harness: HarnessId,
    context: Option<&RunContext>,
) -> Result<(), HarnessError> {
    let Some(route) = context.and_then(|context| context.inference.as_ref()) else {
        return Ok(());
    };
    let compatible = matches!((harness, route.provider.as_str()),
        (HarnessId::Omp, "openai" | "anthropic" | "antigravity")
        | (HarnessId::Codex, "openai")
        | (HarnessId::ClaudeCode, "anthropic")
        | (HarnessId::Antigravity, "antigravity"));    let url = reqwest::Url::parse(&route.base_url).ok();
    let loopback = url.as_ref().is_some_and(|url| {
        url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if !compatible
        || !loopback
        || route.token.is_empty()
        || route.model.is_empty()
        || route.token.chars().any(char::is_control)
    {
        return Err(HarnessError::Protocol(
            "This harness cannot use the supplied Crew Agent Auth route".into(),
        ));
    }
    Ok(())
}

pub(crate) fn apply_run_context(cmd: &mut Command, context: Option<&RunContext>) {
    // Child identity always comes from the host, never an inherited nested run.
    for key in [
        "COMET_SESSION_ID",
        "COMET_IPC_PORT",
        "COMET_SUPERVISION_TOKEN",
        "COMET_INFERENCE_TOKEN",
        "PRIME_AGENT_AUTH_GATEWAY_URL",
        "OMP_AUTH_GATEWAY_URL",
        "OMP_AUTH_GATEWAY_TOKEN",
    ] {
        cmd.env_remove(key);
    }
    let Some(context) = context else { return };
    cmd.env("COMET_SESSION_ID", &context.session_id)
        .env("COMET_IPC_PORT", context.ipc_port.to_string());
    if let Some(token) = &context.supervision_token {
        cmd.env("COMET_SUPERVISION_TOKEN", token);
    }
    let Some(route) = &context.inference else {
        return;
    };
    // The opaque loopback token is the only provider authority a managed child gets.
    // CLI config overrides below also prevent on-disk OAuth credentials taking precedence.
    for key in [
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
    ] {
        cmd.env_remove(key);
    }        cmd.env_remove(key);
    }
    cmd.env("COMET_INFERENCE_TOKEN", &route.token)
        .env("OMP_AUTH_GATEWAY_URL", route.base_url.trim_end_matches('/'))
        .env("OMP_AUTH_GATEWAY_TOKEN", &route.token);
    match route.provider.as_str() {
        "openai" => {
            cmd.env(
                "OPENAI_BASE_URL",
                format!("{}/v1", route.base_url.trim_end_matches('/')),
            )
            .env("OPENAI_API_KEY", &route.token);
        }
        "antigravity" => {
            // Official discovery/harness HTTP clients otherwise inherit proxy
            // routing, which must not carry the private loopback capability.
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
                cmd.env_remove(key);
            }
            cmd.env("AGY_ACP_ENABLE_GATEWAY_AUTH", "1")
                .env("AGY_ACP_FORCE_FILE_STORAGE", "1");
        }
        "anthropic" => {
            cmd.env("ANTHROPIC_BASE_URL", route.base_url.trim_end_matches('/'))
                .env("ANTHROPIC_AUTH_TOKEN", &route.token)
                .env("CLAUDE_CODE_USE_BEDROCK", "0")
                .env("CLAUDE_CODE_USE_VERTEX", "0")
                .env("CLAUDE_CODE_USE_FOUNDRY", "0")
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InferenceRoute;

    #[test]
    fn managed_route_rejects_unsupported_harness_and_clears_inherited_credentials() {
        let context = RunContext {
            session_id: "session".into(),
            ipc_port: 1234,
            fork_from: None,
            supervision_token: Some("exact-run-capability".into()),
            inference: Some(InferenceRoute {
                base_url: "http://127.0.0.1:9876".into(),
                token: "opaque-local-token".into(),
                provider: "anthropic".into(),
                model: "claude".into(),
            }),
        };
        assert!(validate_inference_route(HarnessId::ClaudeCode, Some(&context)).is_ok());
        assert!(validate_inference_route(HarnessId::Pi, Some(&context)).is_err());
        assert!(validate_inference_route(HarnessId::Codex, Some(&context)).is_err());
        let mut cmd = Command::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "must-not-survive");
        apply_run_context(&mut cmd, Some(&context));
        let env: std::collections::BTreeMap<_, _> = cmd.as_std_mut().get_envs().collect();
        assert_eq!(
            env.get(std::ffi::OsStr::new("ANTHROPIC_API_KEY")),
            Some(&None)
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("ANTHROPIC_AUTH_TOKEN")),
            Some(&Some(std::ffi::OsStr::new("opaque-local-token")))
        );
        assert!(!format!("{context:?}").contains("opaque-local-token"));
        assert!(!format!("{context:?}").contains("exact-run-capability"));
        assert_eq!(
            env.get(std::ffi::OsStr::new("COMET_SUPERVISION_TOKEN")),
            Some(&Some(std::ffi::OsStr::new("exact-run-capability")))
        );
        apply_run_context(&mut cmd, None);
        assert!(
            cmd.as_std_mut()
                .get_envs()
                .any(|(key, value)| key == "COMET_SESSION_ID" && value.is_none())
        );
        assert!(
            cmd.as_std_mut()
                .get_envs()
                .any(|(key, value)| key == "COMET_SUPERVISION_TOKEN" && value.is_none())
        );
    }
}
