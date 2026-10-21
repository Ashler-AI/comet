use anyhow::{Context, bail};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use zeron_engine::{EngineConfig, HarnessId};
use zeron_sync::scaffold_auth::{DeviceConfig, ScaffoldConfig};

#[derive(Debug, Default, Args)]
pub struct RuntimeArgs {
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub ipc_port: Option<u16>,
    #[arg(long, global = true)]
    pub edge_url: Option<String>,
    #[arg(long, global = true)]
    pub scaffold_url: Option<String>,
    #[arg(long, global = true)]
    pub project_scope: Option<String>,
    #[arg(long, global = true)]
    pub deployment_id: Option<String>,
}

#[derive(Debug, Default, Args)]
pub struct HeadlessArgs {
    /// Private, one-use device bootstrap issued by Scaffold.
    #[arg(long)]
    pub device_bootstrap_file: Option<PathBuf>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSettings {
    edge_url: Option<String>,
    scaffold_url: Option<String>,
    project_scope: Option<String>,
    deployment_id: Option<String>,
    ipc_port: Option<u16>,
    scaffold_runtime_artifact: Option<(PathBuf, String)>,
}

impl RuntimeArgs {
    pub fn config(&self, persist: bool, own_runtime: bool, headed: bool) -> anyhow::Result<EngineConfig> {
        self.config_with_env(persist, own_runtime, headed && option_env!("COMET_PACKAGE_ENVIRONMENT").is_some(), |name| std::env::var_os(name))
    }

    fn config_with_env(
        &self,
        persist: bool,
        own_runtime: bool,
        pinned: bool,
        mut env: impl FnMut(&str) -> Option<std::ffi::OsString>,
    ) -> anyhow::Result<EngineConfig> {
        let isolated = !pinned && (persist || own_runtime)
            && env("COMET_LOCAL_AGENT_RUNTIME").as_deref() == Some(std::ffi::OsStr::new("1"));
        if isolated && (self.data_dir.as_ref().is_none_or(|dir| dir.as_os_str().is_empty()) || self.ipc_port.is_none()) {
            bail!("Crew agent launches require explicit --data-dir and --ipc-port for an independent runtime");
        }
        // A packaged desktop cannot inherit another environment's authority.
        // Agent environments describe the parent engine, not an owned runtime.
        let defaults = RuntimeArgs::default();
        let args = if pinned { &defaults } else { self };
        let data_dir = std::path::absolute(args.data_dir.clone().unwrap_or_else(|| crate::paths::resolve_data_dir(&mut env, pinned)))?;
        let mut setting = |name: &str, upstream: &str| {
            if pinned || isolated { return None; }
            env(name).and_then(|value| value.into_string().ok()).filter(|value| !value.trim().is_empty())
                .or_else(|| env(upstream).and_then(|value| value.into_string().ok()).filter(|value| !value.trim().is_empty()))
        };
        let explicit_profile = args.data_dir.is_some() || setting("COMET_DATA_DIR", "ZERON_DATA_DIR").is_some();
        let settings_path = data_dir.join("crew-runtime.json");
        let saved = if pinned { RuntimeSettings::default() } else {
            match std::fs::read(&settings_path) {
                Ok(bytes) => serde_json::from_slice::<RuntimeSettings>(&bytes)
                    .context("invalid Crew runtime configuration")?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => RuntimeSettings::default(),
                Err(error) => return Err(error.into()),
            }
        };
        let harness_setting = setting("COMET_HARNESS", "ZERON_HARNESS");
        let mock = harness_setting.as_deref() == Some("mock");
        let edge_token = if mock { None } else { setting("COMET_EDGE_TOKEN", "ZERON_EDGE_TOKEN") };
        let mut edge_url = args.edge_url.clone()
            .or_else(|| setting("COMET_EDGE_URL", "ZERON_EDGE_URL"))
            .or(saved.edge_url);
        let mut scaffold_url = args.scaffold_url.clone()
            .or_else(|| setting("COMET_SCAFFOLD_URL", "CREW_CONTROL_ORIGIN"))
            .or(saved.scaffold_url);
        let mut project_scope = args.project_scope.clone()
            .or_else(|| setting("COMET_PROJECT_SCOPE", "CREW_PROJECT_SCOPE"))
            .or(saved.project_scope);
        let mut deployment_id = args.deployment_id.clone()
            .or_else(|| setting("COMET_DEPLOYMENT_ID", "CREW_DEPLOYMENT_ID"))
            .or(saved.deployment_id);
        if mock {
            edge_url = Some("http://127.0.0.1:27941".into());
            scaffold_url = None;
            project_scope = None;
            deployment_id = None;
        } else if edge_url.is_none() && scaffold_url.is_none() && project_scope.is_none()
            && deployment_id.is_none() && edge_token.is_none()
        {
            if explicit_profile {
                // Explicit source/dev profiles stay local without saved authority.
                edge_url = Some("http://127.0.0.1:27941".into());
            } else {
                let defaults = &crate::environment::DEFAULTS;
                edge_url = Some(defaults.edge_url.into());
                scaffold_url = defaults.scaffold_url.map(Into::into);
                project_scope = defaults.project_scope.map(Into::into);
                deployment_id = defaults.deployment_id.map(Into::into);
            }
        }
        // Canary's standalone development bearer historically uses local Edge.
        if edge_url.is_none() && edge_token.is_some() && scaffold_url.is_none()
            && crate::environment::DEFAULTS.scaffold_url.is_none()
        {
            edge_url = Some(crate::environment::DEFAULTS.edge_url.into());
        }
        let edge_url = edge_url.context("custom Crew authority requires --edge-url")?;
        if scaffold_url.is_none() && edge_token.is_none()
            && (project_scope.is_some() || deployment_id.is_some())
        {
            bail!("custom Crew project or deployment requires --scaffold-url");
        }
        let environment_port = setting("COMET_IPC_PORT", "ZERON_IPC_PORT")
            .map(|port| port.parse::<u16>().context("invalid Crew IPC port")).transpose()?;
        let ipc_port = args.ipc_port.or(environment_port).or(saved.ipc_port)
            .unwrap_or(crate::environment::DEFAULTS.ipc_port);
        if ipc_port == 0 { bail!("Crew IPC port must be nonzero"); }
        if edge_token.is_some() && scaffold_url.is_some() {
            bail!("development bearer and Scaffold authority cannot be configured together");
        }
        let scaffold = scaffold_url.as_ref().map(|origin| {
            let project = project_scope.as_ref().context("Scaffold login requires --project-scope")?;
            let mut scaffold = ScaffoldConfig::new(origin, &edge_url, project);
            scaffold.deployment_id = deployment_id.clone();
            Ok::<_, anyhow::Error>(scaffold)
        }).transpose()?;
        let harness = match harness_setting.as_deref() {
            Some("mock") => HarnessId::Mock,
            Some("codex") => HarnessId::Codex,
            Some("omp") => HarnessId::Omp,
            Some("claude-code") => HarnessId::ClaudeCode,
            Some(value) => serde_json::from_value(serde_json::Value::String(value.into()))
                .context("unknown Crew harness")?,
            None if scaffold.is_some() => HarnessId::Omp,
            None => HarnessId::ClaudeCode,
        };
        let managed_inference = setting("COMET_INFERENCE_MODE", "ZERON_INFERENCE_MODE").as_deref() != Some("local") && !mock;
        let artifact_path = setting("COMET_SCAFFOLD_RUNTIME_BINARY", "CREW_RUNTIME_BINARY");
        let artifact_hash = setting("COMET_SCAFFOLD_RUNTIME_SHA256", "CREW_RUNTIME_SHA256");
        let scaffold_runtime_artifact = if mock { None } else { match (artifact_path, artifact_hash) {
            (Some(path), Some(hash)) => Some((PathBuf::from(path), hash)),
            (None, None) => saved.scaffold_runtime_artifact,
            _ => bail!("runtime artifact requires both its binary path and SHA256"),
        }};
        // These are non-secret launch coordinates, retained for Finder launches
        // and device restarts. Credential storage belongs to the auth service.
        if persist && scaffold.is_some() {
            std::fs::create_dir_all(&data_dir)?;
            let settings = RuntimeSettings {
                edge_url: Some(edge_url.clone()), scaffold_url, project_scope: project_scope.clone(),
                deployment_id, ipc_port: args.ipc_port.or(saved.ipc_port),
                scaffold_runtime_artifact: scaffold_runtime_artifact.clone(),
            };
            let bytes = serde_json::to_vec_pretty(&settings)?;
            let mut file = tempfile::NamedTempFile::new_in(&data_dir)?;
            use std::io::Write;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            file.persist(&settings_path).map_err(|error| error.error)?;
        }
        Ok(EngineConfig {
            data_dir, edge_url, edge_token, ipc_port, default_harness: harness,
            org_id: None,
            workos_client_id: None, scaffold, device: None, managed_inference,
            device_bootstrap_file: None,
            scaffold_runtime_artifact,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bootstrap {
    device_join_grant: String,
    project_id: String,
    deployment_id: String,
    session_id: String,
    device_id: String,
    lifecycle_epoch: u64,
    sandbox_id: String,
    #[serde(default)]
    edge_url: Option<String>,
}

pub fn configure_device(config: &mut EngineConfig, path: &Path) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).context("device bootstrap unavailable")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 16 * 1024 {
        bail!("device bootstrap must be a bounded regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("device bootstrap must be private and owned by the current user");
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 { bail!("device bootstrap exceeded its size limit"); }
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid device bootstrap"))?;
    let authority = config.scaffold.as_ref().context("device bootstrap requires a configured Scaffold authority")?;
    if bootstrap.project_id != authority.project_scope
        || authority.deployment_id.as_deref().is_some_and(|deployment| deployment != bootstrap.deployment_id)
        || bootstrap.edge_url.as_deref().is_some_and(|edge| edge.trim_end_matches('/') != config.edge_url.trim_end_matches('/'))
    {
        bail!("device bootstrap does not match the configured authority");
    }
    config.device = Some(DeviceConfig {
        join_grant: bootstrap.device_join_grant,
        device_id: bootstrap.device_id,
        session_id: bootstrap.session_id,
        deployment_id: bootstrap.deployment_id,
        sandbox_id: bootstrap.sandbox_id,
        lifecycle_epoch: bootstrap.lifecycle_epoch,
    });
    config.device_bootstrap_file = Some(path.to_owned());
    config.default_harness = HarnessId::Omp;
    config.managed_inference = true;
    Ok(())
}

pub async fn scaffold_authority(ipc_port: u16) -> anyhow::Result<()> {
    let client = zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}")).await?;
    let authority = client.call("ScaffoldHostAuthority", serde_json::json!({})).await?;
    println!("{}", serde_json::to_string(&authority)?);
    Ok(())
}

// These bytes are authenticated by the same executable SHA-256 as the engine.
const SCAFFOLD_WEB_ASSETS: &[(&str, &[u8])] = &[
    ("server.mjs", include_bytes!("../../web/server.mjs")),
    ("rpc.mjs", include_bytes!("../../web/rpc.mjs")),
    ("package.json", include_bytes!("../../web/package.json")),
    ("contract.json", include_bytes!("../../web/contract.json")),
    ("public/app.js", include_bytes!("../../web/public/app.js")),
    ("public/index.html", include_bytes!("../../web/public/index.html")),
    ("public/style.css", include_bytes!("../../web/public/style.css")),
];

pub fn install_scaffold_web() -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let bin = executable.parent().context("runtime executable directory unavailable")?;
    let root = bin.parent().context("runtime root unavailable")?;
    if executable.file_name().is_none_or(|name| name != "comet")
        || bin.file_name().is_none_or(|name| name != "bin")
        || root.file_name().is_none_or(|name| name != "crew")
        || root.parent().and_then(Path::file_name).is_none_or(|name| name != ".scaffold")
    { bail!("browser assets require the native Scaffold runtime layout"); }
    install_web_assets(root)
}

fn install_web_assets(root: &Path) -> anyhow::Result<()> {
    let web = root.join("web");
    let public = web.join("public");
    for directory in [root, web.as_path(), public.as_path()] {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)] {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&directory) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
        let metadata = std::fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() { bail!("browser asset directory is not a regular directory"); }
        #[cfg(unix)] {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o022 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
                bail!("browser asset directory is writable by another user");
            }
        }
    }
    for (name, bytes) in SCAFFOLD_WEB_ASSETS {
        install_runtime_file(&web.join(name), bytes)?;
    }
    // The supervisor cannot execute the uploaded engine or browser before this.
    install_runtime_file(&root.join("runtime-ready"), b"crew-native\n")
}

fn install_runtime_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    match options.open(path) {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() != bytes.len() as u64 { bail!("existing browser asset differs from the verified runtime"); }
            #[cfg(unix)] {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 || metadata.mode() & 0o777 != 0o600 || metadata.uid() != unsafe { libc::geteuid() } {
                    bail!("existing browser asset is not a private owned file");
                }
            }
            let mut buffer = [0u8; 8192];
            for expected in bytes.chunks(buffer.len()) {
                file.read_exact(&mut buffer[..expected.len()])?;
                if buffer[..expected.len()] != *expected { bail!("existing browser asset differs from the verified runtime"); }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = tempfile::NamedTempFile::new_in(path.parent().context("asset parent unavailable")?)?;
            file.write_all(bytes)?;
            file.as_file().sync_all()?;
            file.persist_noclobber(path).map_err(|error| error.error)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    fn resolve(args: &RuntimeArgs, persist: bool, own_runtime: bool, vars: &[(&str, &str)]) -> anyhow::Result<EngineConfig> {
        args.config_with_env(persist, own_runtime, false, |name| {
            vars.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).into())
        })
    }

    #[test]
    fn custom_authority_is_not_completed_from_compiled_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crew-runtime.json");
        let mut args = RuntimeArgs {
            data_dir: Some(dir.path().to_owned()), scaffold_url: Some("https://own-control.example.test".into()),
            project_scope: Some("own-project".into()), ..Default::default()
        };
        assert!(resolve(&args, true, true, &[]).is_err());
        assert!(!path.exists());
        args.edge_url = Some("https://own-edge.example.test".into());
        resolve(&args, true, true, &[]).unwrap();
        let config = resolve(&RuntimeArgs { data_dir: args.data_dir.clone(), ..Default::default() }, true, true, &[]).unwrap();
        assert_eq!(config.edge_url, "https://own-edge.example.test");
        let authority = config.scaffold.unwrap();
        assert_eq!(authority.control_origin, "https://own-control.example.test");
        assert_eq!(authority.project_scope, "own-project");
        assert!(authority.deployment_id.is_none());
        let saved: RuntimeSettings = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(saved.deployment_id.is_none());
        assert!(resolve(&args, true, true, &[("COMET_EDGE_TOKEN", "dev@own")]).is_err());
        args.ipc_port = Some(0);
        assert!(resolve(&args, true, true, &[]).is_err());
    }

    #[cfg(comet_default_environment = "canary")]
    #[test]
    fn canary_standalone_development_bearer_still_needs_no_authority_flags() {
        let dir = tempfile::tempdir().unwrap();
        let args = RuntimeArgs { data_dir: Some(dir.path().to_owned()), ..Default::default() };
        let config = resolve(&args, false, true, &[("ZERON_EDGE_TOKEN", "dev@own")]).unwrap();
        assert_eq!(config.edge_token.as_deref(), Some("dev@own"));
        assert!(config.scaffold.is_none());
        assert!(!dir.path().join("crew-runtime.json").exists());
    }

    #[test]
    fn standalone_environment_and_flags_keep_precedence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("crew-runtime.json"), br#"{"edgeUrl":"http://saved.example.test","ipcPort":39011}"#).unwrap();
        let mut args = RuntimeArgs { data_dir: Some(dir.path().to_owned()), ..Default::default() };
        let vars = [("COMET_EDGE_URL", "http://native.example.test"), ("ZERON_EDGE_URL", "http://upstream.example.test"),
            ("COMET_IPC_PORT", "39012"), ("ZERON_IPC_PORT", "39013"), ("ZERON_EDGE_TOKEN", "dev@own")];
        let config = resolve(&args, false, true, &vars).unwrap();
        assert_eq!(config.edge_url, "http://native.example.test");
        assert_eq!(config.ipc_port, 39012);
        assert_eq!(config.edge_token.as_deref(), Some("dev@own"));
        assert!(config.scaffold.is_none());
        assert_eq!(config.default_harness, HarnessId::ClaudeCode);
        args.edge_url = Some("http://flag.example.test".into());
        args.ipc_port = Some(39014);
        let config = resolve(&args, false, true, &vars).unwrap();
        assert_eq!(config.edge_url, "http://flag.example.test");
        assert_eq!(config.ipc_port, 39014);
    }

    #[test]
    fn agent_runtime_uses_own_profile_but_ipc_clients_keep_parent_port() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let own = dir.path().join("own");
        let vars = [("COMET_LOCAL_AGENT_RUNTIME", "1"), ("COMET_DATA_DIR", parent.to_str().unwrap()),
            ("ZERON_DATA_DIR", parent.to_str().unwrap()), ("COMET_IPC_PORT", "39016"), ("ZERON_IPC_PORT", "39016"),
            ("COMET_EDGE_URL", "https://parent-edge.example.test"), ("ZERON_EDGE_URL", "https://parent-edge.example.test"),
            ("COMET_SCAFFOLD_URL", "https://parent-control.example.test"), ("CREW_CONTROL_ORIGIN", "https://parent-control.example.test"),
            ("COMET_PROJECT_SCOPE", "parent-project"), ("CREW_PROJECT_SCOPE", "parent-project"),
            ("COMET_DEPLOYMENT_ID", "parent-deployment"), ("CREW_DEPLOYMENT_ID", "parent-deployment"),
            ("COMET_EDGE_TOKEN", "parent-secret"), ("ZERON_EDGE_TOKEN", "parent-secret"),
            ("COMET_HARNESS", "mock"), ("ZERON_HARNESS", "mock"), ("COMET_INFERENCE_MODE", "local"),
            ("COMET_SCAFFOLD_RUNTIME_BINARY", "/parent/runtime"), ("CREW_RUNTIME_BINARY", "/parent/runtime"),
            ("COMET_SCAFFOLD_RUNTIME_SHA256", "parent-hash"), ("CREW_RUNTIME_SHA256", "parent-hash"), ("ZERON_ORG_ID", "parent-org")];
        for args in [RuntimeArgs::default(), RuntimeArgs { data_dir: Some(own.clone()), ..Default::default() },
            RuntimeArgs { ipc_port: Some(39017), ..Default::default() }]
        {
            assert!(resolve(&args, true, true, &vars).is_err());
            assert!(!own.exists());
            assert!(!parent.exists());
        }
        std::fs::create_dir(&own).unwrap();
        let path = own.join("crew-runtime.json");
        let saved = br#"{"edgeUrl":"https://own-edge.example.test","scaffoldUrl":"https://own-control.example.test","projectScope":"own-project","ipcPort":39011}"#;
        std::fs::write(&path, saved).unwrap();
        let args = RuntimeArgs { data_dir: Some(own.clone()), ipc_port: Some(39017), ..Default::default() };
        let config = resolve(&args, true, true, &vars).unwrap();
        assert_eq!(config.data_dir, own);
        assert_eq!(config.ipc_port, 39017);
        assert_eq!(config.edge_url, "https://own-edge.example.test");
        assert!(config.edge_token.is_none());
        assert!(config.org_id.is_none());
        assert!(config.scaffold_runtime_artifact.is_none());
        assert_eq!(config.default_harness, HarnessId::Omp);
        assert!(config.managed_inference);
        let authority = config.scaffold.unwrap();
        assert_eq!(authority.control_origin, "https://own-control.example.test");
        assert_eq!(authority.project_scope, "own-project");
        assert!(authority.deployment_id.is_none());
        let persisted = std::fs::read(&path).unwrap();
        let settings: RuntimeSettings = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(settings.ipc_port, Some(39017));
        assert!(settings.deployment_id.is_none());
        assert!(settings.scaffold_runtime_artifact.is_none());
        let client = RuntimeArgs { ipc_port: None, ..args };
        assert_eq!(resolve(&client, false, false, &[("COMET_LOCAL_AGENT_RUNTIME", "1"), ("COMET_IPC_PORT", "39016")]).unwrap().ipc_port, 39016);
        assert_eq!(std::fs::read(path).unwrap(), persisted);
        assert!(!parent.exists());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    #[test]
    fn desktop_identity_cli_precedence_and_mock_isolation() {
        let home = tempfile::tempdir().unwrap();
        let profile = home.path().join("explicit-profile");
        std::fs::create_dir(&profile).unwrap();
        let saved = br#"{"edgeUrl":"https://saved.invalid","scaffoldUrl":"https://saved-control.invalid","projectScope":"saved","deploymentId":"saved-deployment","ipcPort":12345}"#;
        std::fs::write(profile.join("crew-runtime.json"), saved).unwrap();
        let env = |name: &str| -> Option<std::ffi::OsString> {
            Some(match name {
                "HOME" => home.path().as_os_str().to_owned(),
                "COMET_DATA_DIR" | "ZERON_DATA_DIR" => profile.as_os_str().to_owned(),
                "COMET_IPC_PORT" => "12346".into(),
                "ZERON_IPC_PORT" => "12347".into(),
                "COMET_EDGE_URL" | "ZERON_EDGE_URL" => "https://wrong.invalid".into(),
                "COMET_SCAFFOLD_URL" | "CREW_CONTROL_ORIGIN" => "https://wrong-control.invalid".into(),
                "COMET_PROJECT_SCOPE" | "CREW_PROJECT_SCOPE" => "wrong".into(),
                "COMET_DEPLOYMENT_ID" | "CREW_DEPLOYMENT_ID" => "wrong-deployment".into(),
                "COMET_EDGE_TOKEN" | "ZERON_EDGE_TOKEN" => "development-only".into(),
                "COMET_INFERENCE_MODE" => "local".into(),
                "COMET_HARNESS" | "ZERON_HARNESS" => "mock".into(),
                "COMET_SCAFFOLD_RUNTIME_BINARY" => "/wrong/binary".into(),
                _ => return None,
            })
        };
        let args = RuntimeArgs {
            data_dir: Some(profile.clone()), ipc_port: Some(12348),
            edge_url: Some("https://argument.invalid".into()),
            scaffold_url: Some("https://argument-control.invalid".into()),
            project_scope: Some("argument".into()), deployment_id: Some("argument".into()),
        };
        let headed = args.config_with_env(false, true, true, env).unwrap();
        let defaults = &crate::environment::DEFAULTS;
        assert_eq!(headed.data_dir, crate::paths::resolve_data_dir(env, true));
        assert_ne!(headed.data_dir, profile);
        assert_eq!(headed.ipc_port, defaults.ipc_port);
        assert_eq!(headed.edge_url, defaults.edge_url);
        assert_eq!(headed.scaffold.as_ref().map(|authority| authority.control_origin.as_str()), defaults.scaffold_url);
        assert_eq!(headed.scaffold.as_ref().map(|authority| authority.project_scope.as_str()), defaults.project_scope);
        assert_eq!(headed.scaffold.as_ref().and_then(|authority| authority.deployment_id.as_deref()), defaults.deployment_id);
        assert!(headed.edge_token.is_none() && headed.managed_inference);
        assert!(headed.scaffold_runtime_artifact.is_none());

        let cli = RuntimeArgs::default().config_with_env(false, false, false, env).unwrap();
        assert_eq!(cli.ipc_port, 12346);
        assert_eq!(cli.data_dir, profile);
        assert!(cli.scaffold.is_none() && cli.edge_token.is_none() && !cli.managed_inference);
        assert_eq!(cli.default_harness, HarnessId::Mock);
        assert_eq!(std::fs::read(profile.join("crew-runtime.json")).unwrap(), saved);

        let source_env = |name: &str| match name {
            "COMET_IPC_PORT" => Some("12346".into()), _ => None,
        };
        let source = RuntimeArgs { data_dir: Some(profile.clone()), ..Default::default() }
            .config_with_env(true, true, false, source_env).unwrap();
        assert_eq!(source.edge_url, "https://saved.invalid");
        assert_eq!(source.scaffold.unwrap().project_scope, "saved");
        assert_eq!(source.ipc_port, 12346);
        let persisted: RuntimeSettings = serde_json::from_slice(&std::fs::read(profile.join("crew-runtime.json")).unwrap()).unwrap();
        assert_eq!(persisted.ipc_port, Some(12345));
        assert_eq!(args.config_with_env(false, false, false, env).unwrap().ipc_port, 12348);
    }

    #[test]
    fn bootstrap_rejects_symlinks_without_consuming_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"private-bootstrap").unwrap();
        let link = dir.path().join("link");
        symlink(&source, &link).unwrap();
        let mut config = RuntimeArgs { data_dir: Some(dir.path().join("profile")), ..Default::default() }.config_with_env(false, false, false, |_| None).unwrap();
        assert!(configure_device(&mut config, &link).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"private-bootstrap");
    }

    #[test]
    fn bootstrap_requires_the_configured_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = RuntimeArgs {
            data_dir: Some(dir.path().join("profile")),
            edge_url: Some("https://edge.invalid".into()),
            scaffold_url: Some("https://control.invalid".into()),
            project_scope: Some("project".into()),
            deployment_id: Some("expected".into()),
            ..Default::default()
        }.config_with_env(false, false, false, |_| None).unwrap();
        let mut bootstrap = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        let mut value = serde_json::json!({
            "deviceJoinGrant":"private", "projectId":"project", "deploymentId":"other",
            "sessionId":"session", "deviceId":"device", "lifecycleEpoch":1,
            "sandboxId":"sandbox", "edgeUrl":"https://edge.invalid"
        });
        bootstrap.write_all(serde_json::to_string(&value).unwrap().as_bytes()).unwrap();
        assert!(configure_device(&mut config, bootstrap.path()).is_err());
        assert!(config.device.is_none());
        value["deploymentId"] = "expected".into();
        std::fs::write(bootstrap.path(), serde_json::to_vec(&value).unwrap()).unwrap();
        configure_device(&mut config, bootstrap.path()).unwrap();
        assert_eq!(config.device.as_ref().unwrap().deployment_id, "expected");
    }

    #[test]
    fn embedded_web_install_rejects_changed_assets_and_directory_links() {
        let root = tempfile::tempdir().unwrap();
        install_web_assets(root.path()).unwrap();
        install_web_assets(root.path()).unwrap();
        let server = root.path().join("web/server.mjs");
        std::fs::write(&server, b"changed").unwrap();
        assert!(install_web_assets(root.path()).is_err());
        assert_eq!(std::fs::read(&server).unwrap(), b"changed");
        let unbound = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), unbound.path().join("web")).unwrap();
        assert!(install_web_assets(unbound.path()).is_err());
        assert!(!unbound.path().join("runtime-ready").exists());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
