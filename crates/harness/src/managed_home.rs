//! Persistent, credential-isolated home directories for Crew-managed harnesses.
//!
//! Some harnesses load personal OAuth tokens implicitly from a home directory
//! (for example Antigravity's `GEMINI_HOME/antigravity-acp/acp_token.json`)
//! and also store resumable session databases under that same directory.
//! Pointing `GEMINI_HOME` at a per-process [`ScratchDir`] isolates credentials
//! but deletes the session database whenever the child exits, breaking
//! `session/resume` on the next spawn.
//!
//! [`ManagedHome`] separates those two lifecycles:
//! - When [`RunContext::managed_state_root`] is provided (normal engine runs),
//!   each harness gets a persistent, private (`0700`) directory under that
//!   profile-scoped root (`<managed_state_root>/<dir_name>`). Session state
//!   survives idle reaps, model/account switches, and engine restarts while
//!   staying strictly isolated from the host's personal home.
//! - When `managed_state_root` is `None` (one-shot probes or standalone tests),
//!   [`ManagedHome`] falls back to a disposable [`ScratchDir`] cleaned up on
//!   drop.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use zeron_proto::HarnessId;

use crate::process::Command;
use crate::scratch::ScratchDir;
use crate::{HarnessError, RunContext};

/// Declarative specification for a harness whose Crew-managed runs require an
/// isolated home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedHomeSpec {
    /// Single-component subdirectory under `RunContext::managed_state_root`.
    pub dir_name: &'static str,
    /// Environment variable directing the harness to this home directory.
    pub env_var: &'static str,
    /// Relative subdirectories created inside the managed home before launch.
    pub subdirs: &'static [&'static str],
    /// Relative paths inside the managed home that must never exist (for
    /// example personal OAuth token files).
    pub forbidden_paths: &'static [&'static str],
}

/// Look up the managed-home specification for a harness, if it needs one.
pub(crate) fn spec_for(harness: HarnessId) -> Option<ManagedHomeSpec> {
    match harness {
        HarnessId::Antigravity => Some(ManagedHomeSpec {
            dir_name: "antigravity",
            env_var: "GEMINI_HOME",
            subdirs: &["antigravity-acp"],
            forbidden_paths: &["antigravity-acp/acp_token.json"],
        }),
        _ => None,
    }
}

/// Whether this launch uses a managed isolated home instead of the host home.
pub(crate) fn uses_managed_home(harness: HarnessId, context: Option<&RunContext>) -> bool {
    context
        .and_then(|value| value.inference.as_ref())
        .is_some()
        && spec_for(harness).is_some()
}

/// Guard for a prepared managed harness home.
///
/// Persistent homes (`_ephemeral: None`) remain on disk when dropped so later
/// spawns can resume harness-native sessions. Ephemeral fallback homes
/// (`_ephemeral: Some(_)`) are removed when dropped, so callers must drop this
/// guard after reaping the child process.
#[derive(Debug)]
pub(crate) struct ManagedHome {
    path: PathBuf,
    env_var: &'static str,
    _ephemeral: Option<ScratchDir>,
}

impl ManagedHome {
    /// Prepare the managed home for `harness` when running with a managed
    /// inference route, or return `Ok(None)` if this run does not use one.
    pub(crate) fn prepare(
        harness: HarnessId,
        context: Option<&RunContext>,
    ) -> Result<Option<Self>, HarnessError> {
        let Some(context) = context.filter(|value| value.inference.is_some()) else {
            return Ok(None);
        };
        let Some(spec) = spec_for(harness) else {
            return Ok(None);
        };
        Self::prepare_with_spec(spec, context.managed_state_root.as_deref()).map(Some)
    }

    pub(crate) fn prepare_with_spec(
        spec: ManagedHomeSpec,
        state_root: Option<&Path>,
    ) -> Result<Self, HarnessError> {
        validate_single_component(spec.dir_name, "managed home directory name")?;
        for subdir in spec.subdirs {
            validate_relative_path(subdir, "managed home subdirectory")?;
        }
        for forbidden in spec.forbidden_paths {
            validate_relative_path(forbidden, "managed home forbidden path")?;
        }

        let (path, ephemeral) = match state_root {
            Some(root) => {
                if !root.is_absolute() {
                    return Err(HarnessError::Protocol(format!(
                        "managed state root must be an absolute path: {}",
                        root.display()
                    )));
                }
                ensure_root_dir(root)?;
                let home = root.join(spec.dir_name);
                ensure_private_dir(&home)?;
                (home, None)
            }
            None => {
                let scratch = ScratchDir::new(&format!("{}-managed", spec.dir_name))?;
                let home = scratch.path().join(spec.dir_name);
                ensure_private_dir(&home)?;
                (home, Some(scratch))
            }
        };

        for subdir in spec.subdirs {
            ensure_relative_subdirs(&path, Path::new(subdir))?;
        }
        for forbidden in spec.forbidden_paths {
            reject_forbidden_path(&path, Path::new(forbidden))?;
        }

        Ok(Self {
            path,
            env_var: spec.env_var,
            _ephemeral: ephemeral,
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn is_persistent(&self) -> bool {
        self._ephemeral.is_none()
    }

    /// Export this home directory's environment variable onto `cmd`, overriding
    /// any host or per-run value.
    pub(crate) fn apply(&self, cmd: &mut Command) {
        cmd.env(self.env_var, &self.path);
    }
}

/// Prepare and apply a harness's managed home (if any) onto `cmd`.
///
/// Call after [`crate::apply_run_env`] and [`crate::apply_run_context`] so the
/// managed home always outranks host and request environment overlays.
pub(crate) fn apply_managed_home(
    harness: HarnessId,
    cmd: &mut Command,
    context: Option<&RunContext>,
) -> Result<Option<ManagedHome>, HarnessError> {
    let home = ManagedHome::prepare(harness, context)?;
    if let Some(home) = &home {
        home.apply(cmd);
    }
    Ok(home)
}

fn validate_single_component(value: &str, label: &str) -> Result<(), HarnessError> {
    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(HarnessError::Protocol(format!(
            "invalid {label}: {value:?}"
        ))),
    }
}

fn validate_relative_path(value: &str, label: &str) -> Result<(), HarnessError> {
    let mut count = 0usize;
    for component in Path::new(value).components() {
        match component {
            Component::Normal(_) => count += 1,
            _ => {
                return Err(HarnessError::Protocol(format!(
                    "invalid {label}: {value:?}"
                )));
            }
        }
    }
    if count == 0 {
        return Err(HarnessError::Protocol(format!(
            "empty {label}: {value:?}"
        )));
    }
    Ok(())
}

fn ensure_root_dir(root: &Path) -> Result<(), HarnessError> {
    if let Some(parent) = root.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    ensure_private_dir(root)
}

fn ensure_private_dir(path: &Path) -> Result<(), HarnessError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                return Err(HarnessError::Protocol(format!(
                    "managed harness home directory must not be a symlink: {}",
                    path.display()
                )));
            }
            if !meta.is_dir() {
                return Err(HarnessError::Protocol(format!(
                    "managed harness home path is not a directory: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_relative_subdirs(base: &Path, relative: &Path) -> Result<(), HarnessError> {
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(HarnessError::Protocol(format!(
                "invalid managed home subdirectory: {}",
                relative.display()
            )));
        };
        current.push(segment);
        ensure_private_dir(&current)?;
    }
    Ok(())
}

fn reject_forbidden_path(base: &Path, relative: &Path) -> Result<(), HarnessError> {
    let mut current = base.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(segment) = component else {
            return Err(HarnessError::Protocol(format!(
                "invalid managed home forbidden path: {}",
                relative.display()
            )));
        };
        current.push(segment);
        let is_leaf = index + 1 == components.len();
        match std::fs::symlink_metadata(&current) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(meta) if is_leaf => {
                let _ = meta;
                return Err(HarnessError::Protocol(format!(
                    "managed harness home must not contain credential file: {}",
                    current.display()
                )));
            }
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(HarnessError::Protocol(format!(
                        "managed harness home path must not traverse a symlink: {}",
                        current.display()
                    )));
                }
                if !meta.is_dir() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InferenceRoute;

    fn managed_context(state_root: Option<PathBuf>) -> RunContext {
        RunContext {
            session_id: "session-1".into(),
            ipc_port: 0,
            supervision_token: None,
            inference: Some(InferenceRoute {
                base_url: "http://127.0.0.1:9876".into(),
                token: "opaque-token".into(),
                provider: "antigravity".into(),
                model: "gemini-3.8-flash-high".into(),
            }),
            fork_from: None,
            managed_state_root: state_root,
        }
    }

    #[test]
    fn persistent_home_survives_drop_and_reuses_same_directory() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("managed-homes");
        let context = managed_context(Some(state_root.clone()));

        let session_file = {
            let home = ManagedHome::prepare(HarnessId::Antigravity, Some(&context))
                .unwrap()
                .expect("antigravity uses managed home");
            assert!(home.is_persistent());
            assert_eq!(home.path(), state_root.join("antigravity"));
            let conv_dir = home.path().join("antigravity-acp").join("conversations");
            std::fs::create_dir_all(&conv_dir).unwrap();
            let db = conv_dir.join("sess-123.db");
            std::fs::write(&db, b"session-history").unwrap();
            db
        };

        assert!(
            session_file.is_file(),
            "persistent managed home must survive drop so session/resume works"
        );

        let home_again = ManagedHome::prepare(HarnessId::Antigravity, Some(&context))
            .unwrap()
            .expect("second launch reuses managed home");
        assert_eq!(home_again.path(), state_root.join("antigravity"));
        assert_eq!(std::fs::read(&session_file).unwrap(), b"session-history");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home_again.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn ephemeral_fallback_cleans_up_on_drop_when_state_root_is_none() {
        let context = managed_context(None);
        let dir_path = {
            let home = ManagedHome::prepare(HarnessId::Antigravity, Some(&context))
                .unwrap()
                .expect("antigravity gets ephemeral fallback");
            assert!(!home.is_persistent());
            assert!(home.path().join("antigravity-acp").is_dir());
            home.path().to_path_buf()
        };
        assert!(
            !dir_path.exists(),
            "ephemeral fallback managed home must clean up on drop"
        );
    }

    #[test]
    fn apply_overrides_host_env_and_skips_non_managed_or_unconfigured_harnesses() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("managed-homes");
        let context = managed_context(Some(state_root.clone()));

        let mut cmd = Command::new("agy_acp_server");
        cmd.env("GEMINI_HOME", "/unowned/host-gemini");
        let home = apply_managed_home(HarnessId::Antigravity, &mut cmd, Some(&context))
            .unwrap()
            .expect("managed home applied");
        let expected = state_root.join("antigravity");
        assert!(cmd.as_std_mut().get_envs().any(|(key, value)| {
            key == "GEMINI_HOME" && value == Some(expected.as_os_str())
        }));
        drop(home);

        // No inference route => no managed home.
        let mut non_managed = context.clone();
        non_managed.inference = None;
        assert!(
            ManagedHome::prepare(HarnessId::Antigravity, Some(&non_managed))
                .unwrap()
                .is_none()
        );
        // Harness without a ManagedHomeSpec => no managed home.
        assert!(
            ManagedHome::prepare(HarnessId::Codex, Some(&context))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn generic_spec_supports_any_harness_home_layout() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let spec = ManagedHomeSpec {
            dir_name: "custom-agent",
            env_var: "CUSTOM_AGENT_HOME",
            subdirs: &["nested/sessions", "cache"],
            forbidden_paths: &["nested/token.json"],
        };
        let home = ManagedHome::prepare_with_spec(spec, Some(&state_root)).unwrap();
        assert_eq!(home.path(), state_root.join("custom-agent"));
        assert!(home.path().join("nested/sessions").is_dir());
        assert!(home.path().join("cache").is_dir());

        let mut cmd = Command::new("custom-agent");
        home.apply(&mut cmd);
        assert!(cmd.as_std_mut().get_envs().any(|(key, value)| {
            key == "CUSTOM_AGENT_HOME" && value == Some(home.path().as_os_str())
        }));

        // Placing a forbidden credential file must fail subsequent launches.
        std::fs::write(home.path().join("nested/token.json"), b"secret").unwrap();
        let err = ManagedHome::prepare_with_spec(spec, Some(&state_root)).unwrap_err();
        assert!(err.to_string().contains("credential file"));
    }

    #[test]
    fn rejects_relative_state_root() {
        let err = ManagedHome::prepare_with_spec(
            spec_for(HarnessId::Antigravity).unwrap(),
            Some(Path::new("relative/managed-homes")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_managed_home_or_subdir() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let state_root = root.path().join("managed-homes");
        std::fs::create_dir_all(&state_root).unwrap();
        std::os::unix::fs::symlink(external.path(), state_root.join("antigravity")).unwrap();

        let err = ManagedHome::prepare_with_spec(
            spec_for(HarnessId::Antigravity).unwrap(),
            Some(&state_root),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }
}
