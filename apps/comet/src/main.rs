//! comet — headed by default; `comet headless` runs the engine alone. Auth is
//! decoupled from the daemon: `comet login` persists the session and exits, so a
//! service-managed `comet headless` only ever loads saved credentials.

mod auth_cli;
mod daemon;
mod session_cli;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "comet", about = "Multi-device controller for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run without the desktop app.
    Headless(HeadlessArgs),
    /// Sign in (paste-code flow), persist the session, and exit.
    Login,
    /// Remove the saved session.
    Logout,
    /// Show auth + engine status (exits nonzero when a sign-in is needed).
    Status,
    #[command(hide = true)]
    ScaffoldAuthority,
    /// Live sync introspection from the running engine: per-room connection
    /// state, last pushed-frame/ack ages, rejoin/probe/resync counters.
    Sync,
    /// Import, inspect, and message global Comet sessions.
    Session {
        #[command(subcommand)]
        command: session_cli::SessionCommand,
    },
    /// Manage `comet headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::Args)]
struct HeadlessArgs {
    /// Mode-0600, single-use JSON bootstrap file written by Scaffold.
    #[arg(long)]
    device_bootstrap_file: Option<std::path::PathBuf>,
    #[arg(long)]
    edge_url: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceBootstrapFile {
    device_join_grant: String,
    project_id: String,
    deployment_id: String,
    session_id: String,
    device_id: String,
    lifecycle_epoch: u64,
    sandbox_id: String,
}

impl HeadlessArgs {
    fn into_bootstrap(self) -> anyhow::Result<Option<comet_engine::DeviceBootstrapConfig>> {
        let Some(path) = self.device_bootstrap_file else {
            return Ok(None);
        };
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            anyhow::bail!("device_join_grant_unavailable");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                anyhow::bail!("device_join_grant_unavailable");
            }
        }
        let bytes = std::fs::read(&path);
        // A bootstrap credential is single use. Remove it before parsing or
        // making any network request, including on malformed input.
        let _ = std::fs::remove_file(&path);
        let bootstrap: DeviceBootstrapFile = serde_json::from_slice(&bytes?)
            .map_err(|_| anyhow::anyhow!("device_join_grant_unavailable"))?;
        Ok(Some(comet_engine::DeviceBootstrapConfig {
            device_join_grant: bootstrap.device_join_grant,
            project_id: bootstrap.project_id,
            deployment_id: bootstrap.deployment_id,
            session_id: bootstrap.session_id,
            device_id: bootstrap.device_id,
            lifecycle_epoch: bootstrap.lifecycle_epoch,
            sandbox_id: bootstrap.sandbox_id,
        }))
    }
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service with approved non-secret overrides.
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

const STAGING_EDGE_URL: &str = "https://comet-staging.internal.ashler.com";
const PRODUCTION_EDGE_URL: &str = "https://comet.internal.ashler.com";
const STAGING_SCAFFOLD_URL: &str = "https://scaffold-staging.internal.ashler.com";
const PRODUCTION_SCAFFOLD_URL: &str = "https://scaffold.internal.ashler.com";
const STAGING_PROJECT_SCOPE: &str = "ashler-staging";
const PRODUCTION_PROJECT_SCOPE: &str = "ashler-production";

fn release_defaults() -> (&'static str, &'static str, &'static str) {
    if option_env!("COMET_DEFAULT_ENVIRONMENT") == Some("production") {
        (
            PRODUCTION_EDGE_URL,
            PRODUCTION_SCAFFOLD_URL,
            PRODUCTION_PROJECT_SCOPE,
        )
    } else {
        (
            STAGING_EDGE_URL,
            STAGING_SCAFFOLD_URL,
            STAGING_PROJECT_SCOPE,
        )
    }
}

fn edge_url_from_env() -> String {
    std::env::var("COMET_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| release_defaults().0.into())
}

fn scaffold_url_from_env(edge_token: &Option<String>) -> Option<String> {
    if edge_token.is_some() {
        return None;
    }
    std::env::var("COMET_SCAFFOLD_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| Some(release_defaults().1.into()))
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (docs/memory-plan.md §1).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(unix)]
const PROCESS_NOFILE_TARGET: libc::rlim_t = 65_536;

/// Raise Comet's process-local descriptor budget without changing launchd or
/// the user's shell configuration. GUI launches on macOS commonly inherit 256.
#[cfg(unix)]
fn raise_process_nofile_limit() -> std::io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is valid for both calls and remains initialized for the
    // duration of each syscall.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = limit.rlim_cur;
    let target = limit.rlim_max.min(PROCESS_NOFILE_TARGET);
    if before < target {
        limit.rlim_cur = target;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((before, limit.rlim_cur))
}

fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    let nofile_limit = raise_process_nofile_limit();
    let mut args = std::env::args_os().collect::<Vec<_>>();
    let initial_url = args
        .get(1)
        .and_then(|value| value.to_str())
        .filter(|value| value.starts_with("comet://invite/"))
        .map(str::to_owned);
    if initial_url.is_some() {
        args.remove(1);
    }
    let cli = Cli::parse_from(args);
    // Long-running modes log at info, one-shot CLI commands at warn (RUST_LOG
    // overrides either).
    // loro's internal block-encode diagnostics log at info and flood
    // journald on every snapshot export — enough to fill a disk on a
    // long-running headless host. Quiet them by default (RUST_LOG still
    // overrides the whole filter).
    let long_running = matches!(&cli.command, None | Some(Command::Headless(_)));
    let default_filter = if long_running {
        "info,loro_internal=warn,loro=warn"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    // Long-running modes mirror stdout logging to {data_dir}/logs — a headed
    // app launched from Finder has no visible stdout, which left every sync
    // wedge report ("stale until restart") with zero diagnostics even though
    // the engine logs the exact failure line. One file per launch, previous
    // launch kept as `.old`.
    let log_file = if long_running {
        let mode = if cli.command.is_some() {
            "headless"
        } else {
            "headed"
        };
        open_log_file(mode)
    } else {
        None
    };
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match log_file {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Arc::new(file)),
                )
                .init(),
            None => registry.init(),
        }
    }
    #[cfg(unix)]
    match nofile_limit {
        Ok((before, after)) if after > before => {
            tracing::info!(before, after, "raised process file descriptor limit");
        }
        Err(error) => {
            tracing::warn!(%error, "could not raise process file descriptor limit");
        }
        _ => {}
    }

    match cli.command {
        Some(Command::Headless(args)) => {
            let mut config = engine_config_from_env();
            if let Some(edge_url) = args
                .edge_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                config.edge_url = edge_url.to_string();
            }
            let bootstrap = args.into_bootstrap()?;
            if let Some(bootstrap) = bootstrap.as_ref() {
                apply_device_bootstrap_policy(&mut config, bootstrap);
            }
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(config);
                let engine = match bootstrap {
                    Some(bootstrap) => engine.with_device_bootstrap(bootstrap),
                    None => engine,
                };
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::ScaffoldAuthority) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(scaffold_authority_cli(engine_config_from_env().ipc_port))
        }
        Some(Command::Sync) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(sync_cli(engine_config_from_env().ipc_port))
        }
        Some(Command::Session { command }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(session_cli::run(command, engine_config_from_env().ipc_port))
        }
        Some(Command::Update { check }) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(engine_config_from_env(), check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            run_headed(initial_url);
            Ok(())
        }
    }
}

fn run_headed(initial_url: Option<String>) {
    let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
    // Headed: the UI probes COMET_IPC_PORT and connects to a running daemon,
    // or embeds the engine in-process (ARCHITECTURE §1).
    comet_ui::run_app(comet_ui::UiConfig {
        data_dir: std::env::var_os("COMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        ipc_port: std::env::var("COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        edge_url: edge_url_from_env(),
        scaffold_url: scaffold_url_from_env(&edge_token),
        edge_token,
        project_scope: std::env::var("COMET_PROJECT_SCOPE")
            .unwrap_or_else(|_| release_defaults().2.into()),
        deployment_id: None,
        initial_url,
        default_harness: comet_ui::HarnessId::ClaudeCode,
        runtime_profile: comet_ui::RuntimeProfile::LocalController,
    });
}

/// Local installs select either the production-local or deterministic mock
/// profile. Scaffold-host authority is forced only by a validated bootstrap.
fn runtime_profile_from_env() -> comet_engine::RuntimeProfile {
    if matches!(
        std::env::var("COMET_HARNESS").as_deref().map(str::trim),
        Ok("mock")
    ) {
        comet_engine::RuntimeProfile::Mock
    } else {
        comet_engine::RuntimeProfile::LocalController
    }
}

fn apply_device_bootstrap_policy(
    config: &mut comet_engine::EngineConfig,
    bootstrap: &comet_engine::DeviceBootstrapConfig,
) {
    config.project_scope = bootstrap.project_id.clone();
    config.deployment_id = Some(bootstrap.deployment_id.clone());
    config.runtime_profile = comet_engine::RuntimeProfile::ScaffoldHost;
    config.default_harness = comet_engine::HarnessId::Omp;
    // Device-mode auth already prevents constructing ScaffoldRuntime. Keep the
    // endpoint absent as defense in depth against recursive sandbox control.
    config.scaffold_url = None;
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> comet_engine::EngineConfig {
    // An explicit local bearer opts out of Scaffold OAuth.
    let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
    comet_engine::EngineConfig {
        data_dir: std::env::var_os("COMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url: edge_url_from_env(),
        ipc_port: std::env::var("COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        runtime_profile: runtime_profile_from_env(),
        project_scope: std::env::var("COMET_PROJECT_SCOPE")
            .unwrap_or_else(|_| release_defaults().2.into()),
        // Ordinary environment/config input cannot select a trusted deployment
        // room. Only validated device bootstrap populates this field.
        deployment_id: None,
        scaffold_url: scaffold_url_from_env(&edge_token),
        edge_token,
    }
}

/// `COMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        _ => comet_engine::HarnessId::ClaudeCode,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}

async fn scaffold_authority_cli(ipc_port: u16) -> anyhow::Result<()> {
    let client = comet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|error| anyhow::anyhow!("scaffold host unavailable: {error}"))?;
    let authority = client
        .call(
            comet_rpc::methods::SCAFFOLD_HOST_AUTHORITY,
            serde_json::json!({}),
        )
        .await
        .map_err(|error| anyhow::anyhow!("ScaffoldHostAuthority failed: {error}"))?;
    println!("{}", serde_json::to_string(&authority)?);
    Ok(())
}

/// `comet sync`: dial the running engine's IPC and print per-room sync state.
/// The introspection surface every 2026-08 incident was missing — "is this
/// device's workspace room actually receiving?" as a one-liner.
async fn sync_cli(ipc_port: u16) -> anyhow::Result<()> {
    let client = comet_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({e}) — is comet running?")
        })?;
    let status = client
        .call(comet_rpc::methods::SYNC_STATUS, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("SyncStatus failed: {e}"))?;
    let now = status.get("nowMs").and_then(|v| v.as_i64()).unwrap_or(0);
    let age = |ms: i64| -> String {
        if ms <= 0 {
            return "never".into();
        }
        let s = (now - ms).max(0) / 1000;
        if s >= 3600 {
            format!("{}h{}m ago", s / 3600, (s % 3600) / 60)
        } else if s >= 60 {
            format!("{}m{}s ago", s / 60, s % 60)
        } else {
            format!("{s}s ago")
        }
    };
    let room_line = |room: Option<&serde_json::Value>| -> String {
        let Some(room) = room else {
            return "no room (dialing or edge-less)".into();
        };
        let get = |k: &str| room.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        format!(
            "{} pushed {} · acked {} · rejoins {} probes {} resyncs {} drops {}",
            if room.get("connected").and_then(|v| v.as_bool()) == Some(true) {
                "connected ·"
            } else {
                "DISCONNECTED ·"
            },
            age(get("lastPushedMs")),
            age(get("lastAckMs")),
            get("rejoins"),
            get("probes"),
            get("fullResyncs"),
            get("disconnects"),
        )
    };
    println!(
        "Device:    {}",
        status
            .get("deviceId")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    );
    println!(
        "Workspace: {}",
        room_line(status.get("workspace").filter(|v| !v.is_null()))
    );
    let chats = status
        .get("chats")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if chats.is_empty() {
        println!("Chats:     none open");
    }
    for chat in &chats {
        println!(
            "Chat {}: {}",
            chat.get("chatId")
                .and_then(|v| v.as_str())
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("?"),
            room_line(chat.get("room").filter(|v| !v.is_null()))
        );
    }
    Ok(())
}

/// `{data_dir}/logs/comet-{mode}.log`, previous launch preserved as `.old`.
/// Headed and headless are separate files so an embedded-engine app and a
/// daemon on the same machine never interleave writes.
///
/// The returned file holds an exclusive `flock` for the process lifetime:
/// rotate-on-launch is only safe when nothing is still WRITING the current
/// file. On 2026-08-04 a dev build launched twice next to the running
/// installed app — the first rename put the daemon's live log at `.old`, the
/// second unlinked it entirely, and the daemon spent the rest of the incident
/// logging to an orphaned inode (an entire day of sync diagnostics gone at
/// the exact moment they were needed). A launch that finds the canonical file
/// locked logs to `comet-{mode}.{pid}.log` instead; the next lock-holding
/// launch sweeps pid-suffixed files older than a week.
fn open_log_file(mode: &str) -> Option<std::fs::File> {
    let dir = std::env::var_os("COMET_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(dirs_data_dir)
        .join("logs");
    open_log_file_in(&dir, mode)
}

/// Dir-parameterized body of [`open_log_file`] (unit-testable without env).
fn open_log_file_in(dir: &std::path::Path, mode: &str) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("comet-{mode}.log"));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // Probe and retain the canonical inode. Rotation copies its previous
        // contents to `.old`, then truncates this same locked file; no path or
        // lock handoff exists for a concurrent launcher to race.
        let preexisting = path.exists();
        let existing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let rc = unsafe { libc::flock(existing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            // A live process owns the canonical log — leave it alone.
            return std::fs::File::create(
                dir.join(format!("comet-{mode}.{}.log", std::process::id())),
            )
            .ok();
        }
        if preexisting {
            let _ = std::fs::copy(&path, dir.join(format!("comet-{mode}.log.old")));
            existing.set_len(0).ok()?;
        }
        sweep_stale_pid_logs(dir, mode);
        Some(existing)
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::rename(&path, dir.join(format!("comet-{mode}.log.old")));
        std::fs::File::create(&path).ok()
    }
}

#[cfg(all(test, unix))]
mod device_bootstrap_tests {
    use super::{Cli, HeadlessArgs, apply_device_bootstrap_policy};
    use clap::Parser as _;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    #[test]
    fn consumes_private_bootstrap_file_and_rejects_secret_argv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap.json");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(
            file,
            r#"{{"deviceJoinGrant":"cg1.secret","projectId":"project-a","deploymentId":"project-a","sessionId":"session-a","deviceId":"device-a","lifecycleEpoch":1,"sandboxId":"sandbox-a"}}"#
        )
        .unwrap();
        drop(file);

        let bootstrap = HeadlessArgs {
            device_bootstrap_file: Some(path.clone()),
            edge_url: None,
        }
        .into_bootstrap()
        .unwrap()
        .unwrap();
        assert_eq!(bootstrap.device_join_grant, "cg1.secret");
        assert!(!path.exists(), "single-use credential file must be removed");
        assert!(
            Cli::try_parse_from(["comet", "headless", "--device-join-grant", "cg1.secret"])
                .is_err(),
            "join credentials must never be accepted in process argv"
        );
    }

    #[test]
    fn validated_bootstrap_forces_omp_only_scaffold_host_policy() {
        let mut config = super::engine_config_from_env();
        config.scaffold_url = Some("https://scaffold.invalid".into());
        let bootstrap = comet_engine::DeviceBootstrapConfig {
            device_join_grant: "cg1.redacted".into(),
            project_id: "ashler-staging".into(),
            deployment_id: "deployment-a".into(),
            session_id: "session-a".into(),
            device_id: "comet-scaffold-sandbox-a-e1".into(),
            lifecycle_epoch: 1,
            sandbox_id: "sandbox-a".into(),
        };
        apply_device_bootstrap_policy(&mut config, &bootstrap);
        assert_eq!(
            config.runtime_profile,
            comet_engine::RuntimeProfile::ScaffoldHost
        );
        assert_eq!(config.default_harness, comet_engine::HarnessId::Omp);
        assert_eq!(config.project_scope, "ashler-staging");
        assert_eq!(config.deployment_id.as_deref(), Some("deployment-a"));
        assert!(config.scaffold_url.is_none());
    }
}

#[cfg(all(test, unix))]
static PROCESS_RESOURCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, unix))]
mod process_limit_tests {
    use super::{PROCESS_NOFILE_TARGET, raise_process_nofile_limit};

    const LOW_NOFILE_CHILD: &str = "COMET_TEST_LOW_NOFILE_PROCESS_CHILD";

    #[test]
    fn raises_soft_limit_and_children_inherit_it() {
        let _process_resource_guard = super::PROCESS_RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if std::env::var_os(LOW_NOFILE_CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "process_limit_tests::raises_soft_limit_and_children_inherit_it",
                    "--nocapture",
                ])
                .env(LOW_NOFILE_CHILD, "1")
                .output()
                .expect("spawn isolated descriptor-limit test");
            assert!(
                output.status.success(),
                "isolated descriptor-limit test failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is valid for both calls; this mutation is confined to
        // the isolated child test process.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur = limit.rlim_cur.min(256);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
        }

        let (_, raised) = raise_process_nofile_limit().expect("raise descriptor limit");
        assert_eq!(raised, limit.rlim_max.min(PROCESS_NOFILE_TARGET));

        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "ulimit -n"])
            .output()
            .expect("spawn child process");
        assert!(child.status.success());
        let inherited: libc::rlim_t = String::from_utf8_lossy(&child.stdout)
            .trim()
            .parse()
            .expect("numeric child descriptor limit");
        assert_eq!(inherited, raised);
    }
}

#[cfg(all(test, unix))]
mod log_file_tests {
    use super::open_log_file_in;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn second_launch_never_rotates_a_live_processes_log() {
        let _process_resource_guard = super::PROCESS_RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // First launch owns the canonical file and keeps writing.
        let first = open_log_file_in(dir, "headed").expect("first log");
        assert!(dir.join("comet-headed.log").is_file());
        // Second launch while the first is alive: canonical file untouched,
        // pid-suffixed overflow file instead (the 2026-08-04 clobber).
        let second = open_log_file_in(dir, "headed").expect("second log");
        let pid_path = dir.join(format!("comet-headed.{}.log", std::process::id()));
        assert!(pid_path.is_file(), "expected pid-suffixed overflow log");
        assert!(
            !dir.join("comet-headed.log.old").exists(),
            "live canonical log must not be rotated away"
        );
        drop(second);
        // After the owner exits, a fresh launch rotates normally.
        drop(first);
        let third = open_log_file_in(dir, "headed").expect("third log");
        assert!(
            dir.join("comet-headed.log.old").is_file(),
            "rotation resumes"
        );
        drop(third);
    }

    #[test]
    fn concurrent_launches_leave_one_locked_canonical_log() {
        let _process_resource_guard = super::PROCESS_RESOURCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let dir = std::sync::Arc::new(dir.path().to_path_buf());
        std::fs::write(dir.join("comet-headed.log"), "previous launch\n").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let launches: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    open_log_file_in(&dir, "headed").expect("concurrent log")
                })
            })
            .collect();
        barrier.wait();
        let files: Vec<_> = launches
            .into_iter()
            .map(|launch| launch.join().expect("launcher thread"))
            .collect();

        let canonical = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("comet-headed.log"))
            .expect("canonical log");
        assert_ne!(
            unsafe { libc::flock(canonical.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "one launch must retain the canonical lock"
        );
        assert!(dir.join("comet-headed.log.old").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join("comet-headed.log.old")).unwrap(),
            "previous launch\n"
        );
        drop(files);
    }
}

/// Delete `comet-{mode}.{pid}.log` overflow files older than a week — they
/// only exist when a second instance raced a live one for the canonical log.
#[cfg(unix)]
fn sweep_stale_pid_logs(dir: &std::path::Path, mode: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("comet-{mode}.");
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(middle) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".log"))
        else {
            continue;
        };
        if !middle.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > week);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod session_parser_tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn parses_session_send_with_explicit_source_and_wait_options() {
        let cli = Cli::try_parse_from([
            "comet",
            "session",
            "send",
            "target-chat",
            "hello",
            "--from",
            "source-chat",
            "--wait",
            "--timeout",
            "4500",
        ])
        .unwrap();
        let Some(Command::Session {
            command:
                super::session_cli::SessionCommand::Send {
                    chat_id,
                    text,
                    from,
                    wait,
                    timeout,
                },
        }) = cli.command
        else {
            panic!("expected session send");
        };
        assert_eq!(chat_id, "target-chat");
        assert_eq!(text, "hello");
        assert_eq!(from.as_deref(), Some("source-chat"));
        assert!(wait);
        assert_eq!(timeout, Some(4500));
    }

    #[test]
    fn parses_session_reply_contract() {
        let cli = Cli::try_parse_from([
            "comet",
            "session",
            "reply",
            "--session",
            "target-chat",
            "--command",
            "command-id",
            "done",
            "--wait",
        ])
        .unwrap();
        let Some(Command::Session {
            command:
                super::session_cli::SessionCommand::Reply {
                    session,
                    command,
                    text,
                    wait,
                },
        }) = cli.command
        else {
            panic!("expected session reply");
        };
        assert_eq!(session, "target-chat");
        assert_eq!(command, "command-id");
        assert_eq!(text, "done");
        assert!(wait);
    }
}
