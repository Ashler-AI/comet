//! Crew: the sandbox host end to end (docs/crew/zgx-thin-port.md, "Scaffold
//! sandboxes"). The `zeron` binary (shipped as `comet`) is started exactly as
//! Platform's native runner starts it (native-crew-runner.mjs), against a
//! Scaffold authority and Edge on one loopback origin:
//!
//! - the grant is exchanged, the bootstrap removed, and the durable
//!   credential is the one the runner's restart path checks;
//! - the engine is the grant's device, and `scaffold-authority` (the
//!   desktop's attach probe, with its workspace-relative data dir) prints
//!   Crew's line;
//! - what a host refuses, it refuses;
//! - SIGTERM ends it with 0 well within the runner's 30 s;
//! - the runner's restart (no bootstrap) refreshes the credential and serves
//!   the same authority;
//! - a refused grant ends it with the words the runner looks for.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const GRANT_ID: &str = "0123456789abcdef0123456789abcdef";
const REFUSED_GRANT_ID: &str = "ffffffffffffffffffffffffffffffff";
const DEVICE: &str = "comet-scaffold-sbx-1-e3";
const CAPABILITIES: [&str; 7] = [
    "session.read",
    "session.sync.write",
    "session.chat",
    "session.control",
    "session.annotate",
    "session.files",
    "session.environment",
];

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The authority and its Edge: device grant exchange and refresh, counted;
/// everything else (rooms, releases) a 404.
struct FakeScaffold {
    origin: String,
    exchanges: Arc<AtomicUsize>,
    refreshes: Arc<AtomicUsize>,
    expires_at: i64,
    _runtime: tokio::runtime::Runtime,
}

impl FakeScaffold {
    fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let expires_at = chrono_now_ms() + 12 * 3_600_000;
        let (exchanges, refreshes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (seen_exchanges, seen_refreshes) = (exchanges.clone(), refreshes.clone());
        runtime.spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let (exchanges, refreshes) = (seen_exchanges.clone(), seen_refreshes.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 8192];
                    loop {
                        let Ok(read) = stream.read(&mut buffer).await else { return };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&request).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned()))
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    let device = |secret: String| {
                        serde_json::json!({
                            "accessToken": format!("cs1.{GRANT_ID}.{secret}"), "expiresAt": expires_at,
                            "userId": "owner@example.com", "email": "owner@example.com",
                            "projectId": "project-1", "deploymentId": "deployment-1", "sandboxId": "sbx-1",
                            "targetDeviceId": DEVICE, "sessionId": "session-1", "lifecycleEpoch": 3,
                            "capabilities": CAPABILITIES,
                        })
                    };
                    let (status, body) = if text.starts_with("POST /auth/device-grants/exchange ") {
                        exchanges.fetch_add(1, Ordering::SeqCst);
                        if text.contains(REFUSED_GRANT_ID) {
                            ("401 Unauthorized", serde_json::json!({"error": "grant_consumed"}))
                        } else {
                            ("200 OK", device("enrolled".into()))
                        }
                    } else if text.starts_with("POST /auth/device-token/refresh ") {
                        let count = refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                        if text.contains(&format!("Bearer cs1.{GRANT_ID}.")) {
                            ("200 OK", device(format!("rotated-{count}")))
                        } else {
                            ("401 Unauthorized", serde_json::json!({"error": "unknown_device"}))
                        }
                    } else {
                        ("404 Not Found", serde_json::json!({}))
                    };
                    let body = body.to_string();
                    let _ = stream
                        .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                        .await;
                });
            }
        });
        Self { origin, exchanges, refreshes, expires_at, _runtime: runtime }
    }
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// A host process, killed if the test ends while it runs.
struct Host {
    child: Child,
    log: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Host {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// SIGTERM, as the runner's quiesce sends it: the exit code and how long.
    fn terminate(&mut self) -> (Option<i32>, Duration) {
        let started = Instant::now();
        // SAFETY: a plain signal to our own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status.code(), started.elapsed());
            }
            if started.elapsed() > Duration::from_secs(60) {
                panic!("still running a minute after SIGTERM:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The environment the runner gives it (its own `COMET_*`, the rest of a
/// clean one), in a home of its own.
fn runner_command(root: &Path, scaffold: &FakeScaffold, data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zeron"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("TMPDIR", std::env::temp_dir())
        .env("ZERON_DISABLE_NOTIFICATIONS", "1")
        .env("ZERON_AUTO_UPDATE", "0")
        .env("COMET_DATA_DIR", data)
        .env("COMET_EDGE_URL", &scaffold.origin)
        .env("COMET_SCAFFOLD_URL", &scaffold.origin)
        .current_dir(root);
    command
}

/// native-crew-runner.mjs's argv: `launchArgs`, the bootstrap at index 5.
fn runner_args(scaffold: &FakeScaffold, data: &Path, port: u16, bootstrap: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = ["--data-dir", data.to_str().unwrap(), "--ipc-port", &port.to_string(), "headless",
        "--edge-url", &scaffold.origin, "--scaffold-url", &scaffold.origin,
        "--project-scope", "project-1", "--deployment-id", "deployment-1"]
        .map(String::from)
        .to_vec();
    if let Some(bootstrap) = bootstrap {
        args.splice(5..5, ["--device-bootstrap-file".to_owned(), bootstrap.to_str().unwrap().to_owned()]);
    }
    args
}

fn launch(root: &Path, scaffold: &FakeScaffold, data: &Path, port: u16, bootstrap: Option<&Path>, name: &str) -> Host {
    let log = root.join(format!("{name}.log"));
    let out = std::fs::File::create(&log).unwrap();
    let child = runner_command(root, scaffold, data)
        .args(runner_args(scaffold, data, port, bootstrap))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone().unwrap()))
        .stderr(Stdio::from(out))
        .spawn()
        .expect("start the host");
    Host { child, log }
}

fn write_bootstrap(root: &Path, grant_id: &str) -> PathBuf {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = root.join("runtime/native-crew");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("enrollment-{grant_id}.json"));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
    file.write_all(
        serde_json::json!({
            "deviceJoinGrant": format!("cg1.{grant_id}.enrollment-secret"), "projectId": "project-1",
            "deploymentId": "deployment-1", "sessionId": "session-1", "deviceId": DEVICE,
            "lifecycleEpoch": 3, "sandboxId": "sbx-1",
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    path
}

/// Wait for its IPC, as the desktop's probe would.
fn served(runtime: &tokio::runtime::Runtime, host: &mut Host, data: &Path, port: u16) -> zeron_rpc::RpcClient {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            panic!("the host exited ({status}):\n{}", host.log());
        }
        if let Ok(client) = runtime.block_on(zeron_rpc::connect_ipc(port, Some(data))) {
            return client;
        }
        assert!(Instant::now() < deadline, "never served its port:\n{}", host.log());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The desktop's attach probe: `.scaffold/crew/bin/comet --data-dir
/// .scaffold/crew-data --ipc-port 39400 scaffold-authority`, from the
/// workspace, its data dir relative.
fn probe(root: &Path, scaffold: &FakeScaffold, relative_data: &str, port: u16) -> (Option<i32>, String, String) {
    let output = runner_command(root, scaffold, &root.join(relative_data))
        .args(["--data-dir", relative_data, "--ipc-port", &port.to_string(), "scaffold-authority"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_native_runner_s_host_enrolls_answers_the_probe_stops_and_restarts() {
    let scaffold = FakeScaffold::start();
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let data = root.join(".scaffold/crew-data");
    let port = free_port();
    let bootstrap = write_bootstrap(&root, GRANT_ID);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut host = launch(&root, &scaffold, &data, port, Some(&bootstrap), "enrolled");
    let client = served(&runtime, &mut host, &data, port);
    assert_eq!(scaffold.exchanges.load(Ordering::SeqCst), 1);
    assert!(!bootstrap.exists(), "the bootstrap is consumed once its grant is verified");

    // The restart path's credential (validateDeviceCredential).
    let credential_path = data.join("scaffold-device-session.json");
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(&credential_path).unwrap();
        assert!(metadata.is_file() && metadata.nlink() == 1 && metadata.permissions().mode() & 0o777 == 0o600);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
    let credential: serde_json::Value = serde_json::from_slice(&std::fs::read(&credential_path).unwrap()).unwrap();
    assert_eq!(credential["accessToken"], format!("cs1.{GRANT_ID}.enrolled"));
    assert_eq!(credential["device"]["grantId"], GRANT_ID);
    assert_eq!(credential["identity"]["userId"], "owner@example.com");
    assert_eq!(credential["identity"]["projectScope"], "project-1");
    assert_eq!(credential["identity"]["deploymentId"], "deployment-1");
    assert_eq!((credential["resource"].as_str(), credential["edgeOrigin"].as_str()), (Some(scaffold.origin.as_str()), Some(scaffold.origin.as_str())));
    assert_eq!(credential["device"]["deviceId"], DEVICE);
    assert_eq!(credential["device"]["lifecycleEpoch"], 3);

    // The grant's device, and Crew's probe line.
    let info = runtime.block_on(client.call(zeron_rpc::methods::ENGINE_INFO, serde_json::json!({}))).unwrap();
    assert_eq!(info["deviceId"], DEVICE);
    let expected = format!(
        r#"{{"grantId":"{GRANT_ID}","expiresAt":{},"principalSubject":"owner@example.com","scope":{{"projectId":"project-1","deploymentId":"deployment-1","sessionId":"session-1"}},"sandboxId":"sbx-1","deviceId":"{DEVICE}","lifecycleEpoch":3,"capabilities":{}}}"#,
        scaffold.expires_at,
        serde_json::to_string(&CAPABILITIES).unwrap()
    );
    let (code, stdout, stderr) = probe(&root, &scaffold, ".scaffold/crew-data", port);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, format!("{expected}\n"), "one line, Crew's, and nothing else on stdout");

    // What a host refuses.
    let call = |method: &str, params: serde_json::Value| runtime.block_on(client.call(method, params)).unwrap_err().to_string();
    assert!(call(zeron_rpc::methods::LIST_AGENT_ACCOUNTS, serde_json::json!({})).contains("cannot manage local credentials"));
    assert!(call(zeron_rpc::methods::UPLOAD_COMMIT, serde_json::json!({"chatId": "other", "uploadId": "u", "fileName": "f"}))
        .contains("bound to a different session"));

    // SIGTERM: exit 0, well within the runner's 30 s.
    drop(client);
    let (code, took) = host.terminate();
    assert_eq!(code, Some(0), "{}", host.log());
    assert!(took < Duration::from_secs(20), "took {took:?}");
    eprintln!("SIGTERM to exit 0: {took:?}");
    let (code, _, _) = probe(&root, &scaffold, ".scaffold/crew-data", port);
    assert_ne!(code, Some(0), "nothing answers for a stopped host");

    // The runner's restart: no bootstrap; the credential is refreshed first.
    let mut host = launch(&root, &scaffold, &data, port, None, "restarted");
    let _client = served(&runtime, &mut host, &data, port);
    assert_eq!(scaffold.exchanges.load(Ordering::SeqCst), 1, "a grant is never exchanged twice");
    assert!(scaffold.refreshes.load(Ordering::SeqCst) >= 1);
    let credential: serde_json::Value = serde_json::from_slice(&std::fs::read(&credential_path).unwrap()).unwrap();
    assert!(credential["accessToken"].as_str().unwrap().starts_with(&format!("cs1.{GRANT_ID}.rotated-")));
    let (code, stdout, stderr) = probe(&root, &scaffold, ".scaffold/crew-data", port);
    assert_eq!((code, stdout), (Some(0), format!("{expected}\n")), "{stderr}");
    let (code, _) = host.terminate();
    assert_eq!(code, Some(0), "{}", host.log());
}

/// A grant Scaffold refuses ends the host at once, with the words the
/// runner's `terminalAuth` looks for, and its bootstrap stays.
#[test]
fn a_refused_grant_ends_the_host_with_the_runner_s_words() {
    let scaffold = FakeScaffold::start();
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let data = root.join(".scaffold/crew-data");
    let bootstrap = write_bootstrap(&root, REFUSED_GRANT_ID);
    let mut host = launch(&root, &scaffold, &data, free_port(), Some(&bootstrap), "refused");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < Duration::from_secs(60), "still running:\n{}", host.log());
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_ne!(status.code(), Some(0));
    let output = host.log();
    // native-crew-runner.mjs `terminalAuth`.
    let terminal = ["Crew sandbox authentication failed", "Crew profile requires a verified identity",
        "headless authentication revoked; stopping synced runtime"];
    assert!(terminal.iter().any(|words| output.contains(words)), "{output}");
    assert!(bootstrap.exists(), "an unverified enrollment is never removed");
    // Nothing usable is left for a restart.
    let leftover = std::fs::read_to_string(data.join("scaffold-device-session.json")).unwrap_or_default();
    assert!(!leftover.contains("cs1."), "{leftover}");
}

/// What the archive's checks run: `comet --help` and `comet headless --help`.
#[test]
fn help_answers_as_install_native_crew_requires() {
    for args in [&["--help"][..], &["headless", "--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_zeron")).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let mut text = String::new();
        output.stdout.as_slice().read_to_string(&mut text).unwrap();
        assert!(text.lines().any(|line| line.contains("--data-dir")), "{text}");
        if args.len() == 2 {
            assert!(text.lines().any(|line| line.contains("--device-bootstrap-file")), "{text}");
        }
    }
}
