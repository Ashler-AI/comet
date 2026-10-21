//! Crew service management. Packaged desktop environments own distinct launchd
//! identities. Runtime coordinates are captured at installation; credentials
//! remain in the profile auth store.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

fn production_service(config: &zeron_engine::EngineConfig) -> bool {
    config.edge_url.trim_end_matches('/') == "https://comet.internal.ashler.com"
        && config.data_dir.file_name() != Some(std::ffi::OsStr::new(".comet-native-staging"))
        && config.ipc_port != 27655
        && config.scaffold.as_ref().is_none_or(|authority| {
            authority.project_scope == "ashler-production"
                && authority.control_origin.trim_end_matches('/') == "https://scaffold.internal.ashler.com"
        })
}

fn launchd_label(production: bool) -> &'static str {
    if production { "ai.ashler.comet" } else { "ai.ashler.comet.staging" }
}

fn systemd_unit(production: bool) -> &'static str {
    if production { "comet-native.service" } else { "comet-native-staging.service" }
}

fn service_root(production: bool) -> &'static str {
    if production { ".comet-native" } else { ".comet-native-staging" }
}

/// Capture executable discovery and configured provider binding. Native BOAT_ID
/// must come from the live service-manager environment, never a copied unit.
/// Runtime coordinates below come from the resolved CLI configuration.
const CAPTURED_ENV: &[&str] = &[
    "PATH",
    "OMP_EXECUTABLE",
    "PI_CODING_AGENT_DIR",
    "RUST_LOG",
    "COMET_DEVBOX_PROVIDER",
    "COMET_DEVBOX_ID",
];

pub fn install(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    zeron_engine::devbox::host_binding().map_err(anyhow::Error::msg)?;
    let production = production_service(config);
    let systemd_unit = systemd_unit(production);
    let data_dir = &config.data_dir;
    let exe = std::env::current_exe().context("resolving the Crew executable path")?;
    let mut env = captured_env();
    env.extend([
        ("COMET_DATA_DIR".into(), data_dir.to_string_lossy().into_owned()),
        ("COMET_IPC_PORT".into(), config.ipc_port.to_string()),
        ("COMET_EDGE_URL".into(), config.edge_url.clone()),
        ("COMET_HARNESS".into(), serde_json::to_value(config.default_harness)?
            .as_str().context("invalid default harness")?.to_owned()),
        ("COMET_INFERENCE_MODE".into(), if config.managed_inference { "managed" } else { "local" }.into()),
        ("ASHLER_INCREMENTAL_TSC_CHECKS".into(), "false".into()),
    ]);
    if let Some(authority) = &config.scaffold {
        env.push(("COMET_SCAFFOLD_URL".into(), authority.control_origin.clone()));
        env.push(("COMET_PROJECT_SCOPE".into(), authority.project_scope.clone()));
        if let Some(deployment) = &authority.deployment_id {
            env.push(("COMET_DEPLOYMENT_ID".into(), deployment.clone()));
        }
    }
    if let Some((binary, digest)) = &config.scaffold_runtime_artifact {
        env.push(("COMET_SCAFFOLD_RUNTIME_BINARY".into(), binary.to_string_lossy().into_owned()));
        env.push(("COMET_SCAFFOLD_RUNTIME_SHA256".into(), digest.clone()));
    }
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path(production)?;
        std::fs::create_dir_all(plist.parent().expect("LaunchAgents parent"))?;
        std::fs::create_dir_all(data_dir)?;
        // Reinstall-friendly: unload any previous incarnation before rewriting.
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target(production)?]);
        std::fs::write(
            &plist,
            render_launchd_plist(&exe, &env, &data_dir.join("daemon.log"), production),
        )?;
        run(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        )?;
        println!(
            "Installed and started {} ({}).",
            launchd_label(production), plist.display()
        );
    } else if cfg!(target_os = "linux") {
        let unit = systemd_unit_path(production)?;
        std::fs::create_dir_all(unit.parent().expect("systemd user dir"))?;
        std::fs::write(&unit, render_systemd_unit(&exe, &env, production))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", systemd_unit])?;
        println!("Installed and started {systemd_unit} ({}).", unit.display());
        println!(
            "For start-at-boot without an active login session (VPS): loginctl enable-linger $USER"
        );
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!(
        "Without a saved account the engine stays local-only; sign-in and restart are optional for sync."
    );
    println!(
        "Logs: {}",
        if cfg!(target_os = "macos") {
            format!("{}", data_dir.join("daemon.log").display())
        } else {
            format!("journalctl --user -u {systemd_unit}")
        }
    );
    Ok(())
}

pub fn uninstall(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    let production = production_service(config);
    if cfg!(target_os = "macos") {
        let _ = run_quiet("launchctl", &["bootout", &launchd_service_target(production)?]);
        let plist = launchd_plist_path(production)?;
        match std::fs::remove_file(&plist) {
            Ok(()) => println!("Removed {}.", plist.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else if cfg!(target_os = "linux") {
        let _ = run_quiet("systemctl", &["--user", "disable", "--now", systemd_unit(production)]);
        let unit = systemd_unit_path(production)?;
        match std::fs::remove_file(&unit) {
            Ok(()) => {
                run("systemctl", &["--user", "daemon-reload"])?;
                println!("Removed {}.", unit.display());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("Not installed.")
            }
            Err(err) => return Err(err.into()),
        }
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    Ok(())
}

pub fn start(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    let production = production_service(config);
    if cfg!(target_os = "macos") {
        let plist = launchd_plist_path(production)?;
        if !plist.exists() {
            bail!("not installed — run `comet daemon install` first");
        }
        // `stop` boots the job out of the domain, so start = bootstrap; already
        // loaded is fine, then kickstart guarantees a running process either way.
        let _ = run_quiet(
            "launchctl",
            &["bootstrap", &launchd_domain()?, &plist.to_string_lossy()],
        );
        run("launchctl", &["kickstart", &launchd_service_target(production)?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "start", systemd_unit(production)])?;
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!("Started.");
    Ok(())
}

pub fn stop(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    let production = production_service(config);
    if cfg!(target_os = "macos") {
        // bootout (not `kill`): with KeepAlive the job would otherwise respawn.
        run("launchctl", &["bootout", &launchd_service_target(production)?])?;
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "stop", systemd_unit(production)])?;
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
    println!("Stopped.");
    Ok(())
}

pub fn restart(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    let production = production_service(config);
    if cfg!(target_os = "macos") {
        if run_quiet(
            "launchctl",
            &["kickstart", "-k", &launchd_service_target(production)?],
        )
        .is_err()
        {
            // Not loaded (e.g. after `stop`) — fall through to a plain start.
            return start(config);
        }
        println!("Restarted.");
        Ok(())
    } else if cfg!(target_os = "linux") {
        run("systemctl", &["--user", "restart", systemd_unit(production)])?;
        println!("Restarted.");
        Ok(())
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
}

pub fn status(config: &zeron_engine::EngineConfig) -> anyhow::Result<()> {
    let production = production_service(config);
    if cfg!(target_os = "macos") {
        let output = Command::new("launchctl")
            .args(["print", &launchd_service_target(production)?])
            .output()
            .context("running launchctl")?;
        if !output.status.success() {
            println!(
                "{}: not loaded{}", launchd_label(production),
                if launchd_plist_path(production)?.exists() {
                    " (installed — `comet daemon start`)"
                } else {
                    " (not installed — `comet daemon install`)"
                }
            );
            return Ok(());
        }
        // `launchctl print` is pages long; surface just the liveness lines.
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{}: loaded", launchd_label(production));
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("state = ")
                || trimmed.starts_with("pid = ")
                || trimmed.starts_with("last exit code = ")
            {
                println!("  {trimmed}");
            }
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        // Passthrough; `status` exits nonzero for inactive units, which is not an
        // error for us to report — the output already says it.
        let _ = Command::new("systemctl")
            .args(["--user", "--no-pager", "status", systemd_unit(production)])
            .status()
            .context("running systemctl")?;
        Ok(())
    } else {
        bail!("Crew daemon is only supported on macOS (launchd) and Linux (systemd)");
    }
}

// ---------------------------------------------------------------------------
// Unit rendering (pure — unit-tested below)
// ---------------------------------------------------------------------------

fn captured_env() -> Vec<(String, String)> {
    CAPTURED_ENV
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key.to_string(), v)))
        .collect()
}

fn render_systemd_unit(exe: &Path, env: &[(String, String)], production: bool) -> String {
    let mut unit = String::from(
        "[Unit]\nDescription=Crew headless engine\nAfter=network-online.target\nStartLimitIntervalSec=60\nStartLimitBurst=5\n\n[Service]\n",
    );
    for (key, value) in env {
        // systemd unquotes the value; escape the characters it treats specially.
        let value = value.replace('\\', "\\\\").replace('"', "\\\"")
            .replace('\n', "\\n").replace('\r', "\\r").replace('%', "%%");
        unit.push_str(&format!("Environment=\"{key}={value}\"\n"));
    }
    unit.push_str(&format!(
        "ExecStart={} headless\nRestart=on-failure\nRestartSec=5\nEnvironmentFile=-%h/{}/env\n\n[Install]\nWantedBy=default.target\n",
        systemd_exec_path(exe), service_root(production)
    ));
    unit
}

/// The ExecStart binary path. An exe under `~/.comet-native/app/` came from the
/// curl|sh installer, whose upgrades relink `app/current` — point the unit at
/// the symlink (as the installer's own unit does) so it never pins one version.
/// (`current_exe` resolves symlinks, so the versioned dir is what we see here.)
fn systemd_exec_path(exe: &Path) -> String {
    exec_path_for(exe, std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn exec_path_for(exe: &Path, home: Option<&Path>) -> String {
    for root in [".comet-native", ".comet-native-staging"] {
        if home.is_some_and(|home| exe.starts_with(home.join(root).join("app"))) {
            return format!("%h/{root}/app/current/comet");
        }
    }
    exe.display().to_string()
}

fn render_launchd_plist(exe: &Path, env: &[(String, String)], log: &Path, production: bool) -> String {
    let mut env_dict = String::new();
    for (key, value) in env {
        env_dict.push_str(&format!(
            "      <key>{}</key><string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{exe}</string>
      <string>headless</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env_dict}    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
      <key>SuccessfulExit</key><false/>
    </dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
  </dict>
</plist>
"#,
        label = launchd_label(production),
        exe = xml_escape(&exe.to_string_lossy()),
        env_dict = env_dict,
        log = xml_escape(&log.to_string_lossy()),
    )
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Paths + process helpers
// ---------------------------------------------------------------------------

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn launchd_plist_path(production: bool) -> anyhow::Result<PathBuf> {
    Ok(home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", launchd_label(production))))
}

fn systemd_unit_path(production: bool) -> anyhow::Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home_dir()?.join(".config"));
    Ok(config.join("systemd/user").join(systemd_unit(production)))
}

fn launchd_domain() -> anyhow::Result<String> {
    let output = Command::new("id").arg("-u").output().context("id -u")?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("could not determine the current uid");
    }
    Ok(format!("gui/{uid}"))
}

fn launchd_service_target(production: bool) -> anyhow::Result<String> {
    Ok(format!("{}/{}", launchd_domain()?, launchd_label(production)))
}

/// Run a command echoing it first; error (with stderr) on nonzero exit.
fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    println!("$ {program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Run without echoing; used where failure is an expected branch.
fn run_quiet(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!("{program} failed ({})", output.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_service_does_not_address_production_installation() {
        let mut config = zeron_engine::EngineConfig {
            data_dir: "/home/u/.comet-native-staging".into(),
            edge_url: "https://comet-staging.internal.ashler.com".into(),
            ipc_port: 27655, edge_token: None, default_harness: zeron_engine::HarnessId::Mock,
            org_id: None, workos_client_id: None, scaffold: None, device: None,
            managed_inference: false, device_bootstrap_file: None, scaffold_runtime_artifact: None,
        };
        let production = production_service(&config);
        assert!(!production);
        assert_eq!(systemd_unit(production), "comet-native-staging.service");
        assert_eq!(launchd_label(production), "ai.ashler.comet.staging");
        let unit = render_systemd_unit(Path::new("/source/comet"), &[], production);
        assert!(unit.contains("EnvironmentFile=-%h/.comet-native-staging/env"));
        assert!(!unit.contains("EnvironmentFile=-%h/.comet-native/env"));
        assert_eq!(exec_path_for(Path::new("/home/u/.comet-native-staging/app/1/comet"), Some(Path::new("/home/u"))), "%h/.comet-native-staging/app/current/comet");
        config.edge_url = "https://comet.internal.ashler.com".into();
        assert!(!production_service(&config));
        config.data_dir = "/home/u/.comet-native".into();
        assert!(!production_service(&config));
        config.ipc_port = 27654;
        assert!(production_service(&config));
        assert_eq!(systemd_unit(true), "comet-native.service");
        assert_eq!(launchd_label(true), "ai.ashler.comet");
    }

    #[test]
    fn systemd_unit_shape() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/bin/zeron"),
            &[
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("ZERON_EDGE_URL".into(), "https://edge.example".into()),
                ("RUST_LOG".into(), "info,zeron=\"debug\"".into()),
                ("COMET_DEVBOX_PROVIDER".into(), "boat".into()),
                ("COMET_DEVBOX_ID".into(), "bx_jdyh7avc".into()),
            ],
            true,
        );
        assert!(unit.contains("ExecStart=/usr/local/bin/zeron headless\n"));
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(unit.contains("Environment=\"ZERON_EDGE_URL=https://edge.example\"\n"));
        for key in ["COMET_DEVBOX_PROVIDER", "COMET_DEVBOX_ID"] {
            assert!(CAPTURED_ENV.contains(&key));
        }
        assert!(unit.contains("Environment=\"COMET_DEVBOX_PROVIDER=boat\"\n"));
        assert!(unit.contains("Environment=\"COMET_DEVBOX_ID=bx_jdyh7avc\"\n"));
        assert!(!CAPTURED_ENV.contains(&"BOAT_ID"));
        assert!(!unit.contains("Environment=\"BOAT_ID="));
        // Inner quotes escaped so systemd re-parses the value verbatim.
        assert!(unit.contains("Environment=\"RUST_LOG=info,zeron=\\\"debug\\\"\"\n"));
        assert!(unit.contains("StartLimitIntervalSec=60\n"));
        assert!(unit.contains("StartLimitBurst=5\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("session.json"));
        assert!(!unit.contains("ConditionPathExists"));
        assert!(unit.contains("EnvironmentFile=-%h/.comet-native/env"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_environment_preserves_literals_without_directive_injection() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/bin/comet"),
            &[("COMET_EDGE_URL".into(), "https://edge.test/%2F\nExecStart=/bad\r".into())],
            false,
        );
        assert!(unit.contains("Environment=\"COMET_EDGE_URL=https://edge.test/%%2F\\nExecStart=/bad\\r\"\n"));
        assert!(!unit.contains("\nExecStart=/bad"));
    }


    #[test]
    fn installed_exe_uses_the_current_symlink() {
        // Installer-managed binary (current_exe resolves the `current` symlink to
        // the versioned dir): the unit must point back at the symlink.
        assert_eq!(
            exec_path_for(
                Path::new("/home/u/.comet-native/app/0.3.0/comet"),
                Some(Path::new("/home/u")),
            ),
            "%h/.comet-native/app/current/comet"
        );
        // Source build: literal path.
        assert_eq!(
            exec_path_for(
                Path::new("/src/target/debug/zeron"),
                Some(Path::new("/home/u"))
            ),
            "/src/target/debug/zeron"
        );
    }

    #[test]
    fn launchd_plist_shape() {
        let plist = render_launchd_plist(
            Path::new("/Users/x/zeron & co/zeron"),
            &[("ZERON_EDGE_URL".into(), "https://e?a=1&b=2".into())],
            Path::new("/Users/x/.zeron/daemon.log"),
            true,
        );
        assert!(plist.contains(&format!("<key>Label</key><string>{}</string>", launchd_label(true))));
        // XML-escaped exe path and env value.
        assert!(plist.contains("<string>/Users/x/zeron &amp; co/zeron</string>"));
        assert!(plist.contains("<string>https://e?a=1&amp;b=2</string>"));
        assert!(plist.contains("<string>headless</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
        assert!(
            plist.contains("<key>StandardOutPath</key><string>/Users/x/.zeron/daemon.log</string>")
        );
    }
}
