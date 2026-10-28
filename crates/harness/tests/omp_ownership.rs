#![cfg(all(target_os = "linux", feature = "native-fixture"))]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};

const FIXTURE: &str = env!("CARGO_BIN_EXE_harness-native-fixture");

struct Holder(Child);
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn hold(path: &Path, mode: &str) -> Holder {
    let mut child = Holder(Command::new(FIXTURE)
        .arg("--ownership-holder").arg(mode).arg(path)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
    let mut ready = String::new();
    BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    child
}

fn states(paths: &[&Path]) -> String {
    let output = Command::new(FIXTURE).arg("--ownership-states").args(paths).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn ownership_ignores_unrelated_non_dumpable_process_but_detects_its_writer() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.jsonl");
    let unrelated = dir.path().join("unrelated");
    std::fs::write(&journal, b"journal\n").unwrap();
    std::fs::write(&unrelated, b"").unwrap();
    let hidden = hold(&unrelated, "writer");
    // Root can inspect non-dumpable processes; ordinary devbox users cannot.
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(std::fs::read_dir(format!("/proc/{}/fd", hidden.0.id())).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied);
    }
    assert_eq!(states(&[&journal, &unrelated]), "IA\nIA\n");
    assert_eq!(states(&[&journal]), "I\nI\n");
    let reader = hold(&journal, "reader");
    assert_eq!(states(&[&journal]), "I\nI\n");
    let writer = hold(&journal, "writer");
    assert_eq!(states(&[&journal]), "A\nA\n");
    drop(writer);
    assert_eq!(states(&[&journal]), "I\nI\n");
    drop(reader);
    // The helper must release its lease before reporting inactivity.
    let _writer = std::fs::OpenOptions::new().append(true)
        .custom_flags(libc::O_NONBLOCK).open(&journal).unwrap();
    assert_eq!(std::fs::read(&journal).unwrap(), b"journal\n");
}

#[test]
fn ownership_invalid_paths_and_exclusive_lease_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.jsonl");
    std::fs::write(&journal, b"journal\n").unwrap();
    let lease = hold(&journal, "lease");
    let missing = dir.path().join("missing");
    assert_eq!(states(&[&journal, &missing, dir.path()]), "UUU\nUUU\n");
    drop(lease);
    assert_eq!(states(&[&journal]), "I\nI\n");
}

#[test]
fn ownership_helper_rejects_permission_denial_and_closes_inherited_fds() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal.jsonl");
    std::fs::write(&journal, b"journal\n").unwrap();
    let raw_path = std::ffi::CString::new(journal.as_os_str().as_encoded_bytes()).unwrap();
    let mut command = Command::new(FIXTURE);
    command.arg("__comet-omp-writer-probe").arg(&journal);
    // Simulate an inherited descriptor without leaving a writer in the parent.
    // open is async-signal-safe and intentionally omits O_CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            if libc::open(raw_path.as_ptr(), libc::O_WRONLY) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"I\n");
    assert!(output.stderr.is_empty());
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(states(&[&journal]), "U\nU\n");
    }
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(states(&[&journal]), "I\nI\n");
}
