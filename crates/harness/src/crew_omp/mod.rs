//! Crew: OMP (oh-my-pi), Crew's build of the Pi coding agent, driven over its
//! RPC mode (docs/crew/zgx-thin-port.md, "OMP"). Ported from Crew's
//! `crates/harness/src/omp/` (Ashler-AI/comet), trimmed to what a managed turn
//! needs. Upstream's Pi driver can't drive it: OMP's flags, commands and turn
//! end differ.

use std::path::PathBuf;

use crate::HarnessError;

/// Where the `omp` CLI is: `OMP_EXECUTABLE`, else the usual installs.
pub(crate) fn resolve_executable(executable: Option<PathBuf>) -> Result<PathBuf, HarnessError> {
    if let Some(path) = executable.or_else(|| std::env::var_os("OMP_EXECUTABLE").map(PathBuf::from)) {
        return crate::executable::validate_native_override(&path);
    }
    let home = crate::executable::home_or_current_dir();
    crate::executable::find_on_paths(
        "omp",
        vec![
            home.join(".local/bin/omp"),
            home.join(".bun/bin/omp"),
            PathBuf::from("/opt/homebrew/bin/omp"),
            PathBuf::from("/usr/local/bin/omp"),
        ],
    )
    .ok_or_else(|| HarnessError::NotInstalled("OMP CLI: install OMP or set OMP_EXECUTABLE".into()))
}

/// OMP's harness (its driver lands here: the plan's step 2).
#[derive(Debug, Default, Clone)]
pub struct OmpHarness {
    executable: Option<PathBuf>,
}

impl OmpHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// A given `omp`, not the one found.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        resolve_executable(self.executable.clone())
    }

    pub fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
}
