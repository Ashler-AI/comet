//! Server-enforced Comet runtime capability profiles.
//!
//! A Scaffold host is not a local desktop with hidden buttons: its engine rejects
//! local-only harnesses, recursive environment control, and session import.

use serde::{Deserialize, Serialize};

use crate::HarnessId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeProfile {
    /// Native desktop/headless install on a developer-owned machine.
    LocalController,
    /// Deployment-bound engine bootstrapped inside a Scaffold sandbox.
    ScaffoldHost,
    /// Deterministic offline testing only.
    Mock,
}

impl RuntimeProfile {
    pub fn allows_harness(self, harness: HarnessId) -> bool {
        match self {
            Self::LocalController => matches!(
                harness,
                HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Omp
            ),
            Self::ScaffoldHost => harness == HarnessId::Omp,
            Self::Mock => harness == HarnessId::Mock,
        }
    }

    pub fn allows_scaffold_control(self) -> bool {
        self == Self::LocalController
    }

    pub fn allows_session_import(self) -> bool {
        self == Self::LocalController
    }

    pub fn allows_agent_accounts(self) -> bool {
        self == Self::LocalController
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_host_allows_only_omp_and_no_recursive_controller_features() {
        assert!(RuntimeProfile::ScaffoldHost.allows_harness(HarnessId::Omp));
        assert!(!RuntimeProfile::ScaffoldHost.allows_harness(HarnessId::ClaudeCode));
        assert!(!RuntimeProfile::ScaffoldHost.allows_harness(HarnessId::Codex));
        assert!(!RuntimeProfile::ScaffoldHost.allows_harness(HarnessId::Mock));
        assert!(!RuntimeProfile::ScaffoldHost.allows_scaffold_control());
        assert!(!RuntimeProfile::ScaffoldHost.allows_session_import());
        assert!(!RuntimeProfile::ScaffoldHost.allows_agent_accounts());
    }

    #[test]
    fn local_controller_can_use_real_local_harnesses_and_scaffold() {
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Omp] {
            assert!(RuntimeProfile::LocalController.allows_harness(harness));
        }
        assert!(!RuntimeProfile::LocalController.allows_harness(HarnessId::Mock));
        assert!(RuntimeProfile::LocalController.allows_scaffold_control());
        assert!(RuntimeProfile::LocalController.allows_session_import());
        assert!(RuntimeProfile::LocalController.allows_agent_accounts());
    }
}
