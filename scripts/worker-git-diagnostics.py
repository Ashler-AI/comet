#!/usr/bin/env python3
"""Disposable CI characterization; never executes on the local workstation."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument("--revision", required=True)
args = parser.parse_args()
if os.environ.get("GITHUB_ACTIONS") != "true":
    raise SystemExit("This characterization is authorized only on GitHub runners")
os.environ["ASHLER_INCREMENTAL_TSC_CHECKS"] = "false"
root = Path(__file__).resolve().parents[1]
evidence = root / "git-diagnostics"
evidence.mkdir(exist_ok=True)
seed = (root / "crates/engine/tests/worker_sessions.rs").read_text()
revision = subprocess.check_output(["git", "rev-parse", args.revision], cwd=root, text=True).strip()
checkout = root / "diagnostic-checkout"
subprocess.run(["git", "worktree", "add", "--detach", str(checkout), revision], cwd=root, check=True)
api = (checkout / "crates/engine/src/doc_host.rs").read_text()
async_api = "pub async fn queue_command_with_id(" in api
if not async_api and "pub fn queue_command_with_id(" not in api:
    raise SystemExit("Baseline has no worker admission API; not a comparable scenario")

if async_api:
    source = seed
else:
    # b36895e has an unrelated Arc borrow compile error. Preserve semantics
    # while making the reference explicit; record the exact disposable patch.
    old_borrow = "self.persist_handle(&self.open(chat_id)?)"
    new_borrow = "self.persist_handle(self.open(chat_id)?.as_ref())"
    assert api.count(old_borrow) == 1
    (checkout / "crates/engine/src/doc_host.rs").write_text(api.replace(old_borrow, new_borrow))
    (evidence / "baseline-compile-adaptation.txt").write_text(old_borrow + "\n=>\n" + new_borrow + "\n")
    prefix = seed.split("#[tokio::test]\nasync fn worker_creation_retries_preserve_checkout_config_and_ownership_across_restart()", 1)[0]
    module = seed[seed.index("#[cfg(unix)]\nmod async_admission {"):]
    module = module.split('    #[tokio::test(flavor = "current_thread")]\n    async fn git_config_failure_rejects_admission_without_append_or_harness()', 1)[0]
    source = prefix + module + "}\n"
    for ident, prompt in [("dropped", "must not run"), ("deadline", "must not run")]:
        old = f'let admission = core.doc_host.queue_command_with_id(WORKER, "{ident}", payload("{ident}", "{prompt}"));'
        new = f'let admission = async {{ core.doc_host.queue_command_with_id(WORKER, "{ident}", payload("{ident}", "{prompt}")) }};'
        assert source.count(old) == 1, old
        source = source.replace(old, new)
    old = 'core.doc_host.command_entry(WORKER, "dropped"))'
    assert source.count(old) == 1
    source = source.replace(old, 'async { core.doc_host.command_entry(WORKER, "dropped") })')

observer = r'''
    static OBSERVED_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

    fn observe_fifo(label: &str, fifo: &Path) {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let listing = std::process::Command::new("lsof")
            .args(["-nP", "-Fpcfa", "--"]).arg(fifo).output().expect("lsof available on CI runner");
        let text = String::from_utf8_lossy(&listing.stdout);
        eprintln!("GIT_DIAGNOSTIC label={label} at_ms={at} test_pid={} fifo={} lsof_status={}\n{}\n{}",
            std::process::id(), fifo.display(), listing.status, text, String::from_utf8_lossy(&listing.stderr));
        let census = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,stat=,args="])
            .output().expect("ps process census available on CI runner");
        let census_text = String::from_utf8_lossy(&census.stdout);
        let pids = {
            let mut known = OBSERVED_PIDS.lock().unwrap();
            for line in census_text.lines() {
                let mut fields = line.split_whitespace();
                let pid = fields.next().and_then(|value| value.parse::<u32>().ok());
                let parent = fields.next().and_then(|value| value.parse::<u32>().ok());
                let _state = fields.next();
                let executable = fields.next().unwrap_or("");
                if parent == Some(std::process::id()) {
                    eprintln!("GIT_DIAGNOSTIC direct_child label={label} {line}");
                    if executable.rsplit('/').next() == Some("git") {
                        if let Some(pid) = pid && !known.contains(&pid) { known.push(pid); }
                    }
                }
            }
            for line in text.lines() {
                if let Some(pid) = line.strip_prefix('p').and_then(|pid| pid.parse::<u32>().ok()) {
                    if pid != std::process::id() && !known.contains(&pid) { known.push(pid); }
                }
            }
            known.clone()
        };
        for pid in pids {
            let state = std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "pid=,ppid=,stat=,args="])
                .output().expect("ps available on CI runner");
            eprintln!("GIT_DIAGNOSTIC known_pid={pid} label={label} ps_status={} {}",
                state.status, String::from_utf8_lossy(&state.stdout));
        }
    }
'''

def replace_once(old, new):
    global source
    assert source.count(old) == 1, old
    source = source.replace(old, new)

replace_once("    struct GitStall {", observer + "\n    struct GitStall {")
replace_once("                            *writer.lock() = Some(file);", "                            *writer.lock() = Some(file);\n                            observe_fifo(\"watchdog-reader-handshake\", &fifo);")
replace_once("                            restore_config(&config, original_config.as_deref());", "                            observe_fifo(\"watchdog-before-release\", &fifo);\n                            restore_config(&config, original_config.as_deref());")
replace_once("                            writer.lock().take();\n                            return;", "                            writer.lock().take();\n                            observe_fifo(\"watchdog-after-release\", &fifo);\n                            return;")
replace_once("        async fn assert_reader_killed(&self) {\n            tokio::time::timeout", "        async fn assert_reader_killed(&self) {\n            observe_fifo(\"before-reader-closure-check\", &self.fifo);\n            let result = tokio::time::timeout")
replace_once('            }).await.expect("cancelled Git must close its FIFO reader, not survive in the background");', '            }).await;\n            observe_fifo("after-reader-closure-check", &self.fifo);\n            result.expect("cancelled Git must close its FIFO reader, not survive in the background");')
replace_once("        async fn reached<F: Future>(&mut self, admission: Pin<&mut F>) {", '        async fn reached<F: Future>(&mut self, admission: Pin<&mut F>) {\n            eprintln!("GIT_DIAGNOSTIC polling_admission test_pid={}", std::process::id());')
replace_once("            assert!(!self.expired.load(Ordering::SeqCst), \"runtime was blocked until watchdog released Git\");", '            eprintln!("GIT_DIAGNOSTIC handshake_returned watchdog_expired={}", self.expired.load(Ordering::SeqCst));\n            assert!(!self.expired.load(Ordering::SeqCst), "runtime was blocked until watchdog released Git");')
(checkout / "crates/engine/tests/worker_git_diagnostics.rs").write_text(source)
(evidence / "generated-test.rs").write_text(source)
(evidence / "source.json").write_text(json.dumps({
    "revision": revision, "asyncApi": async_api,
    "diagnosticSource": os.environ["GITHUB_SHA"],
    "adaptation": "none" if async_api else "Synchronous APIs wrapped in directly polled async blocks, not spawn_blocking. Existing doc_host Arc borrow compile error adapted with .as_ref(); exact change recorded separately. Blocking before cancellation is not a comparable cancellation pass.",
    "assertions": "Original scheduler, no-admission, deadline, and FIFO closure assertions retained. Added lsof PID/FD and ps state observations only."
}, indent=2))

# Compile only on this authorized remote runner, once. Each case has a process-
# group wall-clock guard independent of Tokio; descendants cannot outlive the job.
build = subprocess.run(["cargo", "test", "--locked", "-p", "comet-engine", "--test", "worker_git_diagnostics", "--no-run", "--message-format=json"], cwd=checkout, capture_output=True, text=True)
(evidence / "build.jsonl").write_text(build.stdout)
(evidence / "build.stderr").write_text(build.stderr)
if build.returncode:
    print(build.stderr)
    raise SystemExit(build.returncode)
executables = []
for line in build.stdout.splitlines():
    try: item = json.loads(line)
    except json.JSONDecodeError: continue
    if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "worker_git_diagnostics" and item.get("executable"):
        executables.append(item["executable"])
assert len(executables) == 1, executables
cases = [
    ("dropped", ["--exact", "async_admission::stalled_git_yields_and_dropped_admission_kills_child_without_append", "--nocapture"]),
    ("deadline", ["--exact", "async_admission::stalled_git_deadline_kills_child_and_fails_admission_closed", "--nocapture"]),
    ("concurrent-suite", ["--nocapture"]),
]
results = []
for label, flags in cases:
    started = time.monotonic()
    process = subprocess.Popen([executables[0], *flags], cwd=checkout, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
    timed_out = False
    try:
        output, _ = process.communicate(timeout=90)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
    finally:
        try: os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError: pass
    (evidence / f"{label}.log").write_text(output)
    row = {"case": label, "exitCode": process.returncode, "wallTimeout": timed_out, "seconds": round(time.monotonic() - started, 3)}
    results.append(row)
    print(json.dumps(row))
(evidence / "results.json").write_text(json.dumps(results, indent=2))
# Characterization exit status does NOT turn failed tests into passing gates.
# Actual test exit codes and wall-clock failures are kept in results.json.
