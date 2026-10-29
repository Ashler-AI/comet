//! `comet update` — authenticated release check and verified install using the
//! existing engine identity. Managed Linux installs swap their current link;
//! macOS installs replace the canonical Crew bundle for the package environment.

use anyhow::bail;
use zeron_engine::{Engine, EngineConfig, InstanceLock};
use zeron_update::{InstallKind, current_version, version_newer};

async fn update_authentication(
    config: &EngineConfig,
) -> anyhow::Result<(zeron_update::AccessTokenProvider, Option<InstanceLock>)> {
    // A running engine owns rotating credentials. Otherwise retain its lock
    // alongside the local Auth until the authenticated update work finishes.
    match zeron_rpc::connect_ipc(config.ipc_port, Some(&config.data_dir)).await {
        Ok(client) => {
            let client = std::sync::Arc::new(client);
            let edge_url = config.edge_url.clone();
            Ok((std::sync::Arc::new(move || {
                let client = client.clone();
                let edge_url = edge_url.clone();
                Box::pin(async move { Ok(client.update_access_token(&edge_url).await?) })
            }), None))
        }
        Err(_) => {
            let lock = InstanceLock::acquire(&config.data_dir)?;
            let auth = Engine::build_auth(config).await;
            anyhow::ensure!(!auth.device_mode(), "Desktop updates require local user authentication");
            Ok((std::sync::Arc::new(move || {
                let auth = auth.clone();
                Box::pin(async move { Ok(auth.access_token().await?) })
            }), Some(lock)))
        }
    }
}

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(config: &EngineConfig, check_only: bool) -> anyhow::Result<()> {
    anyhow::ensure!(zeron_update::AUTOMATIC_UPDATES_SUPPORTED, "Automatic updates are unsupported on this platform");
    let edge_url = config.edge_url.as_str();
    let (access_tokens, _lock) = update_authentication(config).await?;
    let manifest_token = access_tokens().await?;
    let manifest = zeron_update::fetch_latest(edge_url, Some(&manifest_token)).await?;
    let current = current_version();
    if !version_newer(&manifest.version, current) {
        println!(
            "comet {current} is up to date (latest: {}).",
            manifest.version
        );
        return Ok(());
    }
    println!("comet {current} → {} available", manifest.version);
    if check_only {
        std::process::exit(1);
    }

    let install = zeron_update::detect_install();
    if let Some(blocker) = install.desktop_update_blocker() {
        bail!("{blocker}");
    }
    match install {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                zeron_update::headless_artifact(&manifest.version)
            );
            let access_token = access_tokens().await?;
            zeron_update::stage_headless(edge_url, Some(&access_token), &manifest, &app_root).await?;
            zeron_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            // No more authenticated requests; the restarted engine must own the profile.
            drop(access_tokens);
            drop(_lock);
            match crate::daemon::restart() {
                Ok(()) => println!("engine service restarted."),
                Err(err) => println!(
                    "note: service restart failed ({err:#}) — restart the engine manually to finish."
                ),
            }
            Ok(())
        }
        InstallKind::MacApp { bundle } => {
            println!(
                "downloading {}…",
                zeron_update::mac_app_artifact(&manifest.version)
            );
            let access_token = access_tokens().await?;
            let staged = zeron_update::stage_mac_app(edge_url, Some(&access_token), &manifest, &config.data_dir).await?;
            let installed = zeron_update::apply_mac_app(&staged, &bundle)?;
            println!("updated {} — relaunch Crew to finish.", installed.display());
            Ok(())
        }
        #[cfg(windows)]
        InstallKind::WindowsPortable { .. } => {
            bail!("Automatic updates are unsupported on Windows");
        }
        InstallKind::Unmanaged => {
            bail!(
                "this binary is not update-managed (source build or hand-copied).\n\
                 Obtain a signed Crew package through the established private release workflow."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_authentication_retains_owner_and_observes_revocation() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = EngineConfig {
            data_dir: temp.path().into(), edge_url: "https://releases.example.test".into(),
            ipc_port: 0, edge_token: Some("local-update-token".into()),
            default_harness: zeron_engine::HarnessId::Mock, org_id: None,
            workos_client_id: None, scaffold: None, device: None,
            managed_inference: false, device_bootstrap_file: None,
        };
        let (tokens, lock) = update_authentication(&config).await.unwrap();
        assert!(lock.is_some());
        assert!(InstanceLock::acquire(&config.data_dir).is_err());
        assert_eq!(tokens().await.unwrap(), "local-update-token");
        drop(tokens);
        drop(lock);
        let engine_lock = InstanceLock::acquire(&config.data_dir).unwrap();

        let mut auth_config = zeron_engine::AuthConfig::new(&config.edge_url, temp.path());
        auth_config.allow_dev_auth = true;
        auth_config.dev_user_id = "ipc-update-token".into();
        let auth = zeron_engine::Auth::new(auth_config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.ipc_port = listener.local_addr().unwrap().port();
        let ipc_token = zeron_rpc::ipc_auth::issue_token(temp.path()).unwrap();
        let server = tokio::spawn(zeron_rpc::serve_ws_listener(
            listener, std::sync::Arc::new(zeron_engine::rpc::AuthRpc::new(auth.clone())),
            ipc_token,
        ));
        let (tokens, lock) = update_authentication(&config).await.unwrap();
        assert!(lock.is_none());
        assert_eq!(tokens().await.unwrap(), "ipc-update-token");
        auth.sign_out();
        assert!(tokens().await.is_err());
        server.abort();
        drop(engine_lock);
    }
}
