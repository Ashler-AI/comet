use std::path::{Path, PathBuf};

use crate::HarnessError;

pub(crate) const EXTENSION_SOURCE: &str = include_str!("prime_agent_auth_gateway.ts");

pub(crate) fn install_extension(agent_dir: &Path) -> Result<Option<PathBuf>, HarnessError> {
    let discovered = agent_dir.join("extensions/omp-auth-gateway.ts");
    if discovered.is_file() {
        return Ok(None);
    }

    let directory = agent_dir.join("comet-runtime");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("agent-auth-gateway.ts");
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(EXTENSION_SOURCE.as_bytes()) {
        std::fs::write(&path, EXTENSION_SOURCE)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(Some(path))
}
pub(crate) fn provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("comet-openai"),
        "anthropic" => Some("comet-anthropic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_a_credential_free_loopback_gateway_adapter() {
        assert!(EXTENSION_SOURCE.contains("OMP_AUTH_GATEWAY_TOKEN"));
        assert!(EXTENSION_SOURCE.contains("http://127.0.0.1:4000"));
        assert!(!EXTENSION_SOURCE.contains("sk-"));
        assert!(!EXTENSION_SOURCE.contains("refreshToken"));
    }

    #[test]
    fn pins_gateway_runs_to_the_granted_provider() {
        assert_eq!(provider("openai"), Some("comet-openai"));
        assert_eq!(provider("anthropic"), Some("comet-anthropic"));
        assert_eq!(provider("prime-inference"), None);
    }
}
