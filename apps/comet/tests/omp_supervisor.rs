#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn wait_for_file(path: &Path, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && let Ok(pid) = value.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 performs existence/permission validation only.
    (unsafe { libc::kill(pid as i32, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[test]
fn supervisor_reaps_omp_group_when_engine_parent_dies() {
    let temp = tempfile::tempdir().unwrap();
    let parent_script = temp.path().join("parent.sh");
    let omp_script = temp.path().join("fake-omp.sh");
    let supervisor_pid_file = temp.path().join("supervisor.pid");
    let omp_pid_file = temp.path().join("omp.pid");
    std::fs::write(
        &parent_script,
        "#!/bin/sh\n\"$1\" __comet-omp-supervisor \"$$\" \"$2\" &\necho $! > \"$3\"\nwait\n",
    )
    .unwrap();
    std::fs::write(
        &omp_script,
        "#!/bin/sh\necho $$ > \"$COMET_TEST_OMP_PID_FILE\"\ntrap '' TERM\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&parent_script, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&omp_script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = Command::new("/bin/sh");
    command
        .arg(&parent_script)
        .arg(env!("CARGO_BIN_EXE_comet"))
        .arg(&omp_script)
        .arg(&supervisor_pid_file)
        .env("COMET_TEST_OMP_PID_FILE", &omp_pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut parent = command.spawn().unwrap();
    let parent_group = parent.id() as i32;
    let supervisor_pid = wait_for_file(&supervisor_pid_file, Duration::from_secs(5));
    let omp_pid = wait_for_file(&omp_pid_file, Duration::from_secs(5));
    assert!(process_exists(supervisor_pid));
    assert!(process_exists(omp_pid));

    parent.kill().unwrap();
    let _ = parent.wait();
    let deadline = Instant::now() + Duration::from_secs(8);
    while (process_exists(supervisor_pid) || process_exists(omp_pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let supervisor_alive = process_exists(supervisor_pid);
    let omp_alive = process_exists(omp_pid);
    if supervisor_alive || omp_alive {
        // SAFETY: the test created this isolated process group.
        unsafe {
            libc::kill(-parent_group, libc::SIGKILL);
        }
    }
    assert!(!supervisor_alive, "supervisor survived its engine parent");
    assert!(!omp_alive, "OMP survived its engine parent");
}
