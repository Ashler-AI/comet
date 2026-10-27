//! zeron-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`comet update`), and the UI
//! (its own report-only checker, the "Check for Updates…" menu item, the
//! sidebar update strip, and the desktop install paths).
//!
//! Crew releases are private: every check and download uses the engine's current
//! login against `/api/releases`. Digests are mandatory; macOS distribution
//! identity, signing team and notarization are verified before execution or swap.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.comet-native/app/<ver>` + `current` symlink — the curl|sh
//!   installer and the Linux tarball's `install.sh`): download the headless
//!   tarball into a new versioned dir, flip the symlink, then restart the
//!   service (daemon) or relaunch (desktop). Same flow the installer script
//!   performs, natively.
//! - **MacApp** (running out of an app bundle): download the app tarball, swap the
//!   bundle directory, relaunch. Driven by the UI.
//! Windows automatic delivery is unsupported until a Crew signing contract exists.
//! - **Unmanaged** (source builds, hand-copied binaries): report only; install a
//!   signed Crew package through the private release workflow.
//!
//! Desktop installs download as soon as a release is seen and install either
//! on "Restart to update" or when the app next quits, so an app nobody
//! restarts deliberately still lands on the new version at its next launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
mod macos_verification;

fn staging_mac_app() -> bool {
    cfg!(target_os = "macos")
        && option_env!("COMET_PACKAGE_ENVIRONMENT")
            .or(option_env!("COMET_DEFAULT_ENVIRONMENT")) != Some("production")
}

fn mac_app_name() -> &'static str {
    if staging_mac_app() { "Crew Staging.app" } else { "Crew.app" }
}

fn launchd_label() -> &'static str {
    match option_env!("COMET_PACKAGE_ENVIRONMENT")
        .or(option_env!("COMET_DEFAULT_ENVIRONMENT"))
    {
        Some("production") => "ai.ashler.comet",
        Some("canary") => "ai.ashler.crew.upstream-canary",
        _ => "ai.ashler.comet.staging",
    }
}

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Crew publishes authenticated updates for macOS and Linux.
pub const AUTOMATIC_UPDATES_SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

fn require_update_delivery() -> anyhow::Result<()> {
    anyhow::ensure!(AUTOMATIC_UPDATES_SUPPORTED, "Crew updates are not supported on this platform");
    Ok(())
}

/// Successful checks repeat this often. The feed is a sub-kilobyte,
/// edge-cached document, so hourly polling is free and bounds how long a
/// long-running app can sit on a stale release.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Failed checks (offline boot, captive portal, transient edge error) back off
/// through these delays, then stay at the last one.
const CHECK_RETRY: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
];
/// Check deadlines are wall-clock times: a monotonic sleep stops counting
/// while the machine sleeps, so a laptop closed overnight used to push its
/// next check out by the whole night. The loop re-reads the clock this often.
const SCHEDULE_TICK: Duration = Duration::from_secs(60);
/// The engine's first check waits out engine boot (room joins, doc re-sync).
const ENGINE_INITIAL_DELAY: Duration = Duration::from_secs(20);
/// The desktop checker only needs the window on screen first.
const DESKTOP_INITIAL_DELAY: Duration = Duration::from_secs(2);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: Duration = Duration::from_secs(5 * 60);
/// A staged binary must answer `--version` within this window.
const STAGED_VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Release metadata is tiny. Bound both its buffered size and total transfer
/// time so a compromised or misconfigured feed cannot hold a checker
/// forever or make every local install buffer an unbounded response.
const RELEASE_METADATA_MAX_BYTES: usize = 1024 * 1024;
const RELEASE_METADATA_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// Private platform manifest written by the Crew release workflow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Artifact file name → required SHA-256 digest.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub sha256: String,
}

/// Artifact-name platform pair matching the packaging scripts. Unsupported
/// updater targets retain their real OS name rather than impersonating Linux.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

fn managed_updates_supported(os: &str) -> bool {
    matches!(os, "linux" | "macos")
}

fn require_managed_update_platform() -> anyhow::Result<()> {
    if !managed_updates_supported(std::env::consts::OS) {
        // Preserve the platform-specific diagnostic on unsupported targets.
        bail!(
            "managed updates are not supported on {}",
            std::env::consts::OS
        );
    }
    require_update_delivery()?;
    Ok(())
}

fn require_mac_app_update_platform() -> anyhow::Result<()> {
    if std::env::consts::OS != "macos" {
        bail!(
            "macOS app updates are not supported on {}",
            std::env::consts::OS
        );
    }
    require_update_delivery()?;
    #[cfg(target_os = "macos")]
    macos_verification::expected_team()?;
    Ok(())
}

/// `comet-<ver>-<os>-<arch>.tar.gz` — the managed CLI tarball.
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("comet-{version}-{os}-{arch}.tar.gz")
}

/// App payload selected by the compile-time package environment.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    let prefix = if staging_mac_app() { "comet-staging" } else { "comet" };
    format!("{prefix}-{version}-macos-{arch}-app.tar.gz")
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    let nums: Vec<u64> = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (!nums.is_empty()).then_some(nums)
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer — garbage release metadata must not
/// trigger an update loop.
pub fn version_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn release_manifest_name() -> &'static str {
    if staging_mac_app() {
        "desktop-staging-manifest.json"
    } else if cfg!(target_os = "macos") {
        "desktop-manifest.json"
    } else {
        "scaffold-manifest.json"
    }
}

fn authorized_get(client: &reqwest::Client, url: &str, access_token: Option<&str>) -> anyhow::Result<reqwest::RequestBuilder> {
    let token = access_token.filter(|token| !token.trim().is_empty())
        .context("Sign in to Crew to check or download updates")?;
    Ok(client.get(url).bearer_auth(token))
}

/// Fetch only the authenticated platform feed; never fall back to a public feed.
pub async fn fetch_latest(edge_url: &str, access_token: Option<&str>) -> anyhow::Result<Manifest> {
    let base = release_base(edge_url)?;
    let client = http_client()?;
    let response = fetch_release_metadata(&client, &format!("{base}/{}", release_manifest_name()), access_token).await?;
    anyhow::ensure!(response.status.is_success(), "fetching Crew release manifest: HTTP {}", response.status);
    let manifest: Manifest = serde_json::from_slice(&response.body).context("parsing Crew release manifest")?;
    anyhow::ensure!(parse_version(&manifest.version).is_some(), "invalid release version");
    anyhow::ensure!(!manifest.files.is_empty(), "release manifest has no checksummed files");
    for metadata in manifest.files.values() {
        anyhow::ensure!(metadata.sha256.len() == 64 && metadata.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()), "invalid release SHA-256");
    }
    Ok(manifest)
}

#[derive(Debug)]
struct ReleaseMetadataResponse {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

async fn fetch_release_metadata(
    client: &reqwest::Client,
    url: &str,
    access_token: Option<&str>,
) -> anyhow::Result<ReleaseMetadataResponse> {
    fetch_release_metadata_with_limits(
        client,
        url,
        access_token,
        RELEASE_METADATA_TIMEOUT,
        RELEASE_METADATA_MAX_BYTES,
    )
    .await
}

async fn fetch_release_metadata_with_limits(
    client: &reqwest::Client,
    url: &str,
    access_token: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<ReleaseMetadataResponse> {
    let request = async {
        let response = authorized_get(client, url, access_token)?
            // Intermediaries must revalidate: the edge already bounds its own
            // cache to a minute, and a proxy holding an old manifest for
            // longer is exactly the "prompt appears late" failure.
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        let status = response.status();
        if !status.is_success() {
            return Ok(ReleaseMetadataResponse {
                status,
                body: Vec::new(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            bail!("release metadata exceeds {max_bytes} bytes");
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading release metadata")?;
            anyhow::ensure!(
                body.len().saturating_add(chunk.len()) <= max_bytes,
                "release metadata exceeds {max_bytes} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(ReleaseMetadataResponse { status, body })
    };
    tokio::time::timeout(timeout, request)
        .await
        .with_context(|| format!("fetching {url} exceeded the metadata deadline"))?
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    http_client_with_timeouts(Duration::from_secs(15), Duration::from_secs(30))
}

fn http_client_with_timeouts(connect: Duration, read: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        // Inactivity timeout, not a total download cap: slow progressing
        // updates remain viable on constrained links.
        .read_timeout(read)
        .user_agent(concat!("comet/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many update redirects");
            }
            if attempt.previous().iter().any(|url| url.origin() != attempt.url().origin()) {
                return attempt.error("update redirect would leave the authenticated Crew origin");
            }
            attempt.follow()
        }))
        .build()
        .context("building http client")
}



fn release_base(edge_url: &str) -> anyhow::Result<String> {
    require_update_delivery()?;
    let mut url = reqwest::Url::parse(edge_url).context("invalid Crew edge URL")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    anyhow::ensure!(url.host_str().is_some()
        && (url.scheme() == "https" || (url.scheme() == "http" && local))
        && url.username().is_empty() && url.password().is_none()
        && url.query().is_none() && url.fragment().is_none()
        && matches!(url.path(), "" | "/"),
        "Crew edge URL must be an exact HTTPS origin (HTTP is loopback-only)");
    url.set_path("/api/releases");
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.comet-native/app/<ver>/comet` behind the `current` symlink
    /// (curl|sh installer, the Linux tarball's `install.sh`, or `comet update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Windows installer or portable package with an explicit update-feed
    /// configuration.
    #[cfg(windows)]
    WindowsPortable { directory: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

/// Why an installation that normally updates itself cannot right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateBlocker {
    /// macOS App Translocation ran the app from a randomized read-only path
    /// (launched from Downloads or a disk image without being moved first).
    Translocated,
    /// Running straight from the mounted disk image.
    DiskImage,
    /// The directory holding the installation is not writable by this user.
    NotWritable(PathBuf),
}

impl std::fmt::Display for UpdateBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Translocated | Self::DiskImage => f.write_str(
                "Crew is running from a temporary, read-only location. Move Crew to your \
                 Applications folder and reopen it to turn on updates.",
            ),
            Self::NotWritable(dir) => write!(
                f,
                "Crew doesn't have permission to replace itself in {}.",
                dir.display()
            ),
        }
    }
}

impl InstallKind {
    /// Whether the desktop app downloads and installs updates itself.
    pub fn supports_desktop_update(&self) -> bool {
        if !AUTOMATIC_UPDATES_SUPPORTED {
            return false;
        }
        match self {
            Self::MacApp { .. } => true,
            // The desktop path relaunches a GUI binary; on Linux that binary
            // is the managed install itself.
            Self::Managed { .. } => cfg!(target_os = "linux"),
            #[cfg(windows)]
            Self::WindowsPortable { .. } => true,
            Self::Unmanaged => false,
        }
    }

    /// Why this installation cannot replace itself, when it can't. Checked
    /// before downloading so the UI can explain the fix instead of failing
    /// after a wasted download with a raw rename error.
    pub fn desktop_update_blocker(&self) -> Option<UpdateBlocker> {
        match self {
            Self::MacApp { bundle } => mac_bundle_blocker(bundle, dir_writable),
            Self::Managed { app_root } => {
                (!dir_writable(app_root)).then(|| UpdateBlocker::NotWritable(app_root.clone()))
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                (!dir_writable(directory)).then(|| UpdateBlocker::NotWritable(directory.clone()))
            }
            Self::Unmanaged => None,
        }
    }

    pub async fn stage_desktop(
        &self,
        edge_url: &str,
        access_token: Option<&str>,
        manifest: &Manifest,
        data_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        require_update_delivery()?;
        match self {
            Self::MacApp { .. } => stage_mac_app(edge_url, access_token, manifest, data_dir).await,
            Self::Managed { app_root } if self.supports_desktop_update() => {
                stage_headless(edge_url, access_token, manifest, app_root).await
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => {
                windows::stage(edge_url, access_token, manifest, directory).await
            }
            _ => bail!("this installation does not support desktop updates"),
        }
    }

    /// Install a staged update. With `relaunch`, the new version starts once
    /// this process exits ("Restart to update"); without it the update takes
    /// effect at the next launch ("install on quit"). Either way the caller
    /// must quit after this succeeds.
    pub fn apply_desktop(&self, staged: &Path, relaunch: bool) -> anyhow::Result<()> {
        require_update_delivery()?;
        match self {
            Self::MacApp { bundle } => {
                let installed = apply_mac_app(staged, bundle)?;
                // The swap copied the staged bundle; the cache is spent.
                if let Some(version_dir) = staged.parent() {
                    let _ = std::fs::remove_dir_all(version_dir);
                }
                if relaunch {
                    relaunch_after_exit(Path::new("/usr/bin/open"), &installed);
                }
                Ok(())
            }
            Self::Managed { app_root } if self.supports_desktop_update() => {
                let version = staged
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("staged install has no version directory")?;
                apply_headless(app_root, version)?;
                if relaunch {
                    let binary = app_root.join("current").join("comet");
                    relaunch_after_exit(&binary, Path::new(""));
                }
                Ok(())
            }
            #[cfg(windows)]
            Self::WindowsPortable { directory } => windows::apply(staged, directory, relaunch),
            _ => bail!("this installation does not support desktop updates"),
        }
    }
}

fn mac_bundle_blocker(bundle: &Path, writable: impl Fn(&Path) -> bool) -> Option<UpdateBlocker> {
    if bundle
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        return Some(UpdateBlocker::Translocated);
    }
    let parent = bundle.parent()?;
    if writable(parent) {
        return None;
    }
    if bundle.starts_with("/Volumes") {
        return Some(UpdateBlocker::DiskImage);
    }
    Some(UpdateBlocker::NotWritable(parent.to_path_buf()))
}

/// Probe by creating (and removing) a file: permission bits alone miss
/// read-only mounts, ACLs, and sandboxed locations.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".zeron-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            true
        }
        // A leftover probe from a crashed run still proves writability.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    detect_install_from_for_os(exe, home, std::env::consts::OS)
}

fn detect_install_from_for_os(exe: &Path, home: Option<&Path>, os: &str) -> InstallKind {
    #[cfg(windows)]
    if os == "windows" && windows::is_managed(exe) {
        return InstallKind::WindowsPortable {
            directory: exe.parent().unwrap().to_owned(),
        };
    }
    // Never interpret a coincidental Windows `%HOME%\.zeron\app` layout as
    // the Unix symlink-managed installation.
    if !managed_updates_supported(os) {
        return InstallKind::Unmanaged;
    }
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        for root in [".comet-native-staging", ".comet-native"] {
            let app_root = home.join(root).join("app");
            if exe.starts_with(&app_root) {
                return InstallKind::Managed { app_root };
            }
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

/// The version currently installed on disk for `kind`, read without executing
/// anything. A long-running process compares it with its own version to notice
/// that something else (the desktop app, `comet update`, a re-run installer)
/// has already replaced its binary.
pub fn installed_version(kind: &InstallKind) -> Option<String> {
    let version = match kind {
        InstallKind::Managed { app_root } => std::fs::read_link(app_root.join("current"))
            .ok()?
            .file_name()?
            .to_str()?
            .split('-').next()?
            .to_owned(),
        InstallKind::MacApp { bundle } => bundle_short_version(
            &std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?,
        )?,
        _ => return None,
    };
    parse_version(&version).map(|_| version)
}

/// `CFBundleShortVersionString` from an XML property list (the packaging
/// template; codesign leaves the format alone).
fn bundle_short_version(plist: &str) -> Option<String> {
    let after_key = plist
        .split("<key>CFBundleShortVersionString</key>")
        .nth(1)?;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let end = value.find("</string>")?;
    Some(value[..end].trim().to_owned())
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream a private release file with mandatory manifest SHA-256 verification.
/// A `.partial` sidecar prevents incomplete downloads from appearing valid.
pub async fn download_release_file(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = format!("{}/{file}", release_base(edge_url)?);
    let expected = manifest.files.get(file).map(|metadata| metadata.sha256.as_str())
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| format!("release manifest has no valid SHA-256 for {file}"))?;
    let partial = dest.with_extension("partial");
    let client = http_client()?;
    let resp = authorized_get(&client, &url, access_token)?
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
    out.flush().await.context("flushing download")?;
    drop(out);
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
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

/// Run a freshly unpacked binary's `--version` before it can replace anything:
/// proves the payload is complete, executable on this machine, and the release
/// the manifest promised.
async fn verify_staged_binary(binary: &Path, version: &str) -> anyhow::Result<()> {
    let output = tokio::time::timeout(
        STAGED_VERSION_TIMEOUT,
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("staged binary version check timed out")?
    .with_context(|| format!("running {} --version", binary.display()))?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::ensure!(
        output.status.success() && reported == format!("comet {version}"),
        "staged binary reported {reported:?} (exit {}), expected \"comet {version}\"",
        output.status
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path and Linux desktop
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>`. An existing
/// version is reused only when its complete tree matches the freshly authenticated
/// payload; a cached executable's presence or `--version` is not proof of trust.
/// Returns the versioned dir without replacing a running release in place.
pub async fn stage_headless(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_managed_update_platform()?;
    let version = &manifest.version;
    anyhow::ensure!(
        parse_version(version).is_some(),
        "invalid release version {version:?}"
    );
    let dest = app_root.join(version);
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
        if !is_executable_file(&unpacked.join("comet")) {
            bail!("tarball {file} did not contain a comet binary");
        }
        if !is_executable_file(&unpacked.join("zeron-engine")) {
            bail!("tarball {file} did not contain the Crew background engine");
        }
        verify_staged_binary(&unpacked.join("comet"), version).await?;
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // A prior stage or another stager may own this version. Authenticate
            // every payload file (including crew-web), not just its executable.
            Err(_) if dest.symlink_metadata().is_ok() => {
                anyhow::ensure!(
                    same_payload(&unpacked, &dest)?,
                    "cached install {} differs from the authenticated release",
                    dest.display()
                );
            }
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

/// Compare without following symlinks or buffering whole release files.
fn same_payload(expected: &Path, cached: &Path) -> std::io::Result<bool> {
    use std::io::Read as _;
    let expected_meta = std::fs::symlink_metadata(expected)?;
    let cached_meta = std::fs::symlink_metadata(cached)?;
    if expected_meta.file_type() != cached_meta.file_type() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if expected_meta.permissions().mode() & 0o111 != cached_meta.permissions().mode() & 0o111 {
            return Ok(false);
        }
    }
    if expected_meta.is_symlink() {
        return Ok(std::fs::read_link(expected)? == std::fs::read_link(cached)?);
    }
    if expected_meta.is_dir() {
        let mut count = 0;
        for entry in std::fs::read_dir(expected)? {
            let entry = entry?;
            if !same_payload(&entry.path(), &cached.join(entry.file_name()))? {
                return Ok(false);
            }
            count += 1;
        }
        return Ok(std::fs::read_dir(cached)?.count() == count);
    }
    if !expected_meta.is_file() || expected_meta.len() != cached_meta.len() {
        return Ok(false);
    }
    let mut expected_file = std::fs::File::open(expected)?;
    let mut cached_file = std::fs::File::open(cached)?;
    let mut expected_bytes = [0; 8192];
    let mut cached_bytes = [0; 8192];
    loop {
        let count = expected_file.read(&mut expected_bytes)?;
        if count == 0 {
            return Ok(cached_file.read(&mut cached_bytes)? == 0);
        }
        cached_file.read_exact(&mut cached_bytes[..count])?;
        if expected_bytes[..count] != cached_bytes[..count] {
            return Ok(false);
        }
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    require_update_delivery()?;
    anyhow::ensure!(parse_version(version).is_some(), "invalid release version");
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !is_executable_file(&target.join("comet"))
            || !is_executable_file(&target.join("zeron-engine"))
        {
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
        require_managed_update_platform()?;
        unreachable!("supported managed-update platforms are Unix")
    }
}

type ManagedRestart = Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>;
static MANAGED_RESTART: std::sync::OnceLock<ManagedRestart> = std::sync::OnceLock::new();

/// Register restart behavior for the desktop-owned background engine, which
/// is not supervised by Crew's launchd/systemd daemon units.
pub fn set_managed_restart(restart: impl Fn() -> anyhow::Result<()> + Send + Sync + 'static) {
    let _ = MANAGED_RESTART.set(Box::new(restart));
}

type ManagedRestartReady = Box<dyn Fn() -> bool + Send + Sync>;
static MANAGED_RESTART_READY: std::sync::OnceLock<ManagedRestartReady> = std::sync::OnceLock::new();

pub fn set_managed_restart_ready(ready: impl Fn() -> bool + Send + Sync + 'static) {
    let _ = MANAGED_RESTART_READY.set(Box::new(ready));
}

fn managed_restart_ready() -> bool {
    MANAGED_RESTART_READY.get().is_none_or(|ready| ready())
}

/// Whether this process is the engine service [`restart_service`] manages:
/// either the desktop-owned engine, or systemd/launchd using Crew's identity.
/// systemd places it in its Crew unit's cgroup; launchd names the job in
/// `XPC_SERVICE_NAME`.
pub fn running_as_installed_service() -> bool {
    if MANAGED_RESTART.get().is_some() {
        return true;
    }
    if cfg!(target_os = "macos") {
        std::env::var("XPC_SERVICE_NAME").is_ok_and(|label| label == launchd_label())
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/self/cgroup")
            .is_ok_and(|cgroups| service_from_cgroup(&cgroups).is_some())
    } else {
        false
    }
}

fn service_from_cgroup(cgroups: &str) -> Option<&str> {
    cgroups
        .lines()
        .filter_map(|line| line.rsplit(':').next())
        .flat_map(|path| path.split('/'))
        .find(|part| {
            matches!(
                *part,
                "crew-upstream-canary.service"
                    | "crew-staging.service"
                    | "crew-production.service"
                    | "comet-native-staging.service"
                    | "comet-native.service"
            )
        })
}

/// Restart the installed engine service (the same units `comet daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary. Only queues the restart: the caller may be the
/// service itself, which must stay responsive to the stop signal that follows.
pub fn restart_service() -> anyhow::Result<()> {
    if let Some(restart) = MANAGED_RESTART.get() {
        return restart();
    }
    require_managed_update_platform()?;
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/{}", launchd_label())],
        )
    } else {
        let cgroups = std::fs::read_to_string("/proc/self/cgroup")?;
        let service = service_from_cgroup(&cgroups)
            .context("this process is not running in an installed Crew service")?;
        run(
            "systemctl",
            &["--user", "--no-block", "restart", service],
        )
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/<app-name>`
/// (idempotent). The bundle is unpacked beside its final name and renamed in
/// only after its binary answers with the expected version, so an interrupted
/// unpack can never be mistaken for a staged update. Returns the staged bundle.
pub async fn stage_mac_app(
    edge_url: &str,
    access_token: Option<&str>,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // Reject unsupported targets before creating a stage or making a request.
    require_mac_app_update_platform()?;
    let version = &manifest.version;
    anyhow::ensure!(
        parse_version(version).is_some(),
        "invalid release version {version:?}"
    );
    let updates = data_dir.join("updates");
    let dir = updates.join(version);
    let staged = dir.join(mac_app_name());
    let staged_binary = staged.join("Contents/MacOS/comet");
    if staged_binary.exists() {
        #[cfg(target_os = "macos")]
        macos_verification::Policy::pinned()?.verify_distribution(&staged)?;
        verify_staged_binary(&staged_binary, version).await?;
        return Ok(staged);
    }
    prune_stale_stages(&updates, version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = mac_app_artifact(version);
    let tarball = dir.join(&file);
    download_release_file(edge_url, access_token, manifest, &file, &tarball).await?;
    let unpack = dir.join(".unpack");
    std::fs::create_dir_all(&unpack)?;
    let unpacked = run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &unpack.to_string_lossy(),
        ],
    )
    .map(|()| unpack.join(mac_app_name()));
    std::fs::remove_file(&tarball).ok();
    let unpacked = unpacked?;
    let unpacked_binary = unpacked.join("Contents/MacOS/comet");
    if !unpacked_binary.exists() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("app tarball {file} did not contain {}", mac_app_name());
    }
    #[cfg(target_os = "macos")]
    macos_verification::Policy::pinned()?.verify_distribution(&unpacked)?;
    if let Err(err) = verify_staged_binary(&unpacked_binary, version).await {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    std::fs::rename(&unpacked, &staged)
        .with_context(|| format!("moving {} into place", staged.display()))?;
    let _ = std::fs::remove_dir_all(&unpack);
    Ok(staged)
}

/// Staged bundles of other versions are dead weight (each is a whole app):
/// drop them whenever a new version stages.
fn prune_stale_stages(updates: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(updates) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Install the staged bundle next to the current app, preserving metadata and
/// migrating renamed installs to `Crew.app` or `Crew Staging.app` as appropriate.
/// Incoming, copied, and installed macOS bundles are authenticated before any
/// installed bundle is moved. Legacy ad-hoc migration requires the updater itself
/// to run from an already trusted distribution; it never re-signs an old app.
/// Any existing target bundle is restored if the replacement fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<PathBuf> {
    require_mac_app_update_platform()?;
    #[cfg(target_os = "macos")]
    let policy = macos_verification::Policy::pinned()?;
    #[cfg(target_os = "macos")]
    policy.verify_distribution(staged)?;
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    #[cfg(target_os = "macos")]
    let target_name = std::ffi::OsStr::new(mac_app_name());
    #[cfg(not(target_os = "macos"))]
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
    // Keep the .app suffix: stapler validates bundles by their file type.
    let fresh = parent.join(format!(".{target_name}.new-{pid}.app"));
    let old = parent.join(format!(".{current_name}.old-{pid}"));
    let displaced_target = (target.as_path() != bundle && target.exists())
        .then(|| parent.join(format!(".{target_name}.old-{pid}")));
    // Do not discard an interrupted update's backups or follow a stale symlink.
    for path in [&fresh, &old].into_iter().chain(displaced_target.iter()) {
        if path.symlink_metadata().is_ok() {
            bail!("update work path already exists: {}", path.display());
        }
    }
    if let Err(error) = run(
        if cfg!(target_os = "macos") {
            "/usr/bin/ditto"
        } else {
            "ditto"
        },
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    ) {
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(error).context("copying the verified Crew update");
    }
    #[cfg(target_os = "macos")]
    {
        // Verify the actual copy, then every installed bundle we would move.
        // All trust checks precede the first rename or removal of installed data.
        let verified = (|| -> anyhow::Result<()> {
            policy.verify_distribution(&fresh)?;
            policy.verify_installed(bundle)?;
            if displaced_target.is_some() {
                policy.verify_installed(&target)?;
            }
            Ok(())
        })();
        if let Err(error) = verified {
            let _ = std::fs::remove_dir_all(&fresh);
            return Err(error).context("Crew update refused before replacing the installed app");
        }
    }
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

/// Detached relauncher: waits for THIS process to exit, then runs `program`
/// (with `argument` unless empty). Launching before exit would race the
/// single-instance engine lock and the IPC port. Paths travel as positional
/// parameters, never spliced into the script. The caller quits after this.
pub fn relaunch_after_exit(program: &Path, argument: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id().to_string();
        let script = r#"while /bin/kill -0 "$1" 2>/dev/null; do sleep 0.2; done
if [ -n "$3" ]; then exec "$2" "$3"; else exec "$2"; fi"#;
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .arg("zeron-relaunch")
            .arg(&pid)
            .arg(program)
            .arg(argument)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = (program, argument);
}

// ---------------------------------------------------------------------------
// Background checker
// ---------------------------------------------------------------------------

/// What the checker reports (the engine's `UpdateStatus` stream, the desktop
/// strip). Version facts only — download/apply progress is owned by whoever
/// drives the update (UI or CLI).
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
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
        }
    }
}

/// `COMET_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("COMET_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// `COMET_AUTO_UPDATE=0|false|no` — the desktop app then only reports: no
/// background download and no install on quit. Unset means on.
pub fn desktop_auto_update_enabled() -> bool {
    std::env::var("COMET_AUTO_UPDATE")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Fresh token from the existing engine login, resolved for each request.
pub type AccessTokenProvider = Arc<dyn Fn() -> futures::future::BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;

/// Who runs the checker — decides what it may do beyond reporting.
enum Role {
    /// The engine: managed installs with `COMET_AUTO_UPDATE` apply themselves,
    /// and a service daemon restarts into a binary someone else installed.
    Engine { quiescent: Option<QuiescentCheck> },
    /// The desktop app: report only — downloading and installing are UI
    /// decisions (see `crates/ui/src/app_update.rs`).
    Desktop,
}

/// Background release checker: polls `{edge}/api/releases` hourly on a wall-clock
/// schedule (so sleep/wake cannot stretch it), backs off on failure, and
/// publishes [`UpdateStatus`] over a watch channel. In the engine, managed
/// installs with `COMET_AUTO_UPDATE` set stage + apply + service restart on
/// their own — but only in a quiet window: while `quiescent` reports activity,
/// the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    edge_url: String,
    access_token: AccessTokenProvider,
    role: Arc<Role>,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    /// Wakes the loop; `forced` says whether the wake demands a check or only
    /// a fresh look at the wall clock.
    wake_tx: Arc<watch::Sender<u64>>,
    forced: Arc<AtomicBool>,
    /// Set by `comet headless`: this process is the installed service and may
    /// restart itself into a newer installed binary.
    service: Arc<AtomicBool>,
    /// Flips to true exactly once; the check loop selects against it so
    /// cancellation lands at any await point (no tokio-util in this crate).
    shutdown_tx: Arc<watch::Sender<bool>>,
    check_task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Updater {
    /// Spawn the engine's check loop (must run on a tokio runtime).
    pub fn spawn(edge_url: String, access_token: AccessTokenProvider, quiescent: Option<QuiescentCheck>) -> Self {
        Self::spawn_role(edge_url, access_token, Role::Engine { quiescent })
    }

    /// Spawn the desktop app's report-only check loop (must run on a tokio
    /// runtime).
    pub fn spawn_desktop(edge_url: String, access_token: AccessTokenProvider) -> Self {
        Self::spawn_role(edge_url, access_token, Role::Desktop)
    }

    fn spawn_role(edge_url: String, access_token: AccessTokenProvider, role: Role) -> Self {
        let initial_delay = match role {
            Role::Engine { .. } => ENGINE_INITIAL_DELAY,
            Role::Desktop => DESKTOP_INITIAL_DELAY,
        };
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let (wake_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let updater = Self {
            edge_url,
            access_token,
            role: Arc::new(role),
            status_tx: Arc::new(status_tx),
            wake_tx: Arc::new(wake_tx),
            forced: Arc::new(AtomicBool::new(false)),
            service: Arc::new(AtomicBool::new(false)),
            shutdown_tx: Arc::new(shutdown_tx),
            check_task: Arc::new(std::sync::Mutex::new(None)),
        };
        // Subscribe before spawning so an immediate `check_now` cannot land
        // before the background task first polls and be lost behind the
        // initial delay.
        let wakes = updater.wake_tx.subscribe();
        let for_loop = updater.clone();
        let task = tokio::spawn(async move { for_loop.check_loop(wakes, initial_delay).await });
        *updater.check_task.lock().unwrap() = Some(task);
        updater
    }

    /// Stop the check loop and wait for it to exit — a replaced runtime must
    /// not keep polling `{edge}/releases` (or auto-applying) in the background.
    /// Idempotent, and callable from any clone.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let task = self
            .check_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    pub async fn current_access_token(&self) -> anyhow::Result<String> {
        (self.access_token)().await
    }

    pub async fn fetch_latest(&self) -> anyhow::Result<Manifest> {
        let token = self.current_access_token().await?;
        fetch_latest(&self.edge_url, Some(&token)).await
    }

    /// Check immediately, for example when authentication recovers after the
    /// process started offline.
    pub fn check_now(&self) {
        self.forced.store(true, Ordering::SeqCst);
        self.wake_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Re-read the wall clock now (window activation, system wake): checks
    /// only if one is due, so frequent pokes cost nothing.
    pub fn poke(&self) {
        self.wake_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Called by `comet headless`: when this process is the installed engine
    /// service and something else installs a newer binary, restart into it at
    /// the next quiet moment. A hand-started `comet headless` is left alone —
    /// restarting the service unit would start a second engine beside it.
    pub fn restart_when_superseded(&self) {
        if !running_as_installed_service() {
            return;
        }
        self.service.store(true, Ordering::SeqCst);
        self.poke();
    }

    /// One user-requested check, awaited: publishes the result like a
    /// scheduled check and returns it.
    pub async fn check(&self) -> anyhow::Result<UpdateStatus> {
        match self.fetch_latest().await {
            Ok(manifest) => {
                self.publish(manifest);
                Ok(self.status_tx.borrow().clone())
            }
            Err(err) => {
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                Err(err)
            }
        }
    }

    fn quiescent_now(&self) -> bool {
        match &*self.role {
            Role::Engine { quiescent } => quiescent.as_ref().is_none_or(|check| check()),
            Role::Desktop => true,
        }
    }

    async fn check_loop(&self, mut wakes: watch::Receiver<u64>, initial_delay: Duration) {
        let mut shutdown = self.shutdown_tx.subscribe();
        // Shutdown must cut the loop at ANY await point — including mid
        // `check_once()` / `auto_apply_when_idle()` HTTP — so the whole body
        // races the flag rather than checking it between iterations.
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => {}
            _ = async {
                tokio::select! {
                    _ = tokio::time::sleep(initial_delay) => {}
                    _ = wakes.changed() => {}
                }
                let mut schedule = Schedule::default();
                let mut superseded = SupersededRestart::default();
                loop {
                    let forced = self.forced.swap(false, Ordering::SeqCst);
                    if forced || schedule.due(SystemTime::now()) {
                        let ok = self.check_once().await;
                        schedule.record(SystemTime::now(), ok);
                        if ok
                            && self.status_tx.borrow().update_available
                            && matches!(*self.role, Role::Engine { .. })
                            && auto_update_enabled()
                            && let InstallKind::Managed { .. } = detect_install()
                        {
                            self.auto_apply_when_idle().await;
                        }
                    }
                    if self.service.load(Ordering::SeqCst) {
                        superseded.poll(|| self.quiescent_now());
                    }
                    let wait = schedule.wait(SystemTime::now());
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = wakes.changed() => {}
                    }
                }
            } => {}
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and authenticates any existing staged tree
    /// against a fresh download before the final quiescence check.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            match self.fetch_latest().await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    let stage = async {
                        let token = self.current_access_token().await?;
                        stage_headless(&self.edge_url, Some(&token), &manifest, &app_root).await
                    }.await;
                    if let Err(err) = stage {
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
        loop {
            while !self.quiescent_now() {
                if !deferred {
                    deferred = true;
                    tracing::info!("auto-update deferred: sessions or terminals active");
                }
                tokio::time::sleep(IDLE_RECHECK).await;
            }
            // `apply_inner` fetches and stages again so a long defer lands on
            // the newest release. Re-check quiescence immediately before the
            // symlink swap: work can begin while that network/disk I/O runs.
            match self.apply_inner(true).await {
                Ok(Some(version)) => {
                    tracing::info!(%version, "auto-update applied; service restarting");
                    return;
                }
                Ok(None) => {
                    if !deferred {
                        deferred = true;
                        tracing::info!(
                            "auto-update deferred: activity began while staging the update"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update failed");
                    return;
                }
            }
        }
    }

    fn publish(&self, manifest: Manifest) {
        let status = UpdateStatus {
            current_version: current_version().to_string(),
            update_available: version_newer(&manifest.version, current_version()),
            latest_version: Some(manifest.version),
            checked_at: Some(now_ms()),
            error: None,
        };
        if status.update_available {
            tracing::info!(
                latest = status.latest_version.as_deref().unwrap_or(""),
                current = %status.current_version,
                "update available"
            );
        }
        self.status_tx.send_replace(status);
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match self.fetch_latest().await {
            Ok(manifest) => {
                self.publish(manifest);
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
        self.apply_inner(false)
            .await?
            .ok_or_else(|| anyhow::anyhow!("update became busy before apply"))
    }

    /// `require_quiescent` is used by automatic updates. It deliberately checks
    /// after all network and staging I/O and directly before the destructive
    /// swap/restart boundary; `None` tells the caller to wait and try again.
    async fn apply_inner(&self, require_quiescent: bool) -> anyhow::Result<Option<String>> {
        require_update_delivery()?;
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = self.fetch_latest().await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        let token = self.current_access_token().await?;
        stage_headless(&self.edge_url, Some(&token), &manifest, &app_root).await?;
        if require_quiescent && !self.quiescent_now() {
            return Ok(None);
        }
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(Some(manifest.version))
    }
}

/// Wall-clock check deadlines with failure backoff.
#[derive(Debug, Default)]
struct Schedule {
    /// `None` until the first check: due immediately.
    next_due: Option<SystemTime>,
    failures: usize,
}

impl Schedule {
    fn due(&self, now: SystemTime) -> bool {
        match self.next_due {
            None => true,
            Some(due) => match due.duration_since(now) {
                Err(_) => true,
                // A deadline further out than any interval means the clock
                // jumped backwards; don't wait for it to catch up.
                Ok(remaining) => remaining.is_zero() || remaining > CHECK_INTERVAL,
            },
        }
    }

    fn record(&mut self, now: SystemTime, ok: bool) {
        let delay = if ok {
            self.failures = 0;
            CHECK_INTERVAL
        } else {
            let delay = CHECK_RETRY[self.failures.min(CHECK_RETRY.len() - 1)];
            self.failures = self.failures.saturating_add(1);
            delay
        };
        self.next_due = Some(now + delay);
    }

    /// Monotonic sleep until the next look at the clock.
    fn wait(&self, now: SystemTime) -> Duration {
        self.next_due
            .and_then(|due| due.duration_since(now).ok())
            .unwrap_or(Duration::ZERO)
            .clamp(Duration::from_secs(1), SCHEDULE_TICK)
    }
}

/// Restarts a service daemon into a newer binary that is already installed
/// (the desktop app swapped the bundle, `comet update` flipped the symlink).
/// Waits for quiescence, and tries at most once per installed version so a
/// broken service manager cannot turn into a restart loop.
#[derive(Debug, Default)]
struct SupersededRestart {
    attempted: Option<String>,
    deferred_logged: bool,
}

impl SupersededRestart {
    fn poll(&mut self, quiescent: impl Fn() -> bool) {
        let Some(installed) = installed_version(&detect_install()) else {
            return;
        };
        self.poll_installed(
            &installed,
            current_version(),
            || quiescent() && managed_restart_ready(),
            restart_service,
        );
    }

    fn poll_installed(
        &mut self,
        installed: &str,
        running: &str,
        quiescent: impl Fn() -> bool,
        restart: impl FnOnce() -> anyhow::Result<()>,
    ) {
        if !version_newer(installed, running) || self.attempted.as_deref() == Some(installed) {
            return;
        }
        if !quiescent() {
            if !self.deferred_logged {
                self.deferred_logged = true;
                tracing::info!(
                    installed,
                    running,
                    "newer version installed; restart deferred: sessions or terminals active"
                );
            }
            return;
        }
        self.attempted = Some(installed.to_owned());
        tracing::info!(
            installed,
            running,
            "newer version installed; restarting service"
        );
        if let Err(err) = restart() {
            tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;


    #[tokio::test]
    async fn stalled_update_headers_and_body_time_out_but_progressing_body_survives() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for stall_body in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                if stall_body {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
            let client =
                http_client_with_timeouts(Duration::from_millis(200), Duration::from_millis(100))
                    .unwrap();
            let request = async {
                client
                    .get(format!("http://{address}"))
                    .send()
                    .await?
                    .bytes()
                    .await
            };
            let error = tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .expect("bounded read")
                .unwrap_err();
            assert!(error.is_timeout());
            server.abort();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..10 {
                socket.write_all(b"x").await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let client =
            http_client_with_timeouts(Duration::from_secs(1), Duration::from_millis(200)).unwrap();
        assert_eq!(
            client
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .len(),
            10
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn release_metadata_has_size_and_total_time_limits() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Reject an advertised oversized body before buffering it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n01234567890")
                .await
                .unwrap();
        });
        let client =
            http_client_with_timeouts(Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Some("fixture-token"),
            Duration::from_secs(1),
            10,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds 10 bytes"));
        server.await.unwrap();

        // Enforce the same cap when Content-Length is absent.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n01234567890")
                .await
                .unwrap();
        });
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Some("fixture-token"),
            Duration::from_secs(1),
            10,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeds 10 bytes"));
        server.await.unwrap();

        // Progressing bytes stay below the client's inactivity timeout but
        // must still obey the metadata operation's total deadline.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..10 {
                if socket.write_all(b"x").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        let error = fetch_release_metadata_with_limits(
            &client,
            &format!("http://{address}"),
            Some("fixture-token"),
            Duration::from_millis(80),
            100,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("metadata deadline"));
        server.abort();
    }

    #[tokio::test]
    async fn stalled_update_tls_handshake_has_a_connect_deadline() {
        use std::time::Duration;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client =
            http_client_with_timeouts(Duration::from_millis(100), Duration::from_secs(30)).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("https://{address}")).send(),
        )
        .await
        .expect("bounded TLS handshake")
        .unwrap_err();
        assert!(error.is_timeout());
        server.abort();
    }


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
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/home/u/.comet-native/app/0.1.1/comet"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.comet-native/app")
            }
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/Applications/Zeron.app/Contents/MacOS/zeron"),
                Some(Path::new("/Users/u")),
                "macos",
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Zeron.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from_for_os(Path::new("/tmp/foo.app/zeron"), None, "macos"),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from_for_os(
                Path::new("/src/target/release/zeron"),
                Some(Path::new("/home/u")),
                "linux",
            ),
            InstallKind::Unmanaged
        );
    }


    #[cfg(windows)]
    #[test]
    fn windows_is_not_misclassified_as_linux() {
        assert_eq!(platform_key().0, "windows");
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_is_always_unmanaged() {
        assert_eq!(
            detect_install_from(
                Path::new(r"C:\Users\u\.zeron\app\0.2.0\zeron.exe"),
                Some(Path::new(r"C:\Users\u")),
            ),
            InstallKind::Unmanaged
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_rejects_platform_specific_updates_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "9.9.9".into(),
            files: BTreeMap::new(),
        };
        let app_root = tmp.path().join("app");
        let data_dir = tmp.path().join("data");

        let managed_err = stage_headless("http://127.0.0.1:1", None, &manifest, &app_root)
            .await
            .unwrap_err();
        assert!(managed_err.to_string().contains("not supported on windows"));
        assert!(!app_root.exists(), "managed staging must not touch disk");

        let mac_err = stage_mac_app("http://127.0.0.1:1", None, &manifest, &data_dir)
            .await
            .unwrap_err();
        assert!(mac_err.to_string().contains("not supported on windows"));
        assert!(!data_dir.exists(), "macOS staging must not touch disk");

        assert!(
            apply_headless(&app_root, &manifest.version)
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(
            apply_mac_app(&data_dir.join("Zeron.app"), &data_dir.join("Installed.app"))
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
        assert!(
            restart_service()
                .unwrap_err()
                .to_string()
                .contains("not supported on windows")
        );
    }


    #[cfg(unix)]
    #[test]
    fn managed_update_swaps_only_to_a_staged_install() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("comet"), ver).unwrap();
            std::fs::write(app_root.join(ver).join("zeron-engine"), ver).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                app_root.join(ver).join("comet"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::set_permissions(
                app_root.join(ver).join("zeron-engine"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink(app_root.join("0.1.0"), app_root.join("current")).unwrap();
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
        std::fs::create_dir_all(app_root.join("0.2.0")).unwrap();
        std::fs::write(app_root.join("0.2.0/comet"), b"main only").unwrap();
        assert!(
            apply_headless(&app_root, "0.2.0").is_err(),
            "an update without the background engine must not activate"
        );
    }
    #[test]
    fn schedule_is_wall_clock_with_failure_backoff() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut schedule = Schedule::default();
        assert!(schedule.due(start), "the first check is due immediately");

        schedule.record(start, true);
        assert!(!schedule.due(start + Duration::from_secs(59 * 60)));
        // A machine that slept through the deadline is due the moment it
        // looks at the clock again — no monotonic remainder to wait out.
        assert!(schedule.due(start + Duration::from_secs(9 * 60 * 60)));
        assert_eq!(schedule.wait(start), SCHEDULE_TICK);
        // A clock that jumped backwards does not postpone checks.
        assert!(schedule.due(start - Duration::from_secs(24 * 60 * 60)));

        let mut backoff = Vec::new();
        for _ in 0..6 {
            schedule.record(start, false);
            backoff.push(schedule.next_due.unwrap().duration_since(start).unwrap());
        }
        assert_eq!(
            backoff,
            [
                CHECK_RETRY[0],
                CHECK_RETRY[1],
                CHECK_RETRY[2],
                CHECK_RETRY[3],
                CHECK_RETRY[3],
                CHECK_RETRY[3]
            ]
        );
        schedule.record(start, true);
        assert_eq!(schedule.failures, 0);
    }

    #[test]
    fn superseded_service_restarts_once_when_quiet() {
        let restarts = std::cell::Cell::new(0);
        let restart = || {
            restarts.set(restarts.get() + 1);
            Ok(())
        };
        let mut superseded = SupersededRestart::default();
        // Same or older installed versions never restart.
        superseded.poll_installed("0.2.0", "0.2.0", || true, restart);
        superseded.poll_installed("0.1.9", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 0);
        // Busy: deferred, not attempted.
        superseded.poll_installed("0.2.1", "0.2.0", || false, restart);
        assert_eq!(restarts.get(), 0);
        // Quiet: exactly one attempt per installed version.
        superseded.poll_installed("0.2.1", "0.2.0", || true, restart);
        superseded.poll_installed("0.2.1", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 1);
        superseded.poll_installed("0.2.2", "0.2.0", || true, restart);
        assert_eq!(restarts.get(), 2);
    }

    #[test]
    fn systemd_service_cgroup_is_recognized() {
        assert_eq!(
            service_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/crew-staging.service\n"
            ),
            Some("crew-staging.service")
        );
        assert_eq!(
            service_from_cgroup("0::/user.slice/user@1000.service/crew-production.service\n"),
            Some("crew-production.service")
        );
        assert_eq!(service_from_cgroup(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/comet-native-staging.service\n"
        ), Some("comet-native-staging.service"));
        assert_eq!(service_from_cgroup(
            "12:pids:/user.slice/user@1000.service/comet-native.service\n1:name=systemd:/x\n"
        ), Some("comet-native.service"));
        assert_eq!(service_from_cgroup(
            "0::/user.slice/user-1000.slice/session-3.scope\n"
        ), None);
        assert_eq!(service_from_cgroup(
            "0::/user.slice/user@1000.service/app.slice/comet-native.service.d\n"
        ), None);
    }

    #[test]
    fn bundle_version_reads_plist_metadata() {
        let plist = "<plist><dict><key>CFBundleShortVersionString</key><string>0.3.1</string></dict></plist>";
        assert_eq!(bundle_short_version(&plist).as_deref(), Some("0.3.1"));
        assert_eq!(bundle_short_version("<plist></plist>"), None);
    }

    #[cfg(unix)]
    #[test]
    fn installed_version_follows_the_current_symlink_and_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        std::fs::create_dir_all(app_root.join("0.4.0")).unwrap();
        std::fs::write(app_root.join("0.4.0").join("zeron"), "").unwrap();
        let managed = InstallKind::Managed {
            app_root: app_root.clone(),
        };
        assert_eq!(installed_version(&managed), None, "no current link yet");
        std::os::unix::fs::symlink(app_root.join("0.4.0"), app_root.join("current")).unwrap();
        assert_eq!(installed_version(&managed).as_deref(), Some("0.4.0"));
        std::fs::remove_file(app_root.join("current")).unwrap();
        std::os::unix::fs::symlink(app_root.join("0.4.2-deadbeef"), app_root.join("current")).unwrap();
        assert_eq!(installed_version(&managed).as_deref(), Some("0.4.2"));

        let bundle = tmp.path().join("Zeron.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            "<plist><dict><key>CFBundleShortVersionString</key><string>0.4.1</string></dict></plist>",
        )
        .unwrap();
        assert_eq!(
            installed_version(&InstallKind::MacApp { bundle }).as_deref(),
            Some("0.4.1")
        );
        assert_eq!(installed_version(&InstallKind::Unmanaged), None);
    }

    #[test]
    fn mac_bundles_that_cannot_replace_themselves_are_explained() {
        let writable = |_: &Path| true;
        let read_only = |_: &Path| false;
        assert_eq!(
            mac_bundle_blocker(Path::new("/Applications/Zeron.app"), writable),
            None
        );
        assert_eq!(
            mac_bundle_blocker(
                Path::new("/private/var/folders/x/T/AppTranslocation/ABC/d/Zeron.app"),
                writable
            ),
            Some(UpdateBlocker::Translocated)
        );
        assert_eq!(
            mac_bundle_blocker(Path::new("/Volumes/Zeron/Zeron.app"), read_only),
            Some(UpdateBlocker::DiskImage)
        );
        // An external drive the user can write to updates in place.
        assert_eq!(
            mac_bundle_blocker(Path::new("/Volumes/Work/Zeron.app"), writable),
            None
        );
        assert_eq!(
            mac_bundle_blocker(Path::new("/Applications/Zeron.app"), read_only),
            Some(UpdateBlocker::NotWritable(PathBuf::from("/Applications")))
        );
    }

    #[test]
    fn writability_probe_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_writable(tmp.path()));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        assert!(!dir_writable(&tmp.path().join("missing")));
    }




    #[cfg(unix)]
    #[tokio::test]
    async fn staged_binary_verification_rejects_the_wrong_release() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("zeron");
        for version in ["1.0.0", "9.9.9"] {
            std::fs::write(&binary, format!("#!/bin/sh\necho 'comet {version}'\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let result = verify_staged_binary(&binary, "9.9.9").await;
            assert_eq!(result.is_ok(), version == "9.9.9");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn managed_cache_requires_authenticated_complete_payload() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        std::fs::create_dir_all(payload.join("crew-web")).unwrap();
        std::fs::write(payload.join("comet"), b"#!/bin/sh\necho 'comet 9.9.9'\n").unwrap();
        std::fs::set_permissions(payload.join("comet"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(payload.join("zeron-engine"), b"#!/bin/sh\necho 'comet-engine 9.9.9'\n").unwrap();
        std::fs::set_permissions(payload.join("zeron-engine"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(payload.join("crew-web/server.mjs"), b"trusted web").unwrap();
        let archive = temp.path().join("release.tar.gz");
        run("tar", &["-czf", archive.to_str().unwrap(), "-C", temp.path().to_str().unwrap(), "payload"]).unwrap();
        let bytes = std::fs::read(archive).unwrap();
        let artifact = headless_artifact("9.9.9");
        let manifest = Manifest {
            version: "9.9.9".into(),
            files: [(artifact.clone(), FileMeta { sha256: format!("{:x}", Sha256::digest(&bytes)) })].into(),
        };
        let app_root = temp.path().join("app");
        let cached = app_root.join(&manifest.version);
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(cached.join("comet"), b"untrusted cached executable").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let error = stage_headless(&origin, None, &manifest, &app_root).await.unwrap_err();
        assert!(error.to_string().contains("Sign in to Crew"));
        let server = tokio::spawn(async move {
            for _ in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET /api/releases/{artifact} HTTP/1.1\r\n")));
                assert!(request.to_ascii_lowercase().contains("authorization: bearer fixture-token\r\n"));
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await.unwrap();
                socket.write_all(&bytes).await.unwrap();
            }
        });

        assert!(stage_headless(&origin, Some("fixture-token"), &manifest, &app_root).await.is_err());
        assert_eq!(std::fs::read(cached.join("comet")).unwrap(), b"untrusted cached executable");
        std::fs::remove_dir_all(&cached).unwrap();
        assert_eq!(stage_headless(&origin, Some("fixture-token"), &manifest, &app_root).await.unwrap(), cached);
        assert_eq!(stage_headless(&origin, Some("fixture-token"), &manifest, &app_root).await.unwrap(), cached);

        // The executable still reports the right release: assets are trust-bound too.
        std::fs::write(cached.join("crew-web/server.mjs"), b"altered web").unwrap();
        assert!(stage_headless(&origin, Some("fixture-token"), &manifest, &app_root).await.is_err());
        std::fs::remove_file(cached.join("crew-web/server.mjs")).unwrap();
        symlink(payload.join("crew-web/server.mjs"), cached.join("crew-web/server.mjs")).unwrap();
        assert!(stage_headless(&origin, Some("fixture-token"), &manifest, &app_root).await.is_err());
        server.await.unwrap();

        std::fs::remove_file(cached.join("crew-web/server.mjs")).unwrap();
        std::fs::copy(payload.join("crew-web/server.mjs"), cached.join("crew-web/server.mjs")).unwrap();
        assert!(same_payload(&payload, &cached).unwrap());
        std::fs::write(cached.join("extra"), b"untracked").unwrap();
        assert!(!same_payload(&payload, &cached).unwrap());
        std::fs::remove_file(cached.join("extra")).unwrap();
        std::fs::set_permissions(cached.join("comet"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!same_payload(&payload, &cached).unwrap());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn private_release_requires_auth_digest_and_correct_channel() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        assert!(fetch_latest(&origin, None).await.is_err());
        assert!(tokio::time::timeout(Duration::from_millis(20), listener.accept()).await.is_err());
        let payload = b"verified Crew archive";
        let artifact = if cfg!(target_os = "macos") { mac_app_artifact("9.9.9") } else { headless_artifact("9.9.9") };
        let digest = format!("{:x}", Sha256::digest(payload));
        let manifest = serde_json::to_string(&Manifest {
            version: "9.9.9".into(),
            files: [(artifact.clone(), FileMeta { sha256: digest })].into(),
        }).unwrap();
        let artifact_path = format!("/api/releases/{artifact}");
        let server = tokio::spawn(async move {
            for (path, status, body) in [
                (format!("/api/releases/{}", release_manifest_name()), "200 OK", manifest.into_bytes()),
                (artifact_path.clone(), "200 OK", payload.to_vec()),
                (artifact_path, "200 OK", b"tampered".to_vec()),
                (format!("/api/releases/{}", release_manifest_name()), "401 Unauthorized", Vec::new()),
                (format!("/api/releases/{}", release_manifest_name()), "200 OK", br#"{"version":"9.9.9","files":{}}"#.to_vec()),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                assert!(request.to_ascii_lowercase().contains("authorization: bearer fixture-token\r\n"));
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let manifest = fetch_latest(&origin, Some("fixture-token")).await.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("archive");
        download_release_file(&origin, Some("fixture-token"), &manifest, &artifact, &destination).await.unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        assert!(download_release_file(&origin, Some("fixture-token"), &manifest, &artifact, &destination).await.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        assert!(!destination.with_extension("partial").exists());
        assert!(fetch_latest(&origin, Some("fixture-token")).await.is_err());
        assert!(fetch_latest(&origin, Some("fixture-token")).await.is_err());
        server.await.unwrap();
    }

    #[test]
    fn release_origins_cannot_override_the_private_channel() {
        for origin in ["http://example.com", "https://example.com/releases", "https://user@example.com", "https://example.com?feed=x", "https://example.com/#feed"] {
            assert!(release_base(origin).is_err());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn release_redirect_cannot_leave_authenticated_origin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", origin.local_addr().unwrap());
        let location = format!("http://{}/api/releases/manifest.json", other.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        assert!(fetch_latest(&url, Some("fixture-token")).await.is_err());
        server.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), other.accept()).await.is_err());
    }
}
