"""Run with python3 scripts/test_local_cargo.py; compiles only a tiny offline crate."""
import fcntl
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


RUNNER = Path(__file__).with_name("local-cargo.py")


def check():
    with tempfile.TemporaryDirectory(prefix="crew-cargo-check-") as directory:
        base = Path(directory).resolve()
        root, worktree, home = base / "repo", base / "worktree", base / "home"
        (root / "scripts").mkdir(parents=True)
        (root / "src").mkdir()
        home.mkdir()
        shutil.copy2(RUNNER, root / "scripts/local-cargo.py")
        (root / "Cargo.toml").write_text('[package]\nname = "crew-cargo-check"\nversion = "0.1.0"\nedition = "2021"\n')
        (root / "src/main.rs").write_text('fn main() { println!("main"); if std::env::args().any(|a| a == "-j20") { println!("argument preserved"); } if std::env::args().any(|a| a == "fail") { std::process::exit(23); } if std::env::args().any(|a| a == "wait") { println!("waiting"); std::thread::sleep(std::time::Duration::from_secs(60)); } }\n')
        (root / "build.rs").write_text(r'''
use std::{env, fs, process::Command, thread, time::Duration};
fn main() {
    let base = std::path::PathBuf::from(env::var("CREW_CHECK_DIR").unwrap());
    let sentinel = base.join("compiling");
    let _guard = fs::OpenOptions::new().write(true).create_new(true).open(&sentinel).expect("Cargo commands overlapped");
    let label = env::var("CREW_CHECK_LABEL").unwrap();
    let result = Command::new("python3").args(["-c", "import os; print(os.getpriority(os.PRIO_PROCESS, 0))"]).output().unwrap();
    fs::write(base.join(&label), format!("{} {}", env::var("NUM_JOBS").unwrap(), String::from_utf8(result.stdout).unwrap().trim())).unwrap();
    thread::sleep(Duration::from_millis(800));
    fs::remove_file(sentinel).unwrap();
    println!("cargo:rerun-if-env-changed=CREW_CHECK_LABEL");
}
''')
        env = dict(os.environ, HOME=str(home), CARGO_HOME=os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
                   RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
                   ASHLER_INCREMENTAL_TSC_CHECKS="false", CI="true", COMET_LOCAL_AGENT_RUNTIME="1",
                   CREW_CHECK_DIR=str(base), CREW_CHECK_LABEL="first", CARGO_BUILD_JOBS="40")
        for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"):
            env.pop(key, None)
        env["PATH"] += os.pathsep + str(Path.home() / ".cargo/bin")

        def run(command, cwd=root, **kwargs):
            return subprocess.run(command, cwd=cwd, env=kwargs.pop("env", env), check=True,
                                  text=True, capture_output=True, timeout=30, **kwargs)

        run(["git", "init", "-b", "main"])
        run(["git", "add", "."])
        run(["git", "-c", "user.name=Crew Check", "-c", "user.email=check@example.invalid", "-c", "core.hooksPath=/dev/null", "commit", "-m", "tiny crate"])
        run(["git", "worktree", "add", "-b", "other", str(worktree)])
        (worktree / "src/main.rs").write_text('fn main() { println!("worktree"); }\n')

        def command(repo, *args):
            return [sys.executable, str(repo / "scripts/local-cargo.py"), *args]

        lock_path = home / ".cache/crew/cargo-build.lock"
        lock_path.parent.mkdir(parents=True)
        with lock_path.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            # Read-only resolution must not wait for a compiling command.
            assert run(command(worktree, "--print-target-dir")).stdout.strip() == str(root / "target")
            assert run(command(worktree, "--print-target-dir"), env=dict(env, CI="1", COMET_LOCAL_AGENT_RUNTIME="0")).stdout.strip() == str(worktree / "target")
            assert run(command(worktree, "--print-target-dir"), env=dict(env, CARGO_TARGET_DIR="custom")).stdout.strip() == str(worktree / "custom")
            assert run(command(worktree, "--print-target-dir", "--target-dir=chosen"), env=dict(env, CARGO_TARGET_DIR="ignored")).stdout.strip() == str(worktree / "chosen")
            run(command(worktree, "build", "--offline", "--jobs=3", "--target-dir", "ci-cache"),
                env=dict(env, CI="true", COMET_LOCAL_AGENT_RUNTIME="0", CREW_CHECK_LABEL="ci"))
            assert (base / "ci").read_text().split()[0] == "3"

        first_copy, second_copy = base / "first-bin", base / "second-bin"
        first = subprocess.Popen(command(root, "build", "--offline", "-j20", "--copy-binary", "debug/crew-cargo-check", str(first_copy)), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        second = None
        try:
            deadline = time.monotonic() + 30
            while not (base / "first").exists():
                assert first.poll() is None, first.communicate()
                assert time.monotonic() < deadline, "build script did not start"
                time.sleep(0.02)
            with lock_path.open("a") as probe:
                try:
                    fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    raise AssertionError("Cargo command does not hold the user-wide gate")
            # Different targets bypass native Cargo cache locks, exposing gate regressions.
            second = subprocess.Popen(command(worktree, "build", "--offline", "--jobs=1", "--target-dir", "separate", "--copy-binary", "debug/crew-cargo-check", str(second_copy)), env=dict(env, CREW_CHECK_LABEL="second"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for process in (first, second):
                out, err = process.communicate(timeout=30)
                assert process.returncode == 0, out + err
        finally:
            for process in (first, second):
                if process is not None and process.poll() is None:
                    process.terminate()
                    process.communicate(timeout=10)
        assert (base / "first").read_text().split()[0] == "2"
        assert (base / "second").read_text().split()[0] == "1"
        assert all(int((base / label).read_text().split()[1]) >= 10 for label in ("first", "second"))
        assert run([str(first_copy)]).stdout.strip() == "main"
        assert run([str(second_copy)]).stdout.strip() == "worktree"
        # A subsequent shared-cache build cannot change an already copied executable.
        run(command(worktree, "build", "--offline"), env=dict(env, CREW_CHECK_LABEL="replacement"))
        assert run([str(root / "target/debug/crew-cargo-check")]).stdout.strip() == "worktree"
        assert run([str(first_copy)]).stdout.strip() == "main"
        for cargo in (["cargo"], command(root)):
            result = subprocess.run([*cargo, "run", "--offline", "--quiet", "--", "fail"], cwd=root, env=env, text=True, capture_output=True, timeout=30)
            assert result.returncode == 23, result.stderr
        assert "argument preserved" in run(command(root, "run", "--offline", "--quiet", "--", "-j20")).stdout
        for jobs in (("--jobs",), ("--jobs=nope",), ("--jobs=0",)):
            result = subprocess.run(command(root, "build", "--offline", *jobs), env=env, capture_output=True, timeout=30)
            assert result.returncode != 0
        child = subprocess.Popen(command(root, "run", "--offline", "--quiet", "--", "wait"), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            assert child.stdout.readline().strip() == "main"
            assert child.stdout.readline().strip() == "waiting"
            child.send_signal(signal.SIGTERM)
            child.communicate(timeout=10)
            assert child.returncode == -signal.SIGTERM
        finally:
            if child.poll() is None:
                child.terminate()
                child.communicate(timeout=10)
    print("local Cargo cache, gate, job cap, priority, snapshots, exit and signal checks passed")


if __name__ == "__main__":
    check()
