#!/usr/bin/env python3
"""Local Cargo CPU/cache policy; CI retains Cargo's normal execution policy."""
import contextlib
import fcntl
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent


def main():
    env = dict(os.environ, ASHLER_INCREMENTAL_TSC_CHECKS="false")
    local = env.get("COMET_LOCAL_AGENT_RUNTIME") == "1" or env.get("CI", "").lower() in ("", "false", "0")
    args, copies = [], []
    incoming = iter(sys.argv[1:])
    for arg in incoming:
        if arg == "--":
            args.extend([arg, *incoming])
            break
        if arg == "--copy-binary":
            copies.append((next(incoming), Path(next(incoming)).absolute()))
        else:
            args.append(arg)

    target = env.get("CARGO_TARGET_DIR")
    cargo_options = args[:args.index("--")] if "--" in args else args
    for i, arg in enumerate(cargo_options):
        if arg == "--target-dir":
            target = cargo_options[i + 1]
        elif arg.startswith("--target-dir="):
            target = arg.split("=", 1)[1]
    if target is None:
        base = ROOT
        if local:
            common = subprocess.check_output(
                ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
                cwd=ROOT, env=env, text=True,
            ).strip()
            base = Path(common).parent
        target = base / "target"
    target = (ROOT / target).resolve()
    if "--print-target-dir" in args:
        print(target)
        return 0
    env["CARGO_TARGET_DIR"] = str(target)
    env["PATH"] = env.get("PATH", "") + os.pathsep + str(Path.home() / ".cargo/bin")

    lock = contextlib.nullcontext()
    if local:
        def cap(value):
            try:
                return value if int(value) == 0 else ("1" if int(value) == 1 else "2")
            except ValueError:
                return value  # Cargo reports invalid job counts itself.

        env["CARGO_BUILD_JOBS"] = cap(env.get("CARGO_BUILD_JOBS", "2"))
        for i, arg in enumerate(cargo_options):
            if arg in ("-j", "--jobs"):
                args[i + 1] = cap(args[i + 1])
            elif arg.startswith("--jobs="):
                args[i] = "--jobs=" + cap(arg.split("=", 1)[1])
            elif arg.startswith("-j") and len(arg) > 2:
                args[i] = "-j" + cap(arg[2:])
        commands = {"build", "check", "test", "run", "bench", "doc", "rustc", "rustdoc", "clippy", "fix", "install"}
        if any(arg in commands for arg in cargo_options) and not any(
            arg in ("-j", "--jobs") or arg.startswith(("-j", "--jobs=")) for arg in cargo_options
        ):
            args.insert(len(cargo_options), "--jobs=" + env["CARGO_BUILD_JOBS"])
        os.setpriority(os.PRIO_PROCESS, 0, max(10, os.getpriority(os.PRIO_PROCESS, 0)))
        lock_path = Path.home() / ".cache/crew/cargo-build.lock"
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        lock = lock_path.open("a")

    with lock as handle:
        if local:
            # ponytail: one user-wide gate; split only if concurrent builds become safe.
            fcntl.flock(handle, fcntl.LOCK_EX)
        child = subprocess.Popen(
            ["cargo", *args], cwd=ROOT, env=env, start_new_session=True,
            pass_fds=(handle.fileno(),) if local else (),
        )
        def forward(received, _frame):
            try:
                os.killpg(child.pid, received)
            except ProcessLookupError:
                pass

        handlers = {sig: signal.signal(sig, forward) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
        status = child.wait()
        for sig, previous in handlers.items():
            signal.signal(sig, previous)
        if status == 0:
            # Snapshot while gated: the next worktree can replace cached binaries.
            for source, destination in copies:
                shutil.copy2(target / source, destination)
    if status < 0:
        sig = -status
        signal.signal(sig, signal.SIG_DFL)
        os.kill(os.getpid(), sig)
    return status


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, IndexError, StopIteration) as error:
        print(f"local-cargo: {error}", file=sys.stderr)
        sys.exit(1)
