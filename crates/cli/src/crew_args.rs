//! Crew: the runtime coordinates a command line may name (docs/crew/zgx-thin-port.md,
//! "Scaffold sandboxes"), as Crew's own command line takes them. Scaffold's native
//! runner starts a sandbox host with exactly
//!
//! ```text
//! comet --data-dir <root>/.scaffold/crew-data --ipc-port 39400 headless \
//!   --device-bootstrap-file F --edge-url E --scaffold-url S --project-scope P --deployment-id D
//! ```
//!
//! and the desktop's attach probe runs
//! `comet --data-dir .scaffold/crew-data --ipc-port 39400 scaffold-authority`.
//!
//! The options are global, so they may stand before or after the subcommand.
//! `--data-dir` and `--ipc-port` replace `ZERON_DATA_DIR`, `ZERON_IPC_PORT` and
//! the flavour's defaults wherever this process resolves them
//! ([`crate::paths::data_dir`], [`crate::ipc_port_from_env`]: logs included),
//! and the flavour's refusals still apply to them. The authority options and
//! the bootstrap are the engine configuration's to read
//! (`apps/zeron/src/crew_cli.rs`); nothing here trusts them.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The runtime a command acts on, before or after its subcommand.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct RuntimeArgs {
    /// The engine's data folder (instead of ZERON_DATA_DIR or the default).
    #[arg(long, global = true, value_name = "DIR", value_parser = absolute_path)]
    pub data_dir: Option<PathBuf>,
    /// The engine's local IPC port (instead of ZERON_IPC_PORT or the default).
    #[arg(long, global = true, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub ipc_port: Option<u16>,
    /// Crew: the Edge of the Scaffold authority below (an https origin).
    #[arg(long, global = true, value_name = "ORIGIN")]
    pub edge_url: Option<String>,
    /// Crew: the Scaffold authority this runtime signs in with (an https origin).
    #[arg(long, global = true, value_name = "ORIGIN")]
    pub scaffold_url: Option<String>,
    /// Crew: the Scaffold project.
    #[arg(long, global = true, value_name = "ID")]
    pub project_scope: Option<String>,
    /// Crew: the Scaffold deployment.
    #[arg(long, global = true, value_name = "ID")]
    pub deployment_id: Option<String>,
}

impl RuntimeArgs {
    /// Whether the command line names a Scaffold authority (any part of one).
    pub fn names_authority(&self) -> bool {
        [&self.edge_url, &self.scaffold_url, &self.project_scope, &self.deployment_id]
            .iter()
            .any(|value| value.is_some())
    }
}

/// `headless`'s own options.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct HeadlessArgs {
    /// Crew: run as the Scaffold sandbox host that this private, one-use
    /// device bootstrap names. It is removed once its grant is exchanged.
    #[arg(long, value_name = "FILE", value_parser = absolute_path)]
    pub device_bootstrap_file: Option<PathBuf>,
}

/// What this process's command line named.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub runtime: RuntimeArgs,
    /// `headless --device-bootstrap-file`.
    pub device_bootstrap_file: Option<PathBuf>,
}

static LAUNCH: OnceLock<Launch> = OnceLock::new();

/// Record the command line's launch ([`crate::run`], once it's parsed).
pub(crate) fn install(launch: Launch) {
    let _ = LAUNCH.set(launch);
}

/// This process's launch (nothing named before [`crate::run`] parsed one).
pub fn launch() -> Launch {
    LAUNCH.get().cloned().unwrap_or_default()
}

/// `--data-dir`, when the command line named one.
pub fn data_dir() -> Option<PathBuf> {
    LAUNCH.get().and_then(|launch| launch.runtime.data_dir.clone())
}

/// `--ipc-port`, when the command line named one.
pub fn ipc_port() -> Option<u16> {
    LAUNCH.get().and_then(|launch| launch.runtime.ipc_port)
}

/// A path as given, made absolute against the current folder (an agent's or
/// the probe's `.scaffold/crew-data` is the workspace's).
fn absolute_path(text: &str) -> Result<PathBuf, String> {
    if text.trim().is_empty() {
        return Err("give a path".into());
    }
    std::path::absolute(text).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use clap::FromArgMatches as _;

    use super::*;
    use crate::{Cli, Command, command};

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let argv = std::iter::once("comet").chain(args.iter().copied());
        Cli::from_arg_matches(&command("0.0.0").try_get_matches_from(argv)?)
    }

    fn launch_of(cli: &Cli) -> Launch {
        crate::launch_of(cli)
    }

    /// Exactly what Platform's native runner starts (native-crew-runner.mjs):
    /// the data dir and port before `headless`, the bootstrap and the
    /// authority after it.
    #[test]
    fn the_native_runner_s_launch_parses() {
        let cli = parse(&[
            "--data-dir", "/workspace/ashler-platform/.scaffold/crew-data", "--ipc-port", "39400", "headless",
            "--device-bootstrap-file", "/workspace/.scaffold/native-crew/enrollment-1.json",
            "--edge-url", "https://crew-zgx-edge-staging.cgoggins.workers.dev",
            "--scaffold-url", "https://scaffold-staging.internal.ashler.com",
            "--project-scope", "ashler-staging", "--deployment-id", "ashler-staging",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Headless(_))));
        let launch = launch_of(&cli);
        assert_eq!(launch.runtime.data_dir, Some(PathBuf::from("/workspace/ashler-platform/.scaffold/crew-data")));
        assert_eq!(launch.runtime.ipc_port, Some(39400));
        assert_eq!(launch.runtime.edge_url.as_deref(), Some("https://crew-zgx-edge-staging.cgoggins.workers.dev"));
        assert_eq!(launch.runtime.scaffold_url.as_deref(), Some("https://scaffold-staging.internal.ashler.com"));
        assert_eq!(launch.runtime.project_scope.as_deref(), Some("ashler-staging"));
        assert_eq!(launch.runtime.deployment_id.as_deref(), Some("ashler-staging"));
        assert_eq!(
            launch.device_bootstrap_file,
            Some(PathBuf::from("/workspace/.scaffold/native-crew/enrollment-1.json"))
        );
        assert!(launch.runtime.names_authority());
        // Its restart: the same, without the bootstrap.
        let restart = parse(&[
            "--data-dir", "/workspace/ashler-platform/.scaffold/crew-data", "--ipc-port", "39400", "headless",
            "--edge-url", "https://edge.example", "--scaffold-url", "https://scaffold.example",
            "--project-scope", "p", "--deployment-id", "d",
        ])
        .unwrap();
        assert_eq!(launch_of(&restart).device_bootstrap_file, None);
        // The options are global: before the subcommand too.
        let before = parse(&["--scaffold-url", "https://scaffold.example", "--project-scope", "p", "headless"]).unwrap();
        assert_eq!(launch_of(&before).runtime.project_scope.as_deref(), Some("p"));
    }

    /// The desktop's attach probe (Crew's `remote_argv("scaffold-authority")`),
    /// with the workspace-relative data dir made the workspace's.
    #[test]
    fn the_attach_probe_parses_with_its_relative_data_dir() {
        let cli = parse(&["--data-dir", ".scaffold/crew-data", "--ipc-port", "39400", "scaffold-authority"]).unwrap();
        assert!(matches!(cli.command, Some(Command::ScaffoldAuthority)));
        assert!(cli.command.as_ref().unwrap().runs_engine());
        let launch = launch_of(&cli);
        assert_eq!(
            launch.runtime.data_dir,
            Some(std::env::current_dir().unwrap().join(".scaffold/crew-data"))
        );
        assert!(!launch.runtime.names_authority());
    }

    #[test]
    fn what_isn_t_a_runtime_is_refused() {
        for args in [
            &["--ipc-port", "0", "headless"][..],
            &["--ipc-port", "65536", "headless"],
            &["--data-dir", "", "headless"],
            // The bootstrap is headless's alone.
            &["--device-bootstrap-file", "/tmp/b.json", "status"],
            &["status", "--device-bootstrap-file", "/tmp/b.json"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        // No runtime named: nothing changes.
        let plain = launch_of(&parse(&["headless"]).unwrap());
        assert_eq!(plain, Launch::default());
    }
}
