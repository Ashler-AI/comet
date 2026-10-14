#!/usr/bin/env python3
"""Protect one foreground command without keeping an idle devbox awake."""
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile

TTL_SECONDS = 3600
RENEW_SECONDS = 300


def renew_boat(sandbox_id):
    try:
        subprocess.run(
            ["boat", "--no-update", "--json", "extend", sandbox_id, "--ttl", str(TTL_SECONDS)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=30, check=True,
        )
        return True
    except (OSError, subprocess.SubprocessError):
        return False


def stop_work(process):
    if process.returncode is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def run(command, provider, sandbox_id, tasks_directory=Path("/.namespace/tasks")):
    marker = None
    if provider == "namespace":
        descriptor, name = tempfile.mkstemp(prefix=f"crew-job-{os.getpid()}-", dir=tasks_directory)
        os.close(descriptor)
        marker = Path(name)
    elif provider == "boat":
        if not renew_boat(sandbox_id):
            print("Could not protect this command from Boat auto-stop. Check this sandbox's Boat sign-in and sandbox.update permission.", file=sys.stderr)
            return 1
    else:
        raise ValueError("Expected namespace or boat")

    def interrupt(signum, _frame):
        raise KeyboardInterrupt(signum)

    previous = {signum: signal.signal(signum, interrupt) for signum in (signal.SIGINT, signal.SIGTERM)}
    process = None
    renew_failed = False
    try:
        process = subprocess.Popen(command, start_new_session=True)
        while True:
            try:
                status = process.wait(timeout=RENEW_SECONDS if provider == "boat" else None)
                return status if status >= 0 else 128 - status
            except subprocess.TimeoutExpired:
                if not renew_boat(sandbox_id):
                    renew_failed = True
                    print("Could not renew Boat sleep protection. Stopping this command; inspect its partial work before retrying.", file=sys.stderr)
                    stop_work(process)
                    return 1
    except KeyboardInterrupt as error:
        return 128 + (error.args[0] if error.args else signal.SIGINT)
    except OSError:
        print("Could not start the protected command.", file=sys.stderr)
        return 1
    finally:
        if process is not None:
            stop_work(process)
        if marker is not None:
            marker.unlink(missing_ok=True)
        elif not renew_failed and not renew_boat(sandbox_id):
            print("Could not reset Boat's idle deadline. Its last finite deadline remains in effect.", file=sys.stderr)
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def main():
    command = sys.argv[1:]
    provider = os.environ.get("COMET_DEVBOX_PROVIDER")
    sandbox_id = os.environ.get("COMET_DEVBOX_ID", "")
    if not command:
        print("Usage: crew-devbox-keepalive <command> [arguments...]", file=sys.stderr)
        return 2
    if provider == "namespace":
        valid = re.fullmatch(r"[a-z0-9]{13}", sandbox_id) and Path("/.namespace/tasks").is_dir()
    elif provider == "boat":
        valid = re.fullmatch(r"bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}", sandbox_id) and os.environ.get("BOAT_ID") == sandbox_id
    else:
        valid = False
    if sys.platform != "linux" or not valid:
        print("Run on the configured Namespace or Boat devbox with its current immutable ID.", file=sys.stderr)
        return 2
    return run(command, provider, sandbox_id)


if __name__ == "__main__":
    sys.exit(main())
