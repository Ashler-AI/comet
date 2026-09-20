//! Explicit Namespace wake, shared across local RPC connections. No background wake.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use comet_proto::{Device, DeviceEnvironment};
use comet_rpc::LinkCache;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, WeakShared};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const BOOT_TIMEOUT: Duration = Duration::from_secs(110);
const RELAY_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_LIMIT: usize = 16 * 1024;
type WakeFuture = BoxFuture<'static, Result<(), String>>;

#[derive(Default)]
pub(crate) struct DeviceWake {
    operations: Mutex<HashMap<String, WeakShared<WakeFuture>>>,
}

impl DeviceWake {
    pub(crate) async fn wake(
        &self,
        device_id: &str,
        provider_id: String,
        project_scope: &str,
        links: Arc<LinkCache>,
    ) -> Result<(), String> {
        let target = device_id.to_owned();
        let command = bootstrap_command(project_scope)?;
        self.coalesce(device_id, async move {
            let executable = devbox_executable().ok_or_else(|| {
                "Namespace CLI not found. Install devbox (including ~/.local/bin/devbox), then run devbox login on this controller.".to_string()
            })?;
            run_bootstrap(executable, &provider_id, command).await?;
            await_peer(&links, &target).await
        }.boxed()).await
    }

    fn coalesce(&self, device_id: &str, operation: WakeFuture) -> Shared<WakeFuture> {
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = operations.get(device_id).and_then(WeakShared::upgrade) {
            return pending;
        }
        operations.retain(|_, operation| operation.upgrade().is_some());
        let shared = operation.shared();
        // The map must not keep the future alive: cancelling the last caller
        // drops the subprocess (kill_on_drop); another caller keeps it running.
        operations.insert(
            device_id.to_owned(),
            shared.downgrade().expect("new wake future"),
        );
        shared
    }
}

pub(crate) fn valid_devbox_id(id: &str) -> bool {
    // Namespace immutable IDs are 13 lowercase alphanumeric characters, not names.
    id.len() == 13
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub(crate) fn bound_devbox_id<'a>(device: &'a Device, local_id: &str) -> Result<&'a str, String> {
    if device.id == local_id || device.environment != Some(DeviceEnvironment::Namespace) {
        return Err("Only a remote Namespace Devbox can be woken".into());
    }
    device.namespace_devbox_id.as_deref().filter(|id| valid_devbox_id(id)).ok_or_else(|| {
        "This device has no valid Namespace Devbox ID. Reconfigure its Crew launcher with NAMESPACE_DEVBOX_ID and register it again; the display name cannot be used.".into()
    })
}

fn devbox_executable() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "devbox.exe"
    } else {
        "devbox"
    };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin").join(executable));
    }
    candidates.extend(
        ["/opt/homebrew/bin", "/usr/local/bin"].map(|dir| PathBuf::from(dir).join(executable)),
    );
    candidates.into_iter().find(|path| {
        let Ok(metadata) = path.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

async fn capture_bounded(mut stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(output);
        }
        let keep = count.min(OUTPUT_LIMIT - output.len());
        output.extend_from_slice(&buffer[..keep]);
        // Drain excess bytes without retaining them, so a verbose CLI cannot
        // deadlock on its output pipe or grow the controller's memory unboundedly.
    }
}

fn bootstrap_command(project_scope: &str) -> Result<&'static str, String> {
    match project_scope {
        "ashler-staging" => Ok("exec \"$HOME/.local/bin/crew-devbox-autostart\" staging"),
        "ashler-production" => Ok("exec \"$HOME/.local/bin/crew-devbox-autostart\" production"),
        _ => Err("Wake Device requires a staging or production Crew controller; this project is not supported".into()),
    }
}

async fn run_bootstrap(
    executable: PathBuf,
    provider_id: &str,
    command: &'static str,
) -> Result<(), String> {
    let mut child = Command::new(executable)
        .args(["exec", provider_id, "--", "sh", "-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not start Namespace devbox CLI: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (status, stdout, stderr) = tokio::time::timeout(BOOT_TIMEOUT, async {
        tokio::try_join!(child.wait(), capture_bounded(stdout), capture_bounded(stderr))
    }).await.map_err(|_| {
        "Namespace startup timed out after 110 seconds. Check devbox login and the machine's status, then retry.".to_string()
    })?.map_err(|error| format!("Could not read Namespace startup result: {error}"))?;
    if status.success() {
        return Ok(());
    }
    // Provider output can contain sensitive auth URLs. Classify it locally, never
    // expose the raw output to workspace peers, logs, or the UI.
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    )
    .to_ascii_lowercase();
    let action = if [
        "unauthenticated",
        "unauthorized",
        "not logged in",
        "login",
        "credential",
        "permission denied",
    ]
    .iter()
    .any(|s| output.contains(s))
    {
        "Namespace authentication failed. Run devbox login on this controller and verify access to this Devbox."
    } else if ["expired", "not found", "no devbox", "does not exist"]
        .iter()
        .any(|s| output.contains(s))
        && !output.contains("crew-devbox-autostart")
    {
        "Namespace could not find this Devbox; it may have expired or been deleted. Verify the bound machine with devbox list; Crew will not create a replacement."
    } else if output.contains("crew-devbox-autostart")
        || output.contains("crew-devbox-staging")
        || output.contains("crew-devbox-production")
    {
        "Crew's Devbox startup helper failed or is missing. Reinstall ~/.local/bin/crew-devbox-autostart and verify the matching channel launcher is signed in."
    } else {
        "Namespace could not boot Crew. Check the Devbox in Namespace and run its existing crew-devbox-autostart helper to inspect the failure."
    };
    Err(format!("{action} (CLI {status})"))
}

async fn await_peer(links: &Arc<LinkCache>, device_id: &str) -> Result<(), String> {
    // A pre-sleep cached socket is not readiness evidence. Force the existing
    // LinkCache identity round-trip and discard pre-wake failure cooldowns.
    links.invalidate(device_id);
    let mut last_error = "no relay response".to_string();
    tokio::time::timeout(RELAY_TIMEOUT, async {
        loop {
            links.reset_cooldown(device_id);
            match links.client(device_id).await {
                Ok(_) => return,
                Err(error) => last_error = error.to_string(),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }).await.map_err(|_| format!(
        "Devbox started, but Crew did not connect to its relay within 60 seconds. Check the matching channel launcher's sign-in, project, and network, then retry. Last relay error: {last_error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn timeout_releases_all_waiters_and_allows_retry() {
        let wake = DeviceWake::default();
        let first = wake.coalesce(
            "device",
            async {
                tokio::time::timeout(BOOT_TIMEOUT, futures::future::pending::<()>())
                    .await
                    .map_err(|_| "startup timeout".to_string())
            }
            .boxed(),
        );
        let second = wake.coalesce("device", async { panic!("duplicate boot") }.boxed());
        let independent = wake.coalesce("other-device", async { Ok(()) }.boxed());
        let (a, b, other) = tokio::join!(first, second, independent);
        assert_eq!(a, Err("startup timeout".into()));
        assert_eq!(a, b);
        assert_eq!(other, Ok(()));
        assert_eq!(
            wake.coalesce("device", async { Ok(()) }.boxed()).await,
            Ok(())
        );
    }

    #[test]
    fn wake_requires_bound_remote_namespace_id() {
        let mut device: Device = serde_json::from_value(serde_json::json!({
            "id": "remote", "name": "Owner Devbox", "platform": "linux", "lastSeenAt": null
        }))
        .unwrap();
        device.namespace_devbox_id = Some("ofpf7g22n4412".into());
        assert!(bound_devbox_id(&device, "local").is_err());
        device.environment = Some(DeviceEnvironment::Namespace);
        assert_eq!(bound_devbox_id(&device, "local").unwrap(), "ofpf7g22n4412");
        assert!(bound_devbox_id(&device, "remote").is_err());
        for invalid in [
            None,
            Some(""),
            Some("machine-name"),
            Some("--show-all"),
            Some("ofpf7g22n4412;id"),
            Some("/ofpf7g22n4412"),
            Some(" ofpf7g22n4412"),
        ] {
            device.namespace_devbox_id = invalid.map(str::to_owned);
            assert!(bound_devbox_id(&device, "local").is_err());
        }
    }

    #[tokio::test]
    async fn concurrent_waiters_share_failure_then_can_retry() {
        let wake = DeviceWake::default();
        let started = Arc::new(AtomicUsize::new(0));
        let (ready, receiver) = tokio::sync::oneshot::channel::<()>();
        let count = started.clone();
        let first = wake.coalesce(
            "device",
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                receiver.await.unwrap();
                Err("boot failed".into())
            }
            .boxed(),
        );
        let second = wake.coalesce("device", async { panic!("duplicate boot") }.boxed());
        assert_eq!(started.load(Ordering::SeqCst), 0);
        ready.send(()).unwrap();
        let (a, b) = tokio::join!(first, second);
        assert_eq!(a, Err("boot failed".into()));
        assert_eq!(a, b);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(
            wake.coalesce("device", async { Ok(()) }.boxed()).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_other_waiter_but_last_drop_releases_operation() {
        let wake = DeviceWake::default();
        let (ready, receiver) = tokio::sync::oneshot::channel::<()>();
        let first = wake.coalesce(
            "device",
            async move { receiver.await.map_err(|e| e.to_string()) }.boxed(),
        );
        let second = wake.coalesce("device", async { panic!("duplicate boot") }.boxed());
        drop(first);
        assert!(!ready.is_closed());
        drop(second);
        assert!(ready.is_closed());
        assert_eq!(
            wake.coalesce("device", async { Ok(()) }.boxed()).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn output_is_drained_but_not_retained_past_limit() {
        let bytes = vec![b'x'; OUTPUT_LIMIT * 3];
        let mut input = bytes.as_slice();
        assert_eq!(
            capture_bounded(&mut input).await.unwrap(),
            bytes[..OUTPUT_LIMIT]
        );
        assert!(input.is_empty());
    }
}
