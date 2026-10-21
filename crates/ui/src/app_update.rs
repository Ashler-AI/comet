//! Desktop self-update: the app's own release checker plus the download /
//! install lifecycle behind the sidebar update strip, "Check for Updates…"
//! (macOS app menu; the account menu elsewhere) and install-on-quit.
//!
//! The checker runs in this process against this binary's version. The
//! engine keeps its own checker for daemons and remote reporting, but the UI
//! may be attached to a daemon of another version, whose status describes
//! that binary rather than the app the user is looking at.
//! Authentication comes from that engine's existing Auth service over local
//! IPC (or the embedded transport), bound to the configured release origin;
//! the desktop never opens another credential store or login flow.
//!
//! Lifecycle, idiomatic to desktop updaters (Sparkle, VS Code, Zed): a found
//! release downloads in the background and is verified, the strip offers
//! "restart to apply", and a staged update the user never restarts for is
//! installed when the app quits — so nobody stays on a stale version just
//! because they never clicked. `COMET_AUTO_UPDATE=0` keeps it report-only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use gpui_tokio::Tokio;
use zeron_update::{InstallKind, UpdateBlocker, UpdateStatus, Updater};

/// Download/install lifecycle of the newest release in this process.
#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    Idle,
    Downloading {
        version: String,
    },
    /// Staged and verified: "restart to apply", or installed on quit.
    Ready {
        version: String,
        staged: PathBuf,
    },
    Failed {
        version: String,
        message: SharedString,
    },
    /// Swapped in; this process is on its way out.
    Installed,
}

/// The user-initiated check's dialog.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    Checking,
    /// The check finished; the dialog renders from the live status + flow.
    Result,
    CheckFailed(SharedString),
}

/// What clicking the sidebar strip does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripAction {
    /// Nothing — progress only.
    None,
    Download,
    Restart,
    /// Explain why this install can't update itself (dialog).
    Explain,
}

pub struct AppUpdate {
    install: InstallKind,
    blocker: Option<UpdateBlocker>,
    /// Background download + install on quit (`COMET_AUTO_UPDATE` unset or on).
    automatic: bool,
    edge_url: String,
    data_dir: PathBuf,
    checker: Updater,
    engine_tx: tokio::sync::watch::Sender<Option<crate::state::EngineHandle>>,
    status: Option<UpdateStatus>,
    flow: Flow,
    prompt: Option<Prompt>,
    _status_watch: Task<()>,
    download: Option<Task<()>>,
    user_check: Option<Task<()>>,
}

struct GlobalAppUpdate(Entity<AppUpdate>);

impl Global for GlobalAppUpdate {}

impl AppUpdate {
    /// Start the desktop checker. Call once at boot, after `gpui_tokio` is
    /// initialized.
    pub fn init(edge_url: String, data_dir: PathBuf, cx: &mut App) {
        let entity = cx.new(|cx| Self::new(edge_url, data_dir, cx));
        cx.set_global(GlobalAppUpdate(entity));
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAppUpdate>()
            .map(|global| global.0.clone())
    }

    fn new(edge_url: String, data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let install = zeron_update::detect_install();
        let blocker = install
            .supports_desktop_update()
            .then(|| install.desktop_update_blocker())
            .flatten();
        let (engine_tx, engine_rx) = tokio::sync::watch::channel::<Option<crate::state::EngineHandle>>(None);
        let token_edge_url = edge_url.clone();
        let token_provider: zeron_update::AccessTokenProvider = Arc::new(move || {
            let engine = engine_rx.borrow().clone();
            let edge_url = token_edge_url.clone();
            Box::pin(async move {
                let engine = engine.ok_or_else(|| anyhow::anyhow!("Connect to the local engine before checking for updates"))?;
                Ok(engine.client().update_access_token(&edge_url).await?)
            })
        });
        let checker = {
            let _runtime = Tokio::handle(cx).enter();
            Updater::spawn_desktop(edge_url.clone(), token_provider)
        };
        let mut statuses = checker.watch();
        let status_watch = cx.spawn(async move |this, cx| {
            while statuses.changed().await.is_ok() {
                let status = statuses.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| this.apply_status(status, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        tracing::info!(?install, ?blocker, "desktop update checker started");
        Self {
            install,
            blocker,
            automatic: zeron_update::desktop_auto_update_enabled(),
            edge_url,
            data_dir,
            checker,
            engine_tx,
            status: None,
            flow: Flow::Idle,
            prompt: None,
            _status_watch: status_watch,
            download: None,
            user_check: None,
        }
    }

    /// Both embedded and IPC-attached desktops borrow the engine's existing auth.
    pub fn set_engine(&mut self, engine: Option<crate::state::EngineHandle>) {
        let connected = engine.is_some();
        self.engine_tx.send_replace(engine);
        if connected {
            self.checker.check_now();
        }
    }

    pub fn install(&self) -> &InstallKind {
        &self.install
    }

    pub fn blocker(&self) -> Option<&UpdateBlocker> {
        self.blocker.as_ref()
    }

    pub fn flow(&self) -> &Flow {
        &self.flow
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// The newer release the last check found, if any.
    pub fn available(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|status| status.update_available)
            .and_then(|status| status.latest_version.as_deref())
    }

    /// Whether this app downloads and installs updates itself right now.
    pub fn self_updating(&self) -> bool {
        self.install.supports_desktop_update() && self.blocker.is_none()
    }

    /// Window activation / wake: check if one is due by the wall clock.
    pub fn poke(&self) {
        self.checker.poke();
    }

    fn apply_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        let succeeded = status.error.is_none();
        self.status = Some(status);
        if succeeded && self.automatic {
            self.download_if_needed(cx);
        }
        cx.notify();
    }

    /// Start (or restart) the background download unless the newest release
    /// is already downloading or staged. A failed download retries at the
    /// next successful check.
    fn download_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.self_updating() {
            return;
        }
        let Some(latest) = self.available() else {
            return;
        };
        let current = match &self.flow {
            Flow::Downloading { .. } | Flow::Installed => return,
            Flow::Ready { version, .. } => Some(version),
            Flow::Idle | Flow::Failed { .. } => None,
        };
        if current.is_some_and(|version| !zeron_update::version_newer(latest, version)) {
            return;
        }
        self.start_download(cx);
    }

    pub fn start_download(&mut self, cx: &mut Context<Self>) {
        if !self.self_updating() || matches!(self.flow, Flow::Downloading { .. }) {
            return;
        }
        let Some(version) = self.available().map(str::to_owned) else {
            return;
        };
        let edge_url = self.edge_url.clone();
        let checker = self.checker.clone();
        let data_dir = self.data_dir.clone();
        let install = self.install.clone();
        self.flow = Flow::Downloading {
            version: version.clone(),
        };
        let stage = Tokio::spawn(cx, async move {
            // Re-read the manifest so a long-lived status still downloads the
            // newest release, with that release's checksums.
            let manifest = checker.fetch_latest().await?;
            anyhow::ensure!(
                zeron_update::version_newer(&manifest.version, zeron_update::current_version()),
                "the release feed no longer offers a newer version"
            );
            let access_token = checker.current_access_token().await?;
            let staged = install
                .stage_desktop(&edge_url, Some(&access_token), &manifest, &data_dir)
                .await?;
            anyhow::Ok((manifest.version, staged))
        });
        self.download = Some(cx.spawn(async move |this, cx| {
            let outcome = match stage.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join) => Err(join.to_string()),
            };
            this.update(cx, |this, cx| {
                this.flow = match outcome {
                    Ok((version, staged)) => {
                        tracing::info!(%version, staged = %staged.display(), "update staged");
                        Flow::Ready { version, staged }
                    }
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        Flow::Failed {
                            version,
                            message: message.into(),
                        }
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// "Check for Updates…": check now and report the outcome in a dialog. A
    /// found release starts downloading at once (it would anyway on the
    /// hourly check), and a failed download gets a fresh attempt.
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.prompt = Some(Prompt::Checking);
        let checker = self.checker.clone();
        let check = Tokio::spawn(cx, async move { checker.check().await });
        self.user_check = Some(cx.spawn(async move |this, cx| {
            let result = match check.await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(err)) => {
                    tracing::warn!(error = %format!("{err:#}"), "update check failed");
                    // The dialog shows the cause; the log keeps the chain.
                    Err(err.root_cause().to_string())
                }
                Err(join) => Err(join.to_string()),
            };
            this.update(cx, |this, cx| {
                let outcome = match result {
                    Ok(status) => {
                        this.status = Some(status);
                        if this.automatic {
                            this.download_if_needed(cx);
                        }
                        Prompt::Result
                    }
                    Err(message) => Prompt::CheckFailed(message.into()),
                };
                // The user may have closed the dialog while it was checking.
                if this.prompt.is_some() {
                    this.prompt = Some(outcome);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub fn dismiss_prompt(&mut self, cx: &mut Context<Self>) {
        self.prompt = None;
        self.user_check = None;
        cx.notify();
    }

    /// Open the dialog on its current result (the strip's "explain" action).
    pub fn show_result(&mut self, cx: &mut Context<Self>) {
        self.prompt = Some(Prompt::Result);
        cx.notify();
    }

    /// The staged update "Restart to update" would install.
    pub fn staged(&self) -> Option<PathBuf> {
        match &self.flow {
            Flow::Ready { staged, .. } => Some(staged.clone()),
            _ => None,
        }
    }

    /// Install `staged` and arrange the relaunch. The caller quits on success.
    pub fn install_for_restart(
        &mut self,
        staged: &Path,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let result = self.install.apply_desktop(staged, true);
        self.flow = match &result {
            Ok(()) => Flow::Installed,
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                Flow::Failed {
                    version: self.available().unwrap_or_default().to_owned(),
                    message: format!("{err:#}").into(),
                }
            }
        };
        cx.notify();
        result
    }

    /// Quit hook: a staged update the user never restarted for installs now,
    /// so the next launch is current. Runs synchronously inside the quit.
    fn install_on_quit(&mut self) {
        if !self.automatic {
            return;
        }
        let Flow::Ready { version, staged } = &self.flow else {
            return;
        };
        match self.install.apply_desktop(staged, false) {
            Ok(()) => {
                tracing::info!(%version, "installed staged update on quit");
                self.flow = Flow::Installed;
            }
            Err(err) => tracing::warn!(error = %err, "installing the staged update on quit failed"),
        }
    }

    /// The sidebar strip for the current state: `None` while nothing is newer.
    pub fn strip(&self) -> Option<(SharedString, StripAction)> {
        let latest = self.available()?;
        Some(strip_for(
            &self.install,
            self.blocker.is_some(),
            &self.flow,
            latest,
        ))
    }
}

/// Label + click action of the strip. Self-updating installs drive their flow
/// from it; blocked installs explain themselves; managed installs without a
/// desktop path get the `comet update` hint; unmanaged installs (source builds,
/// hand-copied binaries) require the established private release workflow.
pub fn strip_for(
    install: &InstallKind,
    blocked: bool,
    flow: &Flow,
    latest: &str,
) -> (SharedString, StripAction) {
    if install.supports_desktop_update() {
        if blocked {
            return (
                format!("Update available — v{latest}").into(),
                StripAction::Explain,
            );
        }
        return match flow {
            Flow::Idle => (
                format!("Update available — v{latest}").into(),
                StripAction::Download,
            ),
            Flow::Downloading { version } => {
                (format!("Downloading v{version}…").into(), StripAction::None)
            }
            Flow::Ready { .. } => (
                "Update ready — restart to apply".into(),
                StripAction::Restart,
            ),
            Flow::Failed { message, .. } => (
                format!("Update failed: {message}").into(),
                StripAction::Download,
            ),
            Flow::Installed => ("Restarting…".into(), StripAction::None),
        };
    }
    if matches!(install, InstallKind::Managed { .. }) {
        (
            format!("Update available — v{latest} · run `comet update`").into(),
            StripAction::Explain,
        )
    } else {
        (
            format!("Update available — v{latest} · unmanaged install").into(),
            StripAction::Explain,
        )
    }
}

/// "Check for Updates…" from a menu: surface the main window, then check.
pub fn check_for_updates(cx: &mut App) {
    crate::activate_main_window(cx);
    if let Some(update) = AppUpdate::global(cx) {
        update.update(cx, |update, cx| update.check_for_updates(cx));
    }
}

/// See [`AppUpdate::install_on_quit`].
pub fn install_on_quit(cx: &mut App) {
    if let Some(update) = AppUpdate::global(cx) {
        update.update(cx, |update, _| update.install_on_quit());
    }
}

