//! Explicit Namespace wake, shared across local RPC connections. No background wake.

use std::collections::HashMap;
use std::fmt::Write as _;
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
const AUTH_FORWARD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const AUTH_FORWARD_READY_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_FORWARD_PORTS: &[(u16, u16)] = &[(8085, 8085)];
const SERVICE_FORWARD_PORTS: &[(u16, u16)] = &[(13_000, 3000), (20_350, 10_350)];
const OUTPUT_LIMIT: usize = 16 * 1024;
type WakeFuture = BoxFuture<'static, Result<(), String>>;

#[derive(Default)]
pub(crate) struct DeviceWake {
    operations: Mutex<HashMap<String, WeakShared<WakeFuture>>>,
    forward_operations: Mutex<HashMap<String, WeakShared<WakeFuture>>>,
    auth_forward: Arc<Mutex<Option<String>>>,
    service_forward: Arc<Mutex<Option<String>>>,
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
        let executable = devbox_executable().ok_or_else(|| {
            "Namespace CLI not found. Install devbox (including ~/.local/bin/devbox), then run devbox login on this controller.".to_string()
        })?;
        let boot_executable = executable.clone();
        let forwards = self.prepare_forwards(executable, &provider_id);
        self.coalesce(
            device_id,
            async move {
                run_bootstrap(boot_executable, &provider_id, command).await?;
                await_peer(&links, &target).await?;
                let _ = await_forwards(forwards).await;
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    pub(crate) async fn ensure_callback_forward(&self, provider_id: String) -> Result<(), String> {
        let executable = devbox_executable().ok_or_else(|| {
            "Namespace CLI not found. Install devbox (including ~/.local/bin/devbox), then run devbox login on this controller.".to_string()
        })?;
        await_forwards(self.prepare_forwards(executable, &provider_id)).await
    }

    pub(crate) fn auth_forward_active(&self, provider_id: &str) -> bool {
        self.auth_forward
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            == Some(provider_id)
    }
    fn prepare_forwards(
        &self,
        executable: PathBuf,
        provider_id: &str,
    ) -> (Shared<WakeFuture>, Option<Shared<WakeFuture>>) {
        let auth_key = format!("auth:{provider_id}");
        let auth = self.coalesce_forward(
            &auth_key,
            ensure_auth_forward(
                executable.clone(),
                provider_id.to_owned(),
                AUTH_FORWARD_PORTS.to_vec(),
                AUTH_FORWARD_READY_TIMEOUT,
                AUTH_FORWARD_TIMEOUT,
                self.auth_forward.clone(),
            )
            .boxed(),
        );
        let service_ports = available_service_ports(SERVICE_FORWARD_PORTS);
        let services = if service_ports.is_empty() {
            None
        } else {
            let service_key = format!("services:{provider_id}");
            Some(
                self.coalesce_forward(
                    &service_key,
                    ensure_auth_forward(
                        executable,
                        provider_id.to_owned(),
                        service_ports,
                        AUTH_FORWARD_READY_TIMEOUT,
                        AUTH_FORWARD_TIMEOUT,
                        self.service_forward.clone(),
                    )
                    .boxed(),
                ),
            )
        };
        (auth, services)
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

    fn coalesce_forward(&self, provider_id: &str, operation: WakeFuture) -> Shared<WakeFuture> {
        let mut operations = self
            .forward_operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = operations.get(provider_id).and_then(WeakShared::upgrade) {
            return pending;
        }
        operations.retain(|_, operation| operation.upgrade().is_some());
        let shared = operation.shared();
        operations.insert(
            provider_id.to_owned(),
            shared.downgrade().expect("new forward future"),
        );
        shared
    }
}

async fn await_forwards(
    (auth, services): (Shared<WakeFuture>, Option<Shared<WakeFuture>>),
) -> Result<(), String> {
    let service = async move {
        if let Some(services) = services {
            services.await
        } else {
            Ok(())
        }
    };
    let (auth, service) = tokio::join!(auth, service);
    if let Err(error) = service {
        tracing::warn!(error, "Namespace app/Tilt forward unavailable");
    }
    auth
}

fn local_port_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn available_service_ports(ports: &[(u16, u16)]) -> Vec<(u16, u16)> {
    ports
        .iter()
        .copied()
        .filter(|(local, remote)| {
            let available = local_port_available(*local);
            if !available {
                tracing::warn!(local, remote, "Skipping occupied Namespace service forward");
            }
            available
        })
        .collect()
}

fn forward_port_spec(ports: &[(u16, u16)]) -> String {
    let mut spec = String::with_capacity(ports.len() * 12);
    for (index, (local, remote)) in ports.iter().enumerate() {
        if index > 0 {
            spec.push(',');
        }
        write!(spec, "{local}:{remote}").expect("writing to String cannot fail");
    }
    spec
}

fn release_auth_forward(active: &Mutex<Option<String>>, provider_id: &str) {
    let mut forward = active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if forward.as_deref() == Some(provider_id) {
        *forward = None;
    }
}

struct AuthForwardLease {
    active: Arc<Mutex<Option<String>>>,
    provider_id: String,
}

impl Drop for AuthForwardLease {
    fn drop(&mut self) {
        release_auth_forward(&self.active, &self.provider_id);
    }
}

async fn ensure_auth_forward(
    executable: PathBuf,
    provider_id: String,
    ports: Vec<(u16, u16)>,
    ready_timeout: Duration,
    lease_duration: Duration,
    active: Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    {
        let mut forward = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match forward.as_deref() {
            Some(current) if current == provider_id => return Ok(()),
            Some(_) => return Err("Another Namespace port forward is already active".into()),
            None => {}
        }
        if let Some((port, _)) = ports.iter().find(|(port, _)| !local_port_available(*port)) {
            return Err(format!("Namespace forward port {port} is already in use"));
        }
        *forward = Some(provider_id.clone());
    }
    let lease = AuthForwardLease {
        active: active.clone(),
        provider_id: provider_id.clone(),
    };
    let port_spec = forward_port_spec(&ports);
    let mut child = Command::new(executable)
        .args(["port-forward", &provider_id, "--ports", &port_spec])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not start Namespace port forward: {error}"))?;
    let ready = tokio::time::timeout(ready_timeout, async {
        loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return Err(format!("Namespace port forward exited early ({status})"));
            }
            if tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, ports[0].0))
                .await
                .is_ok()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err("Namespace port forward did not become ready within 10 seconds".to_string())
    });
    if let Err(error) = ready {
        let _ = child.kill().await;
        return Err(error);
    }
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(lease_duration) => {
                let _ = child.kill().await;
            }
            _ = child.wait() => {}
        }
        drop(lease);
    });
    Ok(())
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
        return Err("Only a remote Namespace Devbox can be controlled".into());
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

    fn unused_port() -> u16 {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

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

    #[tokio::test]
    async fn concurrent_forward_waiters_share_readiness() {
        let wake = DeviceWake::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let first_starts = starts.clone();
        let first = wake.coalesce_forward(
            "ofpf7g22n4412",
            async move {
                first_starts.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(())
            }
            .boxed(),
        );
        let second = wake.coalesce_forward(
            "ofpf7g22n4412",
            async { panic!("duplicate forward") }.boxed(),
        );
        let (a, b) = tokio::join!(first, second);
        assert_eq!(a, Ok(()));
        assert_eq!(a, b);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn occupied_service_port_does_not_drop_other_mappings() {
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let open_port = unused_port();
        assert_eq!(
            available_service_ports(&[(occupied_port, 3000), (open_port, 10_350)]),
            vec![(open_port, 10_350)]
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

    #[test]
    fn bootstrap_commands_follow_the_crew_channel() {
        assert!(
            bootstrap_command("ashler-staging")
                .unwrap()
                .ends_with(" staging")
        );
        assert!(
            bootstrap_command("ashler-production")
                .unwrap()
                .ends_with(" production")
        );
        assert!(bootstrap_command("other").is_err());
    }

    #[test]
    fn default_forward_ports_are_split_by_lifecycle() {
        assert_eq!(forward_port_spec(AUTH_FORWARD_PORTS), "8085:8085");
        assert_eq!(
            forward_port_spec(SERVICE_FORWARD_PORTS),
            "13000:3000,20350:10350"
        );
    }

    #[tokio::test]
    async fn failed_callback_tunnel_can_retry() {
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            provider_id.into(),
            vec![(unused_port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(!wake.auth_forward_active(provider_id));
    }

    #[tokio::test]
    async fn occupied_ports_and_other_devices_fail_without_spawning() {
        let wake = DeviceWake::default();
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            "ofpf7g22n4412".into(),
            vec![(occupied.local_addr().unwrap().port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.unwrap_err().contains("already in use"));
        assert!(!wake.auth_forward_active("ofpf7g22n4412"));

        *wake.auth_forward.lock().unwrap() = Some("another-device".into());
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            "ofpf7g22n4412".into(),
            vec![(unused_port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.unwrap_err().contains("Another Namespace"));
        assert_eq!(
            wake.auth_forward.lock().unwrap().as_deref(),
            Some("another-device")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_callback_tunnel_can_retry() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("devbox");
        std::fs::write(&executable, "#!/bin/sh\nsleep 60\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        let port = unused_port();
        let result = ensure_auth_forward(
            executable,
            provider_id.into(),
            vec![(port, 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(!wake.auth_forward_active(provider_id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_readiness_releases_callback_lease() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("devbox");
        std::fs::write(&executable, "#!/bin/sh\nsleep 60\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        let ports = [(unused_port(), 8085)];
        {
            let pending = ensure_auth_forward(
                executable,
                provider_id.into(),
                ports.to_vec(),
                Duration::from_secs(60),
                Duration::from_secs(60),
                wake.auth_forward.clone(),
            );
            tokio::pin!(pending);
            tokio::select! {
                result = &mut pending => panic!("tunnel exited before cancellation: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            assert!(wake.auth_forward_active(provider_id));
        }
        assert!(!wake.auth_forward_active(provider_id));
    }

    #[cfg(unix)]
    #[test]
    fn auth_forward_child() {
        let Ok(raw) = std::env::var("COMET_TEST_AUTH_FORWARD_PORT") else {
            return;
        };
        let _callback = std::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            raw.parse::<u16>().unwrap(),
        ))
        .unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn callback_tunnel_becomes_ready_and_is_coalesced() {
        use std::os::unix::fs::PermissionsExt;

        let callback_port = unused_port();

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("devbox");
        let test_binary = std::env::current_exe().unwrap();
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{callback_port}' exec '{}' --exact device_wake::tests::auth_forward_child --nocapture\n",
                test_binary.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        ensure_auth_forward(
            executable.clone(),
            provider_id.into(),
            vec![(callback_port, 8085)],
            Duration::from_secs(2),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        assert!(wake.auth_forward_active(provider_id));
        ensure_auth_forward(
            executable,
            provider_id.into(),
            vec![(callback_port, 8085)],
            Duration::from_secs(2),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!wake.auth_forward_active(provider_id));
        assert!(
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, callback_port))
                .await
                .is_err()
        );
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
