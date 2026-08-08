//! `comet update` — check for and apply a newer release, natively (the same
//! flow `edge/src/install.sh` performs: download → verify → symlink swap →
//! service restart). macOS app bundles swap the bundle instead; source builds
//! are report-only.

use anyhow::bail;
use comet_engine::{Engine, EngineConfig};
use comet_update::{InstallKind, current_version, version_newer};

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(config: EngineConfig, check_only: bool) -> anyhow::Result<()> {
    // Authentication comes from the persisted Comet login. Keep its renewal
    // task alive for the whole update and resolve the bearer separately for
    // each release request instead of snapshotting a GCS access token.
    let auth = Engine::build_auth(&config).await;
    let _auth_refresh = auth.spawn_refresh_loop();
    let manifest_token = auth.access_token().await;
    let manifest = comet_update::fetch_latest(&config.edge_url, manifest_token.as_deref()).await?;
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

    match comet_update::detect_install() {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                comet_update::headless_artifact(&manifest.version)
            );
            let artifact_token = auth.access_token().await;
            comet_update::stage_headless(
                &config.edge_url,
                artifact_token.as_deref(),
                &manifest,
                &app_root,
            )
            .await?;
            comet_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            match comet_update::restart_service() {
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
                comet_update::mac_app_artifact(&manifest.version)
            );
            let data_dir = std::env::var_os("COMET_DATA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(super::dirs_data_dir);
            let artifact_token = auth.access_token().await;
            let staged = comet_update::stage_mac_app(
                &config.edge_url,
                artifact_token.as_deref(),
                &manifest,
                &data_dir,
            )
            .await?;
            comet_update::apply_mac_app(&staged, &bundle)?;
            println!(
                "updated {}. Relaunch Ashler Comet to finish.",
                bundle.display()
            );
            Ok(())
        }
        InstallKind::Unmanaged => {
            bail!(
                "this binary is not update-managed (source build or hand-copied).\n\
                 Install the current Ashler Comet release from the internal release channel."
            )
        }
    }
}
