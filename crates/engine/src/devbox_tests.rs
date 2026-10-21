use super::*;

const BOX: &str = "bx_23456789";

// Failure modes: a snapshotted/forked identity can control the wrong VM, an
// unknown provider state is treated as recoverable, or a peer's readiness is
// accepted without proving the registered execution identity.
#[test]
fn host_binding_requires_current_native_identity() {
    assert_eq!(
        resolve_host_binding(true, false, Some(BOX), Some("boat"), Some(BOX)),
        Ok(Some((DeviceEnvironment::Boat, BOX.into())))
    );
    assert_eq!(
        resolve_host_binding(true, true, None, Some("namespace"), Some("abcdefgh23456")),
        Ok(Some((DeviceEnvironment::Namespace, "abcdefgh23456".into())))
    );
    assert_eq!(
        resolve_host_binding(false, false, None, None, None),
        Ok(None)
    );
    for result in [
        resolve_host_binding(true, false, Some(BOX), None, None),
        resolve_host_binding(true, false, None, Some("boat"), Some(BOX)),
        resolve_host_binding(false, false, Some(BOX), Some("boat"), Some(BOX)),
        resolve_host_binding(true, false, Some("bx_abcdefgh"), Some("boat"), Some(BOX)),
        resolve_host_binding(true, false, Some(BOX), Some("boat"), Some("../../other")),
        resolve_host_binding(true, false, None, None, Some(BOX)),
        resolve_host_binding(true, true, None, None, None),
        resolve_host_binding(true, false, None, Some("namespace"), Some("abcdefgh23456")),
        resolve_host_binding(
            true,
            true,
            Some(BOX),
            Some("namespace"),
            Some("abcdefgh23456"),
        ),
        resolve_host_binding(true, true, Some(BOX), Some("boat"), Some(BOX)),
    ] {
        assert!(result.is_err());
    }
}

#[test]
fn lifecycle_selection_never_recovers_or_replaces_a_machine() {
    for state in ["archived", "stopped"] {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"sandbox":{"id":BOX,"state":state}})).unwrap();
        assert_eq!(wake_action(&bytes, BOX), Ok(WakeAction::Resume));
    }
    for state in [
        "ready",
        "running",
        "idle",
        "provisioning",
        "provisioned",
        "restoring",
        "cloning",
    ] {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"sandbox":{"id":BOX,"state":state}})).unwrap();
        assert_eq!(wake_action(&bytes, BOX), Ok(WakeAction::Connect));
    }
    for value in [
        serde_json::json!({"sandbox":{"id":"bx_abcdefgh","state":"archived"}}),
        serde_json::json!({"sandbox":{"id":BOX,"state":"archiving"}}),
        serde_json::json!({"sandbox":{"id":BOX,"state":"error"}}),
        serde_json::json!({"sandbox":{"id":BOX,"state":"unknown"}}),
        serde_json::json!({"ok":false,"sandbox":{"id":BOX,"state":"ready"}}),
    ] {
        assert!(wake_action(&serde_json::to_vec(&value).unwrap(), BOX).is_err());
    }
    assert!(wake_action(b"not JSON", BOX).is_err());
}

#[test]
fn ready_peer_must_match_exact_provider_device_and_synced_scope() {
    for (provider, id) in [
        (DeviceEnvironment::Boat, BOX),
        (DeviceEnvironment::Namespace, "abcdefgh23456"),
    ] {
        let ready = serde_json::json!({"deviceId":"peer", "devboxProvider":provider, "devboxId":id,
            "workspaceScope":"synced", "capabilities":["native"]});
        assert!(verify_peer(&ready, "peer", provider, id).is_ok());
        for (field, value) in [
            ("deviceId", serde_json::json!("other")),
            ("devboxProvider", serde_json::Value::Null),
            ("devboxId", serde_json::json!("other")),
            ("workspaceScope", serde_json::json!("local")),
            ("capabilities", serde_json::json!([42])),
        ] {
            let mut wrong = ready.clone();
            wrong[field] = value;
            assert!(verify_peer(&wrong, "peer", provider, id).is_err());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_cli_capture_surfaces_excess_output_without_retaining_it() {
    let stop = CancellationToken::new();
    let result = run_cli(
        Path::new("/bin/sh"),
        &["-c", "dd if=/dev/zero bs=1024 count=80 2>/dev/null"],
        Duration::from_secs(5),
        &stop,
    )
    .await;
    assert!(matches!(result, Err(error) if error.contains("output exceeded")));
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_cli_cannot_leave_its_child_running() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("owned-child");
    let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    let pid_path = pid_file.clone();
    let watcher = tokio::spawn(async move {
        // Wait on the fixture's own process start, not a provider/entity poll.
        for _ in 0..100 {
            if pid_path.exists() {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fixture child did not start");
    });
    let result = run_cli(
        Path::new("/bin/sh"),
        &["-c", &script],
        Duration::from_secs(5),
        &stop,
    )
    .await;
    watcher.await.unwrap();
    assert!(matches!(result, Err(error) if error.contains("cancelled")));
    let child: libc::pid_t = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Killed descendants can briefly remain as zombies; no live workload may survive.
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &child.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "child remains live: {state}"
    );
}
