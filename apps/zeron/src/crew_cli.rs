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
}

impl RuntimeArgs {
    pub fn config(&self, persist: bool, own_runtime: bool) -> anyhow::Result<EngineConfig> {
        self.config_with_env(persist, own_runtime, |name| std::env::var(name).ok())
    }

    fn config_with_env(
        &self,
        persist: bool,
        own_runtime: bool,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<EngineConfig> {
        let isolated = (persist || own_runtime) && env("COMET_LOCAL_AGENT_RUNTIME").as_deref() == Some("1");
        if isolated && (self.data_dir.as_ref().is_none_or(|dir| dir.as_os_str().is_empty()) || self.ipc_port.is_none()) {
            bail!("Crew agent launches require explicit --data-dir and --ipc-port for an independent runtime");
        }
        // Agent environments describe their parent engine, not this profile.
        // IPC-only clients intentionally retain those coordinates.
        let setting = |name: &str, upstream: &str| {
            if isolated { return None; }
            env(name).filter(|value| !value.trim().is_empty())
                .or_else(|| env(upstream).filter(|value| !value.trim().is_empty()))
        };
        let data_dir = std::path::absolute(self.data_dir.clone().unwrap_or_else(crate::paths::data_dir))?;
        let settings_path = data_dir.join("crew-runtime.json");
        let saved = match std::fs::read(&settings_path) {
            Ok(bytes) => serde_json::from_slice::<RuntimeSettings>(&bytes)
                .context("invalid Crew runtime configuration")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RuntimeSettings::default(),
            Err(error) => return Err(error.into()),
        };
        let edge_token = setting("COMET_EDGE_TOKEN", "ZERON_EDGE_TOKEN");
        let mut edge_url = self.edge_url.clone()
            .or_else(|| setting("COMET_EDGE_URL", "ZERON_EDGE_URL"))
            .or(saved.edge_url);
        let mut scaffold_url = self.scaffold_url.clone()
            .or_else(|| setting("COMET_SCAFFOLD_URL", "CREW_CONTROL_ORIGIN"))
            .or(saved.scaffold_url);
        let mut project_scope = self.project_scope.clone()
            .or_else(|| setting("COMET_PROJECT_SCOPE", "CREW_PROJECT_SCOPE"))
            .or(saved.project_scope);
        let mut deployment_id = self.deployment_id.clone()
            .or_else(|| setting("COMET_DEPLOYMENT_ID", "CREW_DEPLOYMENT_ID"))
            .or(saved.deployment_id);
        if edge_url.is_none() && scaffold_url.is_none() && project_scope.is_none()
            && deployment_id.is_none() && edge_token.is_none()
        {
            let defaults = &crate::environment::DEFAULTS;
            edge_url = Some(defaults.edge_url.into());
            scaffold_url = defaults.scaffold_url.map(Into::into);
            project_scope = defaults.project_scope.map(Into::into);
            deployment_id = defaults.deployment_id.map(Into::into);
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
        let ipc_port = self.ipc_port.or(environment_port).or(saved.ipc_port)
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
        let harness = match setting("COMET_HARNESS", "ZERON_HARNESS").as_deref() {
            Some("mock") => HarnessId::Mock,
            Some("codex") => HarnessId::Codex,
            Some("omp") => HarnessId::Omp,
            Some("claude-code") => HarnessId::ClaudeCode,
            Some(value) => serde_json::from_value(serde_json::Value::String(value.into()))
                .context("unknown Crew harness")?,
            None if scaffold.is_some() => HarnessId::Omp,
            None => HarnessId::ClaudeCode,
        };
        let managed_inference = (isolated || env("COMET_INFERENCE_MODE").as_deref() != Some("local"))
            && harness != HarnessId::Mock;
        // These are non-secret launch coordinates, retained for Finder launches
        // and device restarts. Credential storage belongs to the auth service.
        if persist && scaffold.is_some() {
            std::fs::create_dir_all(&data_dir)?;
            let settings = RuntimeSettings {
                edge_url: Some(edge_url.clone()), scaffold_url, project_scope,
                deployment_id, ipc_port: Some(ipc_port),
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
            org_id: setting("COMET_PROJECT_SCOPE", "ZERON_ORG_ID"),
            workos_client_id: None, scaffold, device: None, managed_inference,
            device_bootstrap_file: None,
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


#[cfg(test)]
mod runtime_tests {
    use super::*;

    fn resolve(args: &RuntimeArgs, persist: bool, own_runtime: bool, vars: &[(&str, &str)]) -> anyhow::Result<EngineConfig> {
        args.config_with_env(persist, own_runtime, |name| {
            vars.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
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
            ("COMET_IPC_PORT", "39012"), ("ZERON_IPC_PORT", "39013"), ("ZERON_EDGE_TOKEN", "dev@own"), ("ZERON_HARNESS", "mock")];
        let config = resolve(&args, false, true, &vars).unwrap();
        assert_eq!(config.edge_url, "http://native.example.test");
        assert_eq!(config.ipc_port, 39012);
        assert_eq!(config.edge_token.as_deref(), Some("dev@own"));
        assert!(config.scaffold.is_none());
        assert_eq!(config.default_harness, HarnessId::Mock);
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
            ("ZERON_ORG_ID", "parent-org")];
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
        let client = RuntimeArgs { ipc_port: None, ..args };
        assert_eq!(resolve(&client, false, false, &[("COMET_LOCAL_AGENT_RUNTIME", "1"), ("COMET_IPC_PORT", "39016")]).unwrap().ipc_port, 39016);
        assert_eq!(std::fs::read(path).unwrap(), persisted);
        assert!(!parent.exists());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn bootstrap_rejects_symlinks_without_consuming_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"private-bootstrap").unwrap();
        let link = dir.path().join("link");
        symlink(&source, &link).unwrap();
        let mut config = RuntimeArgs { data_dir: Some(dir.path().join("profile")), ..Default::default() }.config(false, false).unwrap();
        assert!(configure_device(&mut config, &link).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"private-bootstrap");
    }

}
