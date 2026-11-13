//! Crew: what an engine of this build must not inherit (docs/crew/zgx-thin-port.md).
//!
//! An app or agent shell started by another Crew carries that Crew's
//! coordinates and credentials: its data dir and IPC port (`ZERON_*`,
//! `COMET_*`), its control plane (`CREW_*`), its OMP gateway token, and the
//! agent session's own inference route (`ANTHROPIC_*`, `CLAUDE_CODE_*`).
//! Handed on to this build's engine, they would point it, and every agent it
//! runs, at that other install. The caller sets the engine's own values after
//! [`scrub_inherited`].

use std::process::Command;

/// Prefixes of inherited variables that steer an engine or carry credentials.
const PREFIXES: &[&str] = &["ZERON_", "COMET_", "CREW_", "OMP_", "CLAUDE_CODE_"];

/// Test knobs under those prefixes that steer nothing and may pass.
const KEEP: &[&str] = &["ZERON_MOCK_", "ZERON_DISABLE_NOTIFICATIONS"];

/// Single variables of the same kind.
const NAMES: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
    "__CFBundleIdentifier",
];

/// Whether this process's variable `name` must not reach the engine.
pub fn is_inherited_steering(name: &str) -> bool {
    NAMES.contains(&name)
        || (PREFIXES.iter().any(|prefix| name.starts_with(prefix))
            && !KEEP.iter().any(|keep| name.starts_with(keep)))
}

/// Removes from `command` every variable of this process's that
/// [`is_inherited_steering`].
pub fn scrub_inherited(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(is_inherited_steering) {
            command.env_remove(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_crews_coordinates_and_credentials_are_dropped() {
        for name in [
            "ZERON_DATA_DIR",
            "ZERON_IPC_PORT",
            "ZERON_MANAGED_ENGINE",
            "ZERON_APP",
            "COMET_DATA_DIR",
            "COMET_INFERENCE_TOKEN",
            "COMET_SUPERVISION_TOKEN",
            "CREW_CONTROL_ORIGIN",
            "OMP_AUTH_GATEWAY_TOKEN",
            "CLAUDE_CODE_SESSION_ID",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "__CFBundleIdentifier",
        ] {
            assert!(is_inherited_steering(name), "{name}");
        }
    }

    #[test]
    fn ordinary_variables_and_test_knobs_pass() {
        for name in [
            "HOME",
            "PATH",
            "TMPDIR",
            "LANG",
            "RUST_LOG",
            "ANTHROPIC_API_KEY",
            "ZERON_MOCK_DELAY_MS",
            "ZERON_DISABLE_NOTIFICATIONS",
        ] {
            assert!(!is_inherited_steering(name), "{name}");
        }
    }
}
