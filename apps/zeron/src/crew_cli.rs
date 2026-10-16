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

fn setting(name: &str, upstream: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var(upstream).ok().filter(|value| !value.trim().is_empty()))
}

impl RuntimeArgs {
    pub fn config(&self) -> anyhow::Result<EngineConfig> {
        let data_dir = std::path::absolute(self.data_dir.clone().unwrap_or_else(crate::paths::data_dir))?;
        let settings_path = data_dir.join("crew-runtime.json");
        let saved = match std::fs::read(&settings_path) {
            Ok(bytes) => serde_json::from_slice::<RuntimeSettings>(&bytes)
                .context("invalid Crew runtime configuration")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RuntimeSettings::default(),
            Err(error) => return Err(error.into()),
        };
        let edge_url = self.edge_url.clone()
            .or_else(|| setting("COMET_EDGE_URL", "ZERON_EDGE_URL"))
            .or(saved.edge_url)
            .unwrap_or_else(|| "http://127.0.0.1:27941".into());
        let scaffold_url = self.scaffold_url.clone()
            .or_else(|| setting("COMET_SCAFFOLD_URL", "CREW_CONTROL_ORIGIN"))
            .or(saved.scaffold_url);
        let project_scope = self.project_scope.clone()
            .or_else(|| setting("COMET_PROJECT_SCOPE", "CREW_PROJECT_SCOPE"))
            .or(saved.project_scope);
        let deployment_id = self.deployment_id.clone()
            .or_else(|| setting("COMET_DEPLOYMENT_ID", "CREW_DEPLOYMENT_ID"))
            .or(saved.deployment_id);
        let environment_port = setting("COMET_IPC_PORT", "ZERON_IPC_PORT")
            .map(|port| port.parse::<u16>().context("invalid Crew IPC port")).transpose()?;
        let ipc_port = self.ipc_port.or(environment_port).or(saved.ipc_port).unwrap_or(27954);
        if ipc_port == 0 { bail!("Crew IPC port must be nonzero"); }
        let edge_token = setting("COMET_EDGE_TOKEN", "ZERON_EDGE_TOKEN");
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
        let managed_inference = std::env::var("COMET_INFERENCE_MODE").as_deref() != Ok("local")
            && harness != HarnessId::Mock;
        let artifact_path = setting("COMET_SCAFFOLD_RUNTIME_BINARY", "CREW_RUNTIME_BINARY");
        let artifact_hash = setting("COMET_SCAFFOLD_RUNTIME_SHA256", "CREW_RUNTIME_SHA256");
        let scaffold_runtime_artifact = match (artifact_path, artifact_hash) {
            (Some(path), Some(hash)) => Some((PathBuf::from(path), hash)),
            (None, None) => saved.scaffold_runtime_artifact,
            _ => bail!("runtime artifact requires both its binary path and SHA256"),
        };
        // These are non-secret launch coordinates, retained for Finder launches
        // and device restarts. Credential storage belongs to the auth service.
        if scaffold.is_some() {
            std::fs::create_dir_all(&data_dir)?;
            let settings = RuntimeSettings {
                edge_url: Some(edge_url.clone()), scaffold_url, project_scope,
                deployment_id, ipc_port: Some(ipc_port),
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
            org_id: setting("COMET_PROJECT_SCOPE", "ZERON_ORG_ID"),
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
        let mut config = RuntimeArgs { data_dir: Some(dir.path().join("profile")), ..Default::default() }.config().unwrap();
        assert!(configure_device(&mut config, &link).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"private-bootstrap");
    }
}
