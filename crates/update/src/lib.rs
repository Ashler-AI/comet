//! comet-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`comet update`), and the UI.
//! Release artifacts are stored privately in GCS and served through the
//! authenticated Comet edge (see `.github/workflows/release.yml` and
//! `install.sh`). Each request resolves the persisted Comet login; the edge
//! obtains its own short-lived GCS authorization, so no storage credential is
//! persisted on the device.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.comet-native/app/<ver>` + `current` symlink — the curl|sh
//!   installer): download the headless tarball into a new versioned dir, flip
//!   the symlink, restart the service. Same flow the installer script performs,
//!   natively.
//! - **MacApp** (running out of `Crew.app`): download the app tarball, swap the
//!   bundle directory, relaunch. Driven by the UI.
//! - **Unmanaged** (source builds, hand-copied binaries): report only.
//!
//! Local agent CLIs (OMP, Claude Code, Codex) are tracked on the same check
//! cadence and updated exclusively through their own self-updaters — Comet
//! never swaps a vendor binary itself (see [`HarnessSpec`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use anyhow::{Context as _, bail};
use futures::{StreamExt as _, future::BoxFuture};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Background check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Retry sooner after a failed check (offline boot, transient edge error).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// First check waits out engine boot (room joins, doc re-sync).
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// A `--version` probe must be instant; a hung binary must not stall the tick.
const VERSION_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Vendor release-channel metadata lookups (GitHub / npm registry).
const CHANNEL_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// A self-updater downloads a full CLI (OMP is ~150 MB) — allow slow links.
const SELF_UPDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// Private platform-specific release manifest written by the release workflow.
/// Desktop-only releases advance macOS without advertising an absent Linux host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Artifact file name → required digest metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub sha256: String,
}

/// Artifact-name platform pair — `uname`-style strings matching the packaging
/// scripts: `linux-x86_64`, `linux-aarch64`, `macos-arm64`.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

/// `comet-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("comet-{version}-{os}-{arch}.tar.gz")
}

/// `comet-<ver>-macos-<arch>-app.tar.gz` — the packaged `Crew.app` bundle.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("comet-{version}-macos-{arch}-app.tar.gz")
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer — a garbage `latest.txt` must not
/// trigger an update loop.
pub fn version_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let nums: Vec<u64> = v
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (!nums.is_empty()).then_some(nums)
    }
    match (parts(latest), parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}
fn release_manifest_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "desktop-manifest.json"
    } else {
        "scaffold-manifest.json"
    }
}

fn releases_base(edge_url: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(edge_url).context("invalid Comet edge URL")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.host_str().is_none()
        || (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        bail!("Comet edge URL must be an exact HTTPS origin (HTTP is loopback-only)");
    }
    url.set_path("/api/releases");
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn authorized_get(
    client: &reqwest::Client,
    url: &str,
    access_token: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = client.get(url);
    match access_token.filter(|value| !value.trim().is_empty()) {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

/// Fetch the latest compatible release manifest through the authenticated Comet edge.
pub async fn fetch_latest(edge_url: &str, access_token: Option<&str>) -> anyhow::Result<Manifest> {
    let base = releases_base(edge_url)?;
    let manifest_url = format!("{base}/{}", release_manifest_name());
    let client = http_client()?;
    let manifest: Manifest = authorized_get(&client, &manifest_url, access_token)
        .send()
        .await
        .with_context(|| format!("fetching {manifest_url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {manifest_url}"))?
        .json()
        .await
        .context("parsing private release manifest")?;
    if manifest.version.trim().is_empty() {
        bail!("manifest.json has an empty version");
    }
    if manifest.files.is_empty() {
        bail!("manifest.json has no checksummed release files");
    }
    Ok(manifest)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("comet/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building release HTTP client")
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.comet-native/app/<ver>/comet` behind the `current` symlink
    /// (curl|sh installer / a previous `comet update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = home.join(".comet-native").join("app");
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream `{private release feed}/<file>` to `dest`, verifying its manifest
/// SHA-256. Writes through a `.partial` sidecar so an interrupted download never
/// leaves a plausible-looking artifact behind.
pub async fn download_release_file(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = format!("{}/{file}", releases_base(edge_url)?);
    let expected = manifest
        .files
        .get(file)
        .map(|metadata| metadata.sha256.as_str())
        .filter(|digest| !digest.trim().is_empty())
        .with_context(|| format!("release manifest has no SHA-256 for {file}"))?;
    let partial = dest.with_extension("partial");
    let client = http_client()?;
    let resp = authorized_get(&client, &url, access_token)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.context("writing download")?;
    }
    out.flush().await.ok();
    drop(out);
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        tokio::fs::remove_file(&partial).await.ok();
        bail!("checksum mismatch for {file}: expected {expected}, got {actual}");
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
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

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    let version = &manifest.version;
    let dest = app_root.join(version);
    if dest.join("comet").exists() {
        return Ok(dest);
    }
    let file = headless_artifact(version);
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(edge_url, access_token, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("comet").is_file() {
            bail!("tarball {file} did not contain a comet binary");
        }
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_) if dest.join("comet").exists() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("comet").exists() {
            bail!("{} is not a staged install", target.display());
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        bail!("managed installs are unix-only");
    }
}

/// Restart the installed engine service (the same units `comet daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary.
pub fn restart_service() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/ai.ashler.comet")],
        )
    } else {
        run("systemctl", &["--user", "restart", "comet-native.service"])
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Crew.app`
/// (idempotent). Returns the staged bundle path.
pub async fn stage_mac_app(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let version = &manifest.version;
    let dir = data_dir.join("updates").join(version);
    let staged = dir.join("Crew.app");
    if staged.join("Contents/MacOS/comet").exists() {
        return Ok(staged);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = mac_app_artifact(version);
    let tarball = dir.join(&file);
    download_release_file(edge_url, access_token, manifest, &file, &tarball).await?;
    run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&tarball).ok();
    if !staged.join("Contents/MacOS/comet").exists() {
        bail!("app tarball {file} did not contain Crew.app");
    }
    Ok(staged)
}

/// Install the staged bundle next to the current app, preserving metadata and
/// migrating legacy `Comet.app` installs to the user-facing `Crew.app` name.
/// Any existing target bundle is restored if the replacement fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<PathBuf> {
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let target_name = staged
        .file_name()
        .context("staged app bundle has no name")?;
    let target = parent.join(target_name);
    let current_name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let target_name = target_name.to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{target_name}.new-{pid}"));
    let old = parent.join(format!(".{current_name}.old-{pid}"));
    let displaced_target = (target.as_path() != bundle && target.exists())
        .then(|| parent.join(format!(".{target_name}.old-{pid}")));
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&old);
    if let Some(displaced) = displaced_target.as_ref() {
        let _ = std::fs::remove_dir_all(displaced);
    }
    run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    )?;
    if let Some(displaced) = displaced_target.as_ref() {
        std::fs::rename(&target, displaced).context("moving the existing target app aside")?;
    }
    if let Err(err) = std::fs::rename(bundle, &old) {
        if let Some(displaced) = displaced_target.as_ref() {
            let _ = std::fs::rename(displaced, &target);
        }
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("moving the current app aside");
    }
    if let Err(err) = std::fs::rename(&fresh, &target) {
        let _ = std::fs::rename(&old, bundle);
        if let Some(displaced) = displaced_target.as_ref() {
            let _ = std::fs::rename(displaced, &target);
        }
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    if let Some(displaced) = displaced_target {
        let _ = std::fs::remove_dir_all(displaced);
    }
    Ok(target)
}

/// Detached relauncher: waits for THIS process to exit, then `open`s the bundle.
/// (Opening before exit would race the single-instance engine lock and the IPC
/// port.) The caller quits the app after this returns.
pub fn relaunch_app_after_exit(bundle: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id();
        let script = format!(
            "while /bin/kill -0 {pid} 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"{}\"",
            bundle.display()
        );
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = bundle;
}

// ---------------------------------------------------------------------------
// Local agent CLIs (harnesses)
// ---------------------------------------------------------------------------

/// One local agent CLI as reported over the `UpdateStatus` stream. Version
/// facts only, mirroring [`UpdateStatus`] — apply progress is owned by whoever
/// drives the update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessStatus {
    pub id: String,
    pub name: String,
    /// Resolved executable; `None` = not installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Installed version is below this engine's compatibility floor.
    #[serde(default)]
    pub update_required: bool,
}

/// Where a harness's "latest" version comes from. Only used to *report*
/// updates — applying one always goes through the CLI's own self-updater, so
/// install-method quirks (npm prefixes, native installers) stay the vendor's
/// problem.
#[derive(Debug, Clone)]
pub enum ReleaseChannel {
    /// `api.github.com/repos/<repo>/releases/latest` → `tag_name`, leading `v`
    /// stripped. `GITHUB_TOKEN`/`GH_TOKEN` ride along when set — VPS fleets
    /// behind shared IPs hit the unauthenticated rate limit.
    GitHub { repo: &'static str },
    /// `registry.npmjs.org/<package>/latest` → `version`.
    Npm { package: &'static str },
}

/// A local agent CLI the updater tracks. The engine supplies these —
/// executable resolution lives in `comet-harness` (login-shell PATH snapshot,
/// version-manager bins), which this crate must not depend on.
#[derive(Clone)]
pub struct HarnessSpec {
    pub id: &'static str,
    pub name: &'static str,
    /// Resolve the installed executable (`None` = not installed).
    pub resolve: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
    pub channel: ReleaseChannel,
    /// The CLI's own self-updater, spawned as `<exe> <args…>`.
    pub self_update_args: &'static [&'static str],
    /// Oldest version this engine can drive (`None` = no floor).
    pub min_version: Option<&'static str>,
}

/// First `X.Y.Z…` dotted-numeric token in a `--version` output — tolerates
/// every observed shape: `omp/17.2.9`, `2.1.224 (Claude Code)`,
/// `codex-cli 0.146.0`, `v17.2.12`. Two segments ("2026.08") is a date or
/// counter, never a CLI version.
fn extract_version(output: &str) -> Option<String> {
    output
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|token| token.trim_matches('.'))
        .find(|token| {
            token.split('.').count() >= 3
                && token
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// `<exe> --version` → parsed version, cached by (path → mtime) so the 6h tick
/// and settings-driven refreshes never respawn an unchanged binary.
async fn probe_cli_version(
    cache: &Mutex<HashMap<PathBuf, (SystemTime, Option<String>)>>,
    exe: &Path,
) -> Option<String> {
    let mtime = std::fs::metadata(exe).ok()?.modified().ok()?;
    if let Some((seen, version)) = lock(cache).get(exe)
        && *seen == mtime
    {
        return version.clone();
    }
    let output = tokio::time::timeout(
        VERSION_PROBE_TIMEOUT,
        tokio::process::Command::new(exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok);
    let version = output.as_ref().and_then(|out| {
        extract_version(&String::from_utf8_lossy(&out.stdout))
            .or_else(|| extract_version(&String::from_utf8_lossy(&out.stderr)))
    });
    lock(cache).insert(exe.to_path_buf(), (mtime, version.clone()));
    version
}

async fn fetch_channel_latest(
    client: &reqwest::Client,
    channel: &ReleaseChannel,
) -> anyhow::Result<String> {
    match channel {
        ReleaseChannel::GitHub { repo } => {
            let mut request = client
                .get(format!(
                    "https://api.github.com/repos/{repo}/releases/latest"
                ))
                .header("accept", "application/vnd.github+json")
                .timeout(CHANNEL_FETCH_TIMEOUT);
            if let Some(token) = std::env::var("GITHUB_TOKEN")
                .ok()
                .or_else(|| std::env::var("GH_TOKEN").ok())
                .filter(|token| !token.trim().is_empty())
            {
                request = request.bearer_auth(token.trim().to_string());
            }
            let body: serde_json::Value = request.send().await?.error_for_status()?.json().await?;
            let tag = body
                .get("tag_name")
                .and_then(serde_json::Value::as_str)
                .with_context(|| format!("no tag_name in {repo} latest release"))?;
            Ok(tag.trim_start_matches('v').to_string())
        }
        ReleaseChannel::Npm { package } => {
            let body: serde_json::Value = client
                .get(format!("https://registry.npmjs.org/{package}/latest"))
                .timeout(CHANNEL_FETCH_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let version = body
                .get("version")
                .and_then(serde_json::Value::as_str)
                .with_context(|| format!("no version in {package} dist-tag"))?;
            Ok(version.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Update preferences + boot stamp
// ---------------------------------------------------------------------------

/// `{data_dir}/update-prefs.json`. One knob so far: whether agent CLIs
/// self-refresh after a Comet update lands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePrefs {
    #[serde(default = "default_true")]
    harness_auto_update: bool,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self {
            harness_auto_update: true,
        }
    }
}

fn default_true() -> bool {
    true
}

const PREFS_FILE: &str = "update-prefs.json";

fn load_prefs(data_dir: &Path) -> UpdatePrefs {
    std::fs::read(data_dir.join(PREFS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store_prefs(data_dir: &Path, prefs: &UpdatePrefs) {
    let path = data_dir.join(PREFS_FILE);
    if let Ok(bytes) = serde_json::to_vec_pretty(prefs)
        && let Err(err) = std::fs::write(&path, bytes)
    {
        tracing::warn!(error = %err, path = %path.display(), "update prefs not persisted");
    }
}

/// `COMET_UPDATE_HARNESSES=0|false|no` — operator kill switch for the
/// post-update agent refresh (daemon spelling, mirrors `COMET_AUTO_UPDATE`).
fn harness_updates_disabled_by_env() -> bool {
    std::env::var("COMET_UPDATE_HARNESSES")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(false)
}

/// True exactly when a DIFFERENT Comet version last booted from `data_dir`
/// (the current version is stamped either way). A first boot is not a
/// transition — a fresh install just bootstrapped its agents.
pub fn version_transition(data_dir: &Path) -> bool {
    let stamp = data_dir.join("last-boot-version");
    let previous = std::fs::read_to_string(&stamp)
        .map(|s| s.trim().to_string())
        .ok();
    if previous.as_deref() != Some(current_version())
        && let Err(err) = std::fs::write(&stamp, current_version())
    {
        tracing::warn!(error = %err, "boot-version stamp not persisted");
    }
    matches!(previous, Some(prev) if prev != current_version())
}

// ---------------------------------------------------------------------------
// Engine-side checker
// ---------------------------------------------------------------------------

/// What the engine reports over the `UpdateStatus` stream. Version facts only —
/// download/apply progress is owned by whoever drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Local agent CLIs on this device — populated on the same cadence as the
    /// release check. Empty until the first tick (and over relays from
    /// engines predating the field).
    #[serde(default)]
    pub harnesses: Vec<HarnessStatus>,
    /// The persisted "refresh agents after a Comet update" toggle.
    #[serde(default = "default_true")]
    pub harness_auto_update: bool,
}

impl UpdateStatus {
    fn initial(harness_auto_update: bool) -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
            harnesses: Vec::new(),
            harness_auto_update,
        }
    }
}

/// `COMET_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("COMET_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;
/// Resolves the current Comet login bearer for each release request, allowing
/// the credential to rotate without rebuilding the updater.
pub type AccessTokenSource = Arc<dyn Fn() -> BoxFuture<'static, Option<String>> + Send + Sync>;
type HarnessVersionCache = Arc<Mutex<HashMap<PathBuf, (SystemTime, Option<String>)>>>;

/// Background release checker: polls `{edge}/releases` on a 6h cadence and
/// publishes [`UpdateStatus`] over a watch channel (the `UpdateStatus` RPC
/// stream). Managed installs with `COMET_AUTO_UPDATE` set stage + apply + service
/// restart on their own — but only in a quiet window: while `quiescent` reports
/// activity, the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    edge_url: String,
    data_dir: PathBuf,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    quiescent: Option<QuiescentCheck>,
    access_token: AccessTokenSource,
    /// Agent CLIs tracked alongside the app's own releases.
    harnesses: Arc<Vec<HarnessSpec>>,
    /// `--version` probe results keyed by (path → mtime).
    version_cache: HarnessVersionCache,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(
        edge_url: String,
        data_dir: PathBuf,
        quiescent: Option<QuiescentCheck>,
        access_token: AccessTokenSource,
        harnesses: Vec<HarnessSpec>,
    ) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial(
            load_prefs(&data_dir).harness_auto_update,
        ));
        let updater = Self {
            edge_url,
            data_dir,
            status_tx: Arc::new(status_tx),
            quiescent,
            access_token,
            harnesses: Arc::new(harnesses),
            version_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let for_loop = updater.clone();
        tokio::spawn(async move { for_loop.check_loop().await });
        updater
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn current_access_token(&self) -> Option<String> {
        (self.access_token)().await
    }

    async fn check_loop(&self) {
        tokio::time::sleep(CHECK_INITIAL_DELAY).await;
        loop {
            let ok = self.check_once().await;
            if ok
                && self.status_tx.borrow().update_available
                && auto_update_enabled()
                && let InstallKind::Managed { .. } = detect_install()
            {
                self.auto_apply_when_idle().await;
            }
            tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }).await;
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and reuses the staged dir, keeping the
    /// idle→restart gap to well under a second.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            let manifest_token = self.current_access_token().await;
            match fetch_latest(&self.edge_url, manifest_token.as_deref()).await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    let artifact_token = self.current_access_token().await;
                    if let Err(err) = stage_headless(
                        &self.edge_url,
                        artifact_token.as_deref(),
                        &manifest,
                        &app_root,
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "auto-update staging failed");
                        return;
                    }
                }
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update staging fetch failed");
                    return;
                }
            }
        }
        let mut deferred = false;
        while !self.quiescent_now() {
            if !deferred {
                deferred = true;
                tracing::info!("auto-update deferred: sessions or terminals active");
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
        match self.apply().await {
            Ok(version) => {
                tracing::info!(%version, "auto-update applied; service restarting")
            }
            Err(err) => tracing::warn!(error = %err, "auto-update failed"),
        }
    }

    /// One check; returns false on fetch failure (retry sooner). Agent-CLI
    /// rows ride the same tick: version probes are mtime-cached and channel
    /// lookups short-timeout, so the added latency is a few seconds, 4×/day.
    async fn check_once(&self) -> bool {
        let access_token = self.current_access_token().await;
        match fetch_latest(&self.edge_url, access_token.as_deref()).await {
            Ok(manifest) => {
                let harnesses = self.harness_statuses().await;
                let harness_auto_update = self.status_tx.borrow().harness_auto_update;
                let status = UpdateStatus {
                    current_version: current_version().to_string(),
                    update_available: version_newer(&manifest.version, current_version()),
                    latest_version: Some(manifest.version),
                    checked_at: Some(now_ms()),
                    error: None,
                    harnesses,
                    harness_auto_update,
                };
                if status.update_available {
                    tracing::info!(
                        latest = status.latest_version.as_deref().unwrap_or(""),
                        current = %status.current_version,
                        "update available"
                    );
                }
                self.status_tx.send_replace(status);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest_token = self.current_access_token().await;
        let manifest = fetch_latest(&self.edge_url, manifest_token.as_deref()).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        let artifact_token = self.current_access_token().await;
        stage_headless(
            &self.edge_url,
            artifact_token.as_deref(),
            &manifest,
            &app_root,
        )
        .await?;
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(manifest.version)
    }

    /// Download and verify a macOS app update through the authenticated edge.
    pub async fn stage_mac_update(&self) -> anyhow::Result<PathBuf> {
        let manifest_token = self.current_access_token().await;
        let manifest = fetch_latest(&self.edge_url, manifest_token.as_deref()).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        let artifact_token = self.current_access_token().await;
        stage_mac_app(
            &self.edge_url,
            artifact_token.as_deref(),
            &manifest,
            &self.data_dir,
        )
        .await
    }

    /// Effective post-update agent refresh switch: the persisted toggle,
    /// unless the operator env kill switch overrides it.
    pub fn harness_auto_update(&self) -> bool {
        self.status_tx.borrow().harness_auto_update && !harness_updates_disabled_by_env()
    }

    pub fn set_harness_auto_update(&self, enabled: bool) {
        store_prefs(
            &self.data_dir,
            &UpdatePrefs {
                harness_auto_update: enabled,
            },
        );
        self.status_tx
            .send_modify(|status| status.harness_auto_update = enabled);
    }

    async fn harness_statuses(&self) -> Vec<HarnessStatus> {
        let client = http_client().ok();
        let mut statuses = Vec::with_capacity(self.harnesses.len());
        for spec in self.harnesses.iter() {
            statuses.push(self.harness_status(spec, client.as_ref()).await);
        }
        statuses
    }

    async fn harness_status(
        &self,
        spec: &HarnessSpec,
        client: Option<&reqwest::Client>,
    ) -> HarnessStatus {
        let exe = (spec.resolve)();
        let installed_version = match &exe {
            Some(exe) => probe_cli_version(&self.version_cache, exe).await,
            None => None,
        };
        // "Latest" is only interesting for something that is installed.
        let latest_version = match (&exe, client) {
            (Some(_), Some(client)) => match fetch_channel_latest(client, &spec.channel).await {
                Ok(version) => Some(version),
                Err(err) => {
                    tracing::debug!(harness = spec.id, error = %err, "latest-version lookup failed");
                    None
                }
            },
            _ => None,
        };
        let update_available = matches!(
            (&latest_version, &installed_version),
            (Some(latest), Some(installed)) if version_newer(latest, installed)
        );
        let update_required = matches!(
            (spec.min_version, &installed_version),
            (Some(min), Some(installed)) if version_newer(min, installed)
        );
        HarnessStatus {
            id: spec.id.to_string(),
            name: spec.name.to_string(),
            path: exe.map(|p| p.display().to_string()),
            installed_version,
            latest_version,
            update_available,
            update_required,
        }
    }

    /// Run `id`'s own self-updater and fold its refreshed row into the current
    /// status frame. The child inherits this process's env, so a configured
    /// `GITHUB_TOKEN` reaches `omp update`'s release-metadata requests.
    pub async fn update_harness(&self, id: &str) -> anyhow::Result<HarnessStatus> {
        let spec = self
            .harnesses
            .iter()
            .find(|spec| spec.id == id)
            .with_context(|| format!("unknown harness '{id}'"))?;
        let exe = (spec.resolve)()
            .with_context(|| format!("{} is not installed on this device", spec.name))?;
        let output = tokio::time::timeout(
            SELF_UPDATE_TIMEOUT,
            tokio::process::Command::new(&exe)
                .args(spec.self_update_args)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("{} self-update timed out", spec.name))?
        .with_context(|| format!("spawning {}", exe.display()))?;
        if !output.status.success() {
            bail!(
                "{} self-update failed ({}): {}",
                spec.name,
                output.status,
                output_tail(&output)
            );
        }
        // The swap happened under the CLI's own updater — drop the stale probe.
        lock(&self.version_cache).remove(&exe);
        let client = http_client().ok();
        let status = self.harness_status(spec, client.as_ref()).await;
        let updated = status.clone();
        self.status_tx.send_modify(move |current| {
            match current
                .harnesses
                .iter_mut()
                .find(|row| row.id == updated.id)
            {
                Some(slot) => *slot = updated,
                None => current.harnesses.push(updated),
            }
        });
        Ok(status)
    }

    /// The "on update" contract: refresh every installed agent once after a
    /// Comet version transition. Sequential — vendor updaters contend on their
    /// own locks — and failures are logged, never fatal.
    pub fn spawn_post_update_refresh(&self) {
        let updater = self.clone();
        tokio::spawn(async move {
            // Same boot deference as the release checker.
            tokio::time::sleep(CHECK_INITIAL_DELAY).await;
            for spec in updater.harnesses.iter() {
                if (spec.resolve)().is_none() {
                    continue;
                }
                match updater.update_harness(spec.id).await {
                    Ok(status) => tracing::info!(
                        harness = spec.id,
                        version = status.installed_version.as_deref().unwrap_or("unknown"),
                        "agent refreshed after comet update"
                    ),
                    Err(err) => {
                        tracing::warn!(harness = spec.id, error = %err, "post-update agent refresh failed");
                    }
                }
            }
        });
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Last few lines of a child's combined output — enough to say why an updater
/// failed without shipping megabytes over RPC.
fn output_tail(output: &std::process::Output) -> String {
    let combined = [&output.stdout[..], &output.stderr[..]].concat();
    let text = String::from_utf8_lossy(&combined);
    let mut tail: Vec<&str> = text.lines().rev().take(8).collect();
    tail.reverse();
    tail.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
    }

    #[test]
    fn release_feed_is_derived_from_exact_edge_origin() {
        assert_eq!(
            releases_base("https://comet.internal.ashler.com").unwrap(),
            "https://comet.internal.ashler.com/api/releases"
        );
        assert!(releases_base("http://comet.internal.ashler.com").is_err());
        assert!(releases_base("https://comet.internal.ashler.com/other").is_err());
        assert_eq!(
            releases_base("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787/api/releases"
        );
    }

    #[test]
    fn release_manifest_matches_the_installed_platform() {
        assert_eq!(
            release_manifest_name(),
            if cfg!(target_os = "macos") {
                "desktop-manifest.json"
            } else {
                "scaffold-manifest.json"
            }
        );
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from(
                Path::new("/home/u/.comet-native/app/0.1.1/comet"),
                Some(Path::new("/home/u")),
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.comet-native/app")
            }
        );
        assert_eq!(
            detect_install_from(
                Path::new("/Applications/Comet.app/Contents/MacOS/comet"),
                Some(Path::new("/Users/u")),
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Comet.app")
            }
        );
        assert_eq!(
            detect_install_from(
                Path::new("/Applications/Crew.app/Contents/MacOS/comet"),
                Some(Path::new("/Users/u")),
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Crew.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from(Path::new("/tmp/foo.app/comet"), None),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from(
                Path::new("/src/target/release/comet"),
                Some(Path::new("/home/u"))
            ),
            InstallKind::Unmanaged
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_update_migrates_legacy_bundle_name_to_crew() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("Comet.app");
        let staged = tmp.path().join("updates").join("Crew.app");
        let legacy_binary = legacy.join("Contents/MacOS/comet");
        let staged_binary = staged.join("Contents/MacOS/comet");
        std::fs::create_dir_all(legacy_binary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(staged_binary.parent().unwrap()).unwrap();
        std::fs::write(&legacy_binary, b"old").unwrap();
        std::fs::write(&staged_binary, b"new").unwrap();

        let installed = apply_mac_app(&staged, &legacy).unwrap();

        assert_eq!(installed, tmp.path().join("Crew.app"));
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(installed.join("Contents/MacOS/comet")).unwrap(),
            b"new"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_update_replaces_a_stale_crew_bundle_during_legacy_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("Comet.app");
        let target = tmp.path().join("Crew.app");
        let staged = tmp.path().join("updates").join("Crew.app");
        for (bundle, contents) in [
            (&legacy, b"legacy".as_slice()),
            (&target, b"stale".as_slice()),
            (&staged, b"current".as_slice()),
        ] {
            let binary = bundle.join("Contents/MacOS/comet");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, contents).unwrap();
        }

        let installed = apply_mac_app(&staged, &legacy).unwrap();

        assert_eq!(installed, target);
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(installed.join("Contents/MacOS/comet")).unwrap(),
            b"current"
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("comet-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("comet-0.2.0-{os}-{arch}.tar.gz")
        );
        assert!(mac_app_artifact("0.2.0").ends_with("-app.tar.gz"));
    }

    #[test]
    fn manifest_requires_file_checksums() {
        let full: Manifest = serde_json::from_str(
            r#"{"version":"0.1.1","files":{"comet-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(full.files["comet-0.1.1-linux-x86_64.tar.gz"].sha256, "abc");
        assert!(
            serde_json::from_str::<Manifest>(r#"{"version":"0.1.1","files":{"comet.tar.gz":{}}}"#)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("comet"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }

    #[test]
    fn version_extraction() {
        assert_eq!(extract_version("omp/17.2.9").as_deref(), Some("17.2.9"));
        assert_eq!(
            extract_version("2.1.224 (Claude Code)").as_deref(),
            Some("2.1.224")
        );
        assert_eq!(
            extract_version("codex-cli 0.146.0").as_deref(),
            Some("0.146.0")
        );
        assert_eq!(extract_version("v17.2.12").as_deref(), Some("17.2.12"));
        assert_eq!(extract_version("no version here"), None);
        // Two segments is a date/counter, not a CLI version.
        assert_eq!(extract_version("built 2026.08"), None);
    }

    #[test]
    fn update_status_wire_compat() {
        // Pre-harness frames (older engines over the relay) must still parse.
        let old: UpdateStatus = serde_json::from_str(r#"{"currentVersion":"0.1.0"}"#).unwrap();
        assert!(old.harnesses.is_empty());
        assert!(old.harness_auto_update);
    }

    #[test]
    fn boot_version_transition() {
        let tmp = tempfile::tempdir().unwrap();
        // First boot stamps but is not a transition.
        assert!(!version_transition(tmp.path()));
        assert!(!version_transition(tmp.path()));
        std::fs::write(tmp.path().join("last-boot-version"), "0.0.1").unwrap();
        assert!(version_transition(tmp.path()));
        assert!(!version_transition(tmp.path()));
    }

    #[test]
    fn harness_auto_update_pref_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_prefs(tmp.path()).harness_auto_update);
        store_prefs(
            tmp.path(),
            &UpdatePrefs {
                harness_auto_update: false,
            },
        );
        assert!(!load_prefs(tmp.path()).harness_auto_update);
    }
}
