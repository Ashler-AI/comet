#[test]
fn browser_suppression_exits_without_starting_the_app() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_zeron"))
        .args([
            "--noop-browser",
            "https://accounts.google.com/o/oauth2/auth?client_id=fake&state=test",
        ])
        .output()
        .expect("suppression process");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn session_read_uses_inherited_ipc_without_rewriting_saved_runtime() {
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let profile = tempfile::tempdir().unwrap();
    let settings = profile.path().join("crew-runtime.json");
    let saved_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let saved = serde_json::to_vec(&serde_json::json!({
        "edgeUrl": "https://edge.example.test", "scaffoldUrl": "https://scaffold.example.test",
        "projectScope": "test", "deploymentId": "canary",
        "ipcPort": saved_listener.local_addr().unwrap().port(),
    })).unwrap();
    std::fs::write(&settings, &saved).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let inherited_port = listener.local_addr().unwrap().port();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (connection, _) = listener.accept().unwrap();
        // Close before the WebSocket upgrade: observe routing without admitting a command.
        drop(connection);
        sent.send(()).unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeron"))
        .env_clear()
        .env("COMET_IPC_PORT", inherited_port.to_string())
        .arg("--data-dir").arg(profile.path())
        .args(["session", "read", "95e5f344-d332-445d-b91e-dd2e691153f6"])
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    if let Err(error) = received.recv_timeout(Duration::from_secs(10)) {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!("session read did not reach the inherited IPC port: {error}; {output:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("session read did not exit after IPC closed: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    drop(saved_listener);
    assert!(!output.status.success(), "the probe does not admit RPC commands");
    assert_eq!(std::fs::read(settings).unwrap(), saved);
}
