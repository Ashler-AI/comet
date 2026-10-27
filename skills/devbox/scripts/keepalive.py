#!/usr/bin/env python3
"""Finite foreground sleep protection; never an idle keepalive daemon."""
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import tempfile

TTL_SECONDS = 3600
RENEW_SECONDS = 300


def renew_boat(sandbox_id):
    if sys.platform != "linux" or os.environ.get("COMET_DEVBOX_PROVIDER") != "boat" \
            or not re.fullmatch(r"bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}", sandbox_id) \
            or os.environ.get("COMET_DEVBOX_ID") != sandbox_id or os.environ.get("BOAT_ID") != sandbox_id:
        return False
    process = None
    try:
        process = subprocess.Popen(
            ["boat", "--no-update", "--json", "extend", sandbox_id, "--ttl", str(TTL_SECONDS)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        return wait_for_exit(process, 30) == 0
    except (OSError, subprocess.SubprocessError):
        return False
    finally:
        if process is not None:
            stop_work(process, grace=0)


def stop_work(process, grace=3):
    # Keep the leader reserved until every descendant receives the final KILL;
    # repeated cancellation cannot interrupt this ownership-critical cleanup.
    previous = {signum: signal.signal(signum, signal.SIG_IGN) for signum in (signal.SIGINT, signal.SIGTERM)}
    try:
        try:
            os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        except ChildProcessError:
            return  # Already reaped: its process-group ID may have been reused.
        for sig in ((signal.SIGTERM, signal.SIGKILL) if grace else (signal.SIGKILL,)):
            try:
                os.killpg(process.pid, sig)
            except ProcessLookupError:
                pass
            if sig == signal.SIGTERM:
                time.sleep(grace)
        process.wait()
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def wait_for_exit(process, timeout):
    deadline = time.monotonic() + timeout
    while True:
        result = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        if result is not None:
            return result.si_status if result.si_code == os.CLD_EXITED else 128 + result.si_status
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        time.sleep(min(0.05, remaining))


def run(command, provider, sandbox_id, tasks_directory=Path("/.namespace/tasks"), timeout=None):
    # Install cancellation handling before admission's provider subprocess too.
    def interrupt(signum, _frame):
        raise KeyboardInterrupt(signum)

    previous = {signum: signal.signal(signum, interrupt) for signum in (signal.SIGINT, signal.SIGTERM)}
    process = None
    marker = None
    renew_failed = False
    deadline = time.monotonic() + timeout if timeout is not None else float("inf")
    try:
        if provider == "namespace":
            descriptor, name = tempfile.mkstemp(prefix=f"crew-job-{os.getpid()}-", dir=tasks_directory)
            os.close(descriptor)
            marker = Path(name)
        elif provider == "boat":
            if not renew_boat(sandbox_id):
                renew_failed = True
                print("Could not protect work; check this sandbox's Boat sign-in and sandbox.update permission.", file=sys.stderr)
                return 1
        else:
            raise ValueError("Expected namespace or boat")
        process = subprocess.Popen(command, start_new_session=True)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                print("Command exceeded its deadline; inspect partial work before retrying.", file=sys.stderr)
                return 124
            status = wait_for_exit(process, min(RENEW_SECONDS, remaining))
            if status is not None:
                return status
            if time.monotonic() >= deadline:
                return 124
            if provider == "boat" and not renew_boat(sandbox_id):
                renew_failed = True
                print("Could not renew protection; stopping this command. Inspect partial work before retrying.", file=sys.stderr)
                return 1
    except KeyboardInterrupt as error:
        return 128 + (error.args[0] if error.args else signal.SIGINT)
    except OSError:
        print("Could not start the protected setup command.", file=sys.stderr)
        return 1
    finally:
        # A second cancellation must not interrupt cleanup and release the
        # reserved leader before its descendants have received the final KILL.
        for signum in previous:
            signal.signal(signum, signal.SIG_IGN)
        if process is not None:
            stop_work(process)
        # One finite idle reset; never a persistent daemon or open-ended lease.
        if marker is not None:
            marker.unlink(missing_ok=True)
        elif provider == "boat" and not renew_failed and not renew_boat(sandbox_id):
            print("Could not reset Boat's idle deadline; its last finite deadline remains in effect.", file=sys.stderr)
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def main():
    args = sys.argv[1:]
    timeout = None
    if args[:1] == ["--timeout"]:
        if len(args) < 4 or not args[1].isdigit() or args[2] != "--" or not 1 <= int(args[1]) <= 86400:
            print("Usage: keepalive.py [--timeout SECONDS (1..86400) --] command [args...]", file=sys.stderr)
            return 2
        timeout, args = int(args[1]), args[3:]
    provider = os.environ.get("COMET_DEVBOX_PROVIDER")
    sandbox_id = os.environ.get("COMET_DEVBOX_ID", "")
    namespace = Path("/.namespace/tasks").is_dir()
    valid = ((provider == "boat" and not namespace and os.environ.get("BOAT_ID") == sandbox_id
              and re.fullmatch(r"bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}", sandbox_id))
             or (provider == "namespace" and namespace and not os.environ.get("BOAT_ID")
                 and re.fullmatch(r"[a-z0-9]{13}", sandbox_id)))
    if sys.platform != "linux" or not valid or not args:
        print("Run a command on the matching native Namespace or Boat host with its immutable ID.", file=sys.stderr)
        return 2
    return run(args, provider, sandbox_id, timeout=timeout)


if __name__ == "__main__":
    sys.exit(main())
