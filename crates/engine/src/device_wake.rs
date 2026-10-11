//! Explicit provider-bound Devbox wake, shared across local RPC connections.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use parking_lot::Mutex;
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
        provider: DeviceEnvironment,
        provider_id: String,
        project_scope: &str,
        links: Arc<LinkCache>,
    ) -> Result<(), String> {
        let target = device_id.to_owned();
        let command = bootstrap_command(project_scope)?;
        let executable = provider_executable(provider)?;
        let boot_executable = executable.clone();
        let forwards = self.prepare_forwards(executable, provider, &provider_id);
        let key = format!("{}:{provider_id}:{device_id}", provider_name(provider));
        self.coalesce(
            &key,
            async move {
                run_bootstrap(boot_executable, provider, &provider_id, command).await?;
                await_peer(&links, &target).await?;
                let _ = await_forwards(forwards).await;
                Ok(())
            }
            .boxed(),
        )
        .await
    }

    pub(crate) async fn ensure_callback_forward(&self, provider: DeviceEnvironment, provider_id: String) -> Result<(), String> {
        let executable = provider_executable(provider)?;
        await_forwards(self.prepare_forwards(executable, provider, &provider_id)).await
    }

    pub(crate) fn auth_forward_active(&self, provider: DeviceEnvironment, provider_id: &str) -> bool {
        self.auth_forward
            .lock()
            .as_deref()
            .and_then(|key| key.split_once(':'))
            == Some((provider_name(provider), provider_id))
    }
    fn prepare_forwards(
        &self,
        executable: PathBuf,
        provider: DeviceEnvironment,
        provider_id: &str,
    ) -> (Shared<WakeFuture>, Option<Shared<WakeFuture>>) {
        let key = provider_key(provider, provider_id);
        let auth_key = format!("auth:{key}");
        let auth = self.coalesce_forward(
            &auth_key,
            ensure_auth_forward(
                executable.clone(),
                provider,
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
            let service_key = format!("services:{key}");
            Some(
                self.coalesce_forward(
                    &service_key,
                    ensure_auth_forward(
                        executable,
                        provider,
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
            .lock();
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
            .lock();
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
        tracing::warn!(error, "Devbox app/Tilt forward unavailable");
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
                tracing::warn!(local, remote, "Skipping occupied Devbox service forward");
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
        .lock();
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

struct ForwardProcessGroup {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: Option<u32>,
}

impl ForwardProcessGroup {
    fn spawn(command: &mut Command) -> std::io::Result<Self> {
        command.kill_on_drop(!cfg!(unix));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        let child = command.spawn()?;
        Ok(Self {
            #[cfg(unix)]
            group: child.id(),
            child,
        })
    }

    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            // SAFETY: the unreaped child reserves this group ID. Disarm before
            // reaping, so cancellation cannot signal a subsequently reused ID.
            unsafe {
                libc::killpg(group as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    fn has_exited(&mut self) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            let Some(group) = self.group else {
                return Ok(true);
            };
            // Observe exit without reaping: descendants may still hold listeners,
            // and the leader must reserve the group ID until we kill the group.
            let mut status = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    group as libc::id_t,
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.group = None;
                }
                return Err(error);
            }
            Ok(unsafe { status.si_pid() } != 0)
        }
        #[cfg(not(unix))]
        {
            self.child.try_wait().map(|status| status.is_some())
        }
    }

    async fn wait_for_exit(&mut self) -> std::io::Result<()> {
        loop {
            if self.has_exited()? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(&mut self) {
        self.kill_group();
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

impl Drop for ForwardProcessGroup {
    fn drop(&mut self) {
        self.kill_group();
    }
}

async fn ensure_auth_forward(
    executable: PathBuf,
    provider: DeviceEnvironment,
    provider_id: String,
    ports: Vec<(u16, u16)>,
    ready_timeout: Duration,
    lease_duration: Duration,
    active: Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    if !provider.valid_devbox_id(&provider_id) { return Err("Invalid Devbox ID".into()); }
    if ports.is_empty() {
        return Ok(());
    }
    let key = provider_key(provider, &provider_id);
    {
        let mut forward = active
            .lock();
        match forward.as_deref() {
            Some(current) if current == key => return Ok(()),
            Some(_) => return Err("Another Devbox port forward is already active".into()),
            None => {}
        }
        if let Some((port, _)) = ports.iter().find(|(port, _)| !local_port_available(*port)) {
            return Err(format!("Devbox forward port {port} is already in use"));
        }
        *forward = Some(key.clone());
    }
    let lease = AuthForwardLease {
        active: active.clone(),
        provider_id: key,
    };
    let mapping_size = if provider == DeviceEnvironment::Boat { 1 } else { ports.len() };
    let mut processes = Vec::with_capacity(ports.len() / mapping_size);
    for mapping in ports.chunks(mapping_size) {
        let mut command = Command::new(&executable);
        if provider == DeviceEnvironment::Boat {
            let (local, remote) = mapping[0];
            command.args(["--no-update", "--json", "forward", &provider_id, "--remote", &remote.to_string(), "--local", &local.to_string(), "--bind", "127.0.0.1"]);
        } else {
            command.args(["port-forward", &provider_id, "--ports", &forward_port_spec(mapping)]);
        }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        processes.push(ForwardProcessGroup::spawn(&mut command)
            .map_err(|_| "Could not start Devbox port forward".to_string())?);
    }
    let ready = tokio::time::timeout(ready_timeout, async {
        loop {
            for process in &mut processes {
                if process.has_exited().map_err(|_| "Could not observe Devbox port forward".to_string())? {
                    return Err("Devbox port forward exited early".into());
                }
            }
            let mut ready = true;
            for (local, _) in &ports {
                ready &= tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *local)).await.is_ok();
            }
            if ready { return Ok(()); }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err("Devbox port forward did not become ready within 10 seconds".to_string())
    });
    if let Err(error) = ready {
        for process in &mut processes { process.stop().await; }
        return Err(error);
    }
    let expires_at = tokio::time::Instant::now() + lease_duration;
    tokio::spawn(async move {
        let _lease = lease;
        let _ = tokio::time::timeout_at(expires_at, async {
            loop {
                if processes.iter_mut().any(|process| process.has_exited().unwrap_or(true)) { break; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await;
        for process in &mut processes { process.stop().await; }
    });
    Ok(())
}
fn provider_name(provider: DeviceEnvironment) -> &'static str {
    match provider {
        DeviceEnvironment::Namespace => "namespace",
        DeviceEnvironment::Boat => "boat",
    }
}

fn provider_key(provider: DeviceEnvironment, id: &str) -> String {
    format!("{}:{id}", provider_name(provider))
}

pub(crate) fn bound_devbox_id<'a>(device: &'a Device, local_id: &str) -> Result<(DeviceEnvironment, &'a str), String> {
    if device.id == local_id {
        return Err("Only a remote Devbox can be controlled".into());
    }
    device.devbox_binding().ok_or_else(|| {
        "This device has no valid Devbox binding. Reconfigure its Crew launcher with COMET_DEVBOX_PROVIDER and COMET_DEVBOX_ID and register it again; the display name cannot be used.".into()
    })
}

fn provider_executable(provider: DeviceEnvironment) -> Result<PathBuf, String> {
    let name = match provider {
        DeviceEnvironment::Namespace => "devbox",
        DeviceEnvironment::Boat => "boat",
    };
    cli_executable(name).ok_or_else(|| format!("{name} CLI not found. Install it (including ~/.local/bin/{name}) and run {name} login on this controller."))
}

fn cli_executable(name: &str) -> Option<PathBuf> {
    let executable = if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(&executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin").join(&executable));
    }
    candidates.extend(
        ["/opt/homebrew/bin", "/usr/local/bin"].map(|dir| PathBuf::from(dir).join(&executable)),
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

async fn run_cli(executable: &std::path::Path, args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut command = Command::new(executable);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = ForwardProcessGroup::spawn(&mut command)
        .map_err(|_| "Could not start Devbox CLI".to_string())?;
    let stdout = process.child.stdout.take().expect("piped stdout");
    let stderr = process.child.stderr.take().expect("piped stderr");
    tokio::try_join!(async {
        process.wait_for_exit().await?;
        process.kill_group();
        process.child.wait().await
    }, capture_bounded(stdout), capture_bounded(stderr))
        .map_err(|_| "Could not read Devbox CLI result".to_string())
}

fn boat_wake_action(output: &[u8], id: &str) -> Result<bool, String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|_| "Boat returned an invalid machine status".to_string())?;
    let sandbox = value.get("sandbox").unwrap_or(&value);
    if value.get("ok") == Some(&serde_json::Value::Bool(false)) || sandbox.get("id").and_then(serde_json::Value::as_str) != Some(id) {
        return Err("Boat did not confirm the registered immutable Devbox ID; Crew will not control another machine".into());
    }
    match sandbox.get("state").and_then(serde_json::Value::as_str) {
        Some("stopped" | "archived") => Ok(true),
        Some("ready" | "idle" | "running" | "restoring" | "provisioning" | "provisioned" | "cloning" | "init") => Ok(false),
        Some("archiving" | "stopping") => Err("Boat is still saving this Devbox. Wait until it is archived, then retry Wake Device; Crew will not interrupt its snapshot".into()),
        _ => Err("Boat cannot start this Devbox in its current state. Inspect it with boat info; Crew will not recover, replace, or recreate it".into()),
    }
}

fn boat_starting(output: &[u8]) -> bool {
    let Some(line) = output.split(|byte| *byte == b'\n').rfind(|line| !line.iter().all(u8::is_ascii_whitespace)) else { return false; };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else { return false; };
    if value.get("event").and_then(serde_json::Value::as_str) != Some("error") { return false; }
    match value.get("code").and_then(serde_json::Value::as_str) {
        Some("boat_starting" | "sandbox_starting") => true,
        Some("sandbox_direct_failed") => {
            value.get("error").and_then(serde_json::Value::as_str) == Some("sandbox_restoring") ||
            (value.pointer("/error/details/retryable").and_then(serde_json::Value::as_bool) == Some(true) &&
             value.pointer("/error/details/message").and_then(serde_json::Value::as_str) == Some("sandbox_restoring"))
        }
        _ => false,
    }
}

fn boat_failure(stdout: &[u8], stderr: &[u8]) -> String {
    // Never return provider output: auth URLs, environment values and command logs
    // are private to this controller, including structured JSON error messages.
    let output = format!("{}\n{}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr)).to_ascii_lowercase();
    if ["unauthorized", "unauthenticated", "login", "credential", "permission denied"].iter().any(|word| output.contains(word)) {
        "Boat authentication failed. Run boat login on this controller and verify access to this Devbox.".into()
    } else if ["billing_required", "limit_reached", "rate_limited", "trial_machine_class_not_allowed", "org_suspended"].iter().any(|word| output.contains(word)) {
        "Boat refused this operation because of billing, usage limits, or account permissions. Check the controller's Boat account before retrying.".into()
    } else if ["not_found", "not found", "does not exist"].iter().any(|word| output.contains(word)) && !output.contains("crew-devbox-autostart") {
        "Boat could not find this Devbox. Verify its registered immutable ID; Crew will not create a replacement.".into()
    } else {
        "Boat could not start Crew. Check boat info and the existing ~/.local/bin/crew-devbox-autostart helper; Crew will not power-cycle or replace this machine.".into()
    }
}

pub(crate) async fn run_boat_extend(id: &str, cancel: &tokio_util::sync::CancellationToken) -> Result<(), String> {
    if !DeviceEnvironment::Boat.valid_devbox_id(id) { return Err("Invalid Boat Devbox ID".into()); }
    let executable = provider_executable(DeviceEnvironment::Boat)?;
    let args = ["--no-update", "--json", "extend", id, "--ttl", "3600"];
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("Boat lease renewal cancelled".into()),
        result = tokio::time::timeout(Duration::from_secs(30), run_cli(&executable, &args)) => {
            let (status, stdout, stderr) = result.map_err(|_| "Boat lease renewal timed out after 30 seconds".to_string())??;
            if status.success() { Ok(()) } else { Err(boat_failure(&stdout, &stderr)) }
        }
    }
}

async fn run_bootstrap(
    executable: PathBuf,
    provider: DeviceEnvironment,
    provider_id: &str,
    command: &'static str,
) -> Result<(), String> {
    tokio::time::timeout(BOOT_TIMEOUT, async {
        if !provider.valid_devbox_id(provider_id) { return Err("Invalid Devbox ID".into()); }
        if provider == DeviceEnvironment::Boat {
            let (status, stdout, stderr) = run_cli(&executable, &["--no-update", "--json", "info", provider_id]).await?;
            if !status.success() { return Err(boat_failure(&stdout, &stderr)); }
            if boat_wake_action(&stdout, provider_id)? {
                let (status, stdout, stderr) = run_cli(&executable, &["--no-update", "--json", "resume", provider_id, "--ttl", "3600"]).await?;
                if !status.success() { return Err(boat_failure(&stdout, &stderr)); }
            }
            loop {
                let (status, stdout, stderr) = run_cli(&executable, &["--no-update", "--json", "exec", provider_id, command]).await?;
                if status.success() { return Ok(()); }
                if !boat_starting(&stdout) { return Err(boat_failure(&stdout, &stderr)); }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        let (status, stdout, stderr) = run_cli(&executable, &["exec", provider_id, "--", "sh", "-c", command]).await?;
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
    }).await.map_err(|_| "Devbox startup timed out after 110 seconds. Check provider login and this machine's status, then retry; Crew will not create a replacement.".to_string())?
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

    // ponytail: serialize OS listener fixtures; inherit bound sockets if parallel startup matters.
    static LISTENER_FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn boat_state_requires_exact_identity_and_known_lifecycle() {
        let id = "bx_f7k2q9hd";
        for state in ["ready", "idle", "running", "restoring", "provisioning", "provisioned", "cloning"] {
            let output = serde_json::to_vec(&serde_json::json!({"ok": true, "sandbox": {"id": id, "state": state}})).unwrap();
            assert!(!boat_wake_action(&output, id).unwrap());
        }
        for state in ["stopped", "archived"] {
            let output = serde_json::to_vec(&serde_json::json!({"ok": true, "sandbox": {"id": id, "state": state}})).unwrap();
            assert!(boat_wake_action(&output, id).unwrap());
        }
        for state in ["archiving", "stopping", "error", "deleted", "unknown"] {
            let output = serde_json::to_vec(&serde_json::json!({"ok": true, "sandbox": {"id": id, "state": state}})).unwrap();
            assert!(boat_wake_action(&output, id).is_err());
        }
        for output in [br#"{"id":"bx_22222222","state":"archived"}"#.as_slice(), br#"{"state":"ready"}"#, b"not json"] {
            assert!(boat_wake_action(output, id).is_err());
        }
    }

    #[test]
    fn boat_exec_retries_only_when_command_did_not_run() {
        assert!(boat_starting(br#"{"event":"error","code":"sandbox_starting"}"#));
        assert!(boat_starting(br#"{"event":"error","code":"sandbox_direct_failed","error":"sandbox_restoring"}"#));
        assert!(boat_starting(br#"{"event":"error","code":"sandbox_direct_failed","error":{"details":{"retryable":true,"message":"sandbox_restoring"}}}"#));
        assert!(!boat_starting(b"{\"event\":\"error\",\"code\":\"sandbox_starting\"}\n{\"event\":\"error\",\"code\":\"unauthorized\"}\n"));
        assert!(!boat_starting(br#"{"event":"error","code":"sandbox_direct_failed","error":{"details":{"retryable":false,"message":"sandbox_restoring"}}}"#));
        for output in [br#"{"event":"error","code":"sandbox_direct_failed","error":"command timed out"}"#.as_slice(), br#"{"event":"error","code":"unauthorized","error":"sandbox_starting https://secret"}"#, b"sandbox_starting"] {
            assert!(!boat_starting(output));
        }
    }

    #[tokio::test]
    async fn provider_keys_do_not_coalesce_same_opaque_id() {
        let wake = DeviceWake::default();
        let namespace = provider_key(DeviceEnvironment::Namespace, "same");
        let boat = provider_key(DeviceEnvironment::Boat, "same");
        let first = wake.coalesce(&namespace, async { Err("namespace".into()) }.boxed());
        let second = wake.coalesce(&boat, async { Err("boat".into()) }.boxed());
        assert_eq!(first.await, Err("namespace".into()));
        assert_eq!(second.await, Err("boat".into()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn last_boot_waiter_cancellation_kills_descendant_listener() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let port = unused_port();
        let temp = tempfile::tempdir().unwrap();
        let executable = write_forward_executable(temp.path(), &format!(
            "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{port}' '{}' --exact device_wake::tests::auth_forward_child --nocapture &\nwait\n",
            std::env::current_exe().unwrap().display()
        ));
        let wake = DeviceWake::default();
        let mut first = wake.coalesce("boat:boot", async move {
            run_cli(&executable, &[]).await.map(|_| ())
        }.boxed());
        let second = wake.coalesce("boat:boot", async { panic!("duplicate boot") }.boxed());
        let ports = [port];
        tokio::select! {
            result = &mut first => panic!("boot exited before cancellation: {result:?}"),
            _ = wait_for_listener_ports(&ports, true) => {}
        }
        drop(first);
        wait_for_listener_ports(&[port], true).await;
        drop(second);
        wait_for_listener_ports(&[port], false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn boat_forward_mappings_are_separate_and_all_expire() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let reserved = [
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
        ];
        let ports = reserved.each_ref().map(|listener| listener.local_addr().unwrap().port());
        drop(reserved);
        let temp = tempfile::tempdir().unwrap();
        // Each invocation holds only its own --local listener. A combined-mapping
        // invocation cannot satisfy both readiness checks.
        let executable = write_forward_executable(temp.path(), &format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n if [ \"$1\" = --local ]; then shift; port=$1; fi\n shift\ndone\nCOMET_TEST_AUTH_FORWARD_PORT=\"$port\" exec '{}' --exact device_wake::tests::auth_forward_child --nocapture\n",
            std::env::current_exe().unwrap().display()
        ));
        let active = Arc::new(Mutex::new(None));
        ensure_auth_forward(executable, DeviceEnvironment::Boat, "bx_f7k2q9hd".into(),
            vec![(ports[0], 3000), (ports[1], 10_350)], Duration::from_secs(2), AUTH_FORWARD_TIMEOUT, active.clone()).await.unwrap();
        wait_for_listener_ports(&ports, true).await;
        assert_eq!(active.lock().as_deref(), Some("boat:bx_f7k2q9hd"));
        tokio::time::pause();
        tokio::time::advance(AUTH_FORWARD_TIMEOUT).await;
        tokio::time::resume();
        wait_for_listener_ports(&ports, false).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if active.lock().is_none() { return; }
                tokio::task::yield_now().await;
            }
        }).await.expect("forward reservation was not released");
    }

    #[tokio::test]
    async fn occupied_boat_auth_port_fails_closed_without_provider_cli() {
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let active = Arc::new(Mutex::new(None));
        let error = ensure_auth_forward(PathBuf::from("/missing/boat"), DeviceEnvironment::Boat,
            "bx_f7k2q9hd".into(), vec![(occupied.local_addr().unwrap().port(), 8085)],
            Duration::from_secs(1), AUTH_FORWARD_TIMEOUT, active.clone()).await.unwrap_err();
        assert!(error.contains("already in use"));
        assert!(active.lock().is_none());
    }

    #[cfg(unix)]
    async fn wait_for_listener_ports(ports: &[u16], active: bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut ready = true;
                for port in ports {
                    ready &= tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port)).await.is_ok() == active;
                }
                if ready { return; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("listener lifecycle did not converge");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn boat_wake_resumes_only_archived_machine_and_checks_existing_helper() {
        for state in ["running", "archived", "archiving", "error"] {
            let temp = tempfile::tempdir().unwrap();
            let resumed = temp.path().join("resumed");
            let helper = temp.path().join("helper");
            let info = serde_json::json!({"ok":true,"sandbox":{"id":"bx_f7k2q9hd","state":state}});
            let executable = write_forward_executable(temp.path(), &format!(
                "#!/bin/sh\ncase \"$3\" in\ninfo) printf '%s' '{info}';;\nresume) [ '{state}' = archived ] || exit 91; touch '{}';;\nexec) if [ '{state}' = archived ]; then [ -f '{}' ] || exit 92; fi; touch '{}';;\n*) exit 93;;\nesac\n",
                resumed.display(), resumed.display(), helper.display()
            ));
            let result = run_bootstrap(executable, DeviceEnvironment::Boat, "bx_f7k2q9hd",
                bootstrap_command("ashler-staging").unwrap()).await;
            assert_eq!(result.is_ok(), matches!(state, "running" | "archived"));
            assert_eq!(resumed.exists(), state == "archived");
            assert_eq!(helper.exists(), matches!(state, "running" | "archived"));
        }
    }

    fn unused_port() -> u16 {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[cfg(unix)]
    fn write_forward_executable(directory: &std::path::Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("devbox");
        std::fs::write(&executable, script).unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        executable
    }

    #[cfg(unix)]
    async fn wait_for_callback_state(
        wake: &DeviceWake,
        provider_id: &str,
        ports: &[u16],
        active: bool,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut matches = wake.auth_forward_active(DeviceEnvironment::Namespace, provider_id) == active;
                for port in ports {
                    matches &= tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port))
                        .await
                        .is_ok()
                        == active;
                }
                if matches {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("callback lease state and listener reachability did not converge");
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
    fn wake_requires_bound_remote_provider_id() {
        let mut device: Device = serde_json::from_value(serde_json::json!({
            "id": "remote", "name": "Owner Devbox", "platform": "linux", "lastSeenAt": null
        }))
        .unwrap();
        device.devbox_id = Some("ofpf7g22n4412".into());
        assert!(bound_devbox_id(&device, "local").is_err());
        device.devbox_provider = Some(DeviceEnvironment::Namespace);
        assert_eq!(bound_devbox_id(&device, "local").unwrap(), (DeviceEnvironment::Namespace, "ofpf7g22n4412"));
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
            device.devbox_id = invalid.map(str::to_owned);
            assert!(bound_devbox_id(&device, "local").is_err());
        }
        device.devbox_provider = Some(DeviceEnvironment::Boat);
        device.devbox_id = Some("bx_f7k2q9hd".into());
        assert_eq!(bound_devbox_id(&device, "local").unwrap(), (DeviceEnvironment::Boat, "bx_f7k2q9hd"));
        assert!(bound_devbox_id(&device, "remote").is_err());
        device.devbox_id = Some("ofpf7g22n4412".into());
        assert!(bound_devbox_id(&device, "local").is_err());

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


    #[tokio::test]
    async fn failed_callback_tunnel_can_retry() {
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(unused_port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(!wake.auth_forward_active(DeviceEnvironment::Namespace, provider_id));
    }

    #[tokio::test]
    async fn occupied_ports_and_other_devices_fail_without_spawning() {
        let wake = DeviceWake::default();
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            DeviceEnvironment::Namespace,
            "ofpf7g22n4412".into(),
            vec![(occupied.local_addr().unwrap().port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.unwrap_err().contains("already in use"));
        assert!(!wake.auth_forward_active(DeviceEnvironment::Namespace, "ofpf7g22n4412"));

        *wake.auth_forward.lock() = Some("another-device".into());
        let result = ensure_auth_forward(
            PathBuf::from("/definitely/missing/devbox"),
            DeviceEnvironment::Namespace,
            "ofpf7g22n4412".into(),
            vec![(unused_port(), 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.unwrap_err().contains("Another Devbox"));
        assert_eq!(
            wake.auth_forward.lock().as_deref(),
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
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(port, 8085)],
            Duration::from_millis(100),
            Duration::from_millis(100),
            wake.auth_forward.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(!wake.auth_forward_active(DeviceEnvironment::Namespace, provider_id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_readiness_releases_callback_lease() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let callback_port = unused_port();
        let temp = tempfile::tempdir().unwrap();
        let executable = write_forward_executable(
            temp.path(),
            &format!(
                "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{callback_port}' '{}' --exact device_wake::tests::auth_forward_child --nocapture &\nwait\n",
                std::env::current_exe().unwrap().display()
            ),
        );
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        let mut first = wake.coalesce_forward(
            provider_id,
            ensure_auth_forward(
                executable,
                DeviceEnvironment::Namespace,
                provider_id.into(),
                // Keep readiness pending while a descendant holds a listener.
                vec![(0, 8085)],
                Duration::from_secs(60),
                Duration::from_secs(60),
                wake.auth_forward.clone(),
            )
            .boxed(),
        );
        let second = wake.coalesce_forward(
            provider_id,
            async { panic!("duplicate forward") }.boxed(),
        );
        let callback_ports = [callback_port];
        tokio::select! {
            result = &mut first => panic!("tunnel exited before cancellation: {result:?}"),
            _ = wait_for_callback_state(&wake, provider_id, &callback_ports, true) => {}
        }
        drop(first);
        wait_for_callback_state(&wake, provider_id, &[callback_port], true).await;
        drop(second);
        wait_for_callback_state(&wake, provider_id, &[callback_port], false).await;
    }

    #[cfg(unix)]
    #[test]
    fn auth_forward_child() {
        let Ok(raw) = std::env::var("COMET_TEST_AUTH_FORWARD_PORT") else {
            return;
        };
        let _callbacks: Vec<_> = raw
            .split(',')
            .map(|port| {
                std::net::TcpListener::bind((
                    std::net::Ipv4Addr::LOCALHOST,
                    port.parse::<u16>().unwrap(),
                ))
                .unwrap()
            })
            .collect();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn callback_tunnel_becomes_ready_and_is_coalesced() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let callback_port = unused_port();
        let temp = tempfile::tempdir().unwrap();
        let executable = write_forward_executable(
            temp.path(),
            &format!(
                "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{callback_port}' exec '{}' --exact device_wake::tests::auth_forward_child --nocapture\n",
                std::env::current_exe().unwrap().display()
            ),
        );

        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        ensure_auth_forward(
            executable.clone(),
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(callback_port, 8085)],
            Duration::from_secs(2),
            AUTH_FORWARD_TIMEOUT,
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        // Real subprocess startup cannot use paused time; control only expiry.
        tokio::time::pause();
        assert!(wake.auth_forward_active(DeviceEnvironment::Namespace, provider_id));
        ensure_auth_forward(
            executable,
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(callback_port, 8085)],
            Duration::from_secs(2),
            AUTH_FORWARD_TIMEOUT,
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        tokio::time::advance(AUTH_FORWARD_TIMEOUT).await;
        tokio::time::resume();
        wait_for_callback_state(&wake, provider_id, &[callback_port], false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn callback_tunnel_lease_kills_spawned_process_group() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let reserved = [
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap(),
        ];
        let ports = reserved
            .each_ref()
            .map(|listener| listener.local_addr().unwrap().port());
        drop(reserved);
        let temp = tempfile::tempdir().unwrap();
        let executable = write_forward_executable(
            temp.path(),
            &format!(
                "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{},{}' '{}' --exact device_wake::tests::auth_forward_child --nocapture &\nwait\n",
                ports[0], ports[1], std::env::current_exe().unwrap().display()
            ),
        );
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        ensure_auth_forward(
            executable,
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(ports[0], 8085), (ports[1], 8086)],
            Duration::from_secs(2),
            AUTH_FORWARD_TIMEOUT,
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        wait_for_callback_state(&wake, provider_id, &ports, true).await;
        tokio::time::pause();
        tokio::time::advance(AUTH_FORWARD_TIMEOUT).await;
        tokio::time::resume();
        wait_for_callback_state(&wake, provider_id, &ports, false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn callback_tunnel_leader_exit_kills_spawned_process_group() {
        let _fixture = LISTENER_FIXTURES.lock().await;
        let callback_port = unused_port();
        let temp = tempfile::tempdir().unwrap();
        let exit_marker = temp.path().join("exit");
        let executable = write_forward_executable(
            temp.path(),
            &format!(
                "#!/bin/sh\nCOMET_TEST_AUTH_FORWARD_PORT='{callback_port}' '{}' --exact device_wake::tests::auth_forward_child --nocapture &\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
                std::env::current_exe().unwrap().display(), exit_marker.display()
            ),
        );
        let wake = DeviceWake::default();
        let provider_id = "ofpf7g22n4412";
        ensure_auth_forward(
            executable,
            DeviceEnvironment::Namespace,
            provider_id.into(),
            vec![(callback_port, 8085)],
            Duration::from_secs(2),
            AUTH_FORWARD_TIMEOUT,
            wake.auth_forward.clone(),
        )
        .await
        .unwrap();
        std::fs::write(exit_marker, b"").unwrap();
        wait_for_callback_state(&wake, provider_id, &[callback_port], false).await;
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
