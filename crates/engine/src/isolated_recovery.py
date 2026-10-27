# Shipped with REPOSITORY_SOURCE and NATIVE_SOURCE by the native recovery transport.
import contextlib
import ctypes
import fcntl
import hashlib
import io
import json
import os
import pathlib
import re
import shutil
import stat
import sys
import uuid

MAX_JSON = 65536
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_NATIVE = 64 * 1024 * 1024
MAX_FILES = 50000
MAX_DIRECTORIES = 25000
PLATFORM = pathlib.Path("/workspace/ashler-platform").resolve(strict=True)


def fail(reason):
    raise ValueError(reason)


def sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def text(value, limit=4096):
    return isinstance(value, str) and 0 < len(value) <= limit and not any(ord(c) < 32 or ord(c) == 127 for c in value)


def regular(path, limit, contents=False):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit:
            fail("regular_file_bounds")
        digest, count, data = hashlib.sha256(), 0, bytearray()
        while True:
            block = stream.read(min(1024 * 1024, limit - count + 1))
            if not block:
                break
            count += len(block)
            if count > limit:
                fail("regular_file_bounds")
            digest.update(block)
            if contents:
                data.extend(block)
        if count != info.st_size:
            fail("changed_file")
    return digest.hexdigest(), count, stat.S_IMODE(info.st_mode), bytes(data)


def directory(path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or path.resolve(strict=True) != path:
        fail("directory_containment")

def private_directory(path):
    directory(path)
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fchmod(fd, 0o700)
        os.fsync(fd)
    finally:
        os.close(fd)


def private_file(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit:
            fail("private_file_bounds")
        os.fchmod(fd, 0o600)
        os.fsync(fd)
    finally:
        os.close(fd)




def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, data, immutable=False):
    if len(data) > MAX_JSON:
        fail("json_bounds")
    temporary = path.parent / ("write-" + uuid.uuid4().hex)
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        if immutable:
            try:
                publish_directory(temporary, path)
            except FileExistsError:
                if regular(path, MAX_JSON, True)[3] != data:
                    fail("conflicting_immutable_record")
        else:
            if path.is_symlink():
                fail("record_symlink")
            os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
    sync_dir(path.parent)


def encoded(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def walk(root, metadata=False, exclude=()):
    directory(root)
    count = directories = total = 0
    for parent, dirs, files in os.walk(root, followlinks=False):
        parent = pathlib.Path(parent)
        if parent == root:
            dirs[:] = [name for name in dirs if name not in exclude]
            files[:] = [name for name in files if name not in exclude]
        if not metadata and parent == root and ".git" in dirs:
            dirs.remove(".git")
        for name in sorted([*dirs, *files]):
            candidate = parent / name
            info = candidate.lstat()
            relative = candidate.relative_to(root).as_posix()
            if len(relative) > 4096 or any(ord(c) < 32 or ord(c) == 127 for c in relative):
                fail("tree_path")
            if stat.S_ISDIR(info.st_mode):
                directories += 1
                if directories > MAX_DIRECTORIES:
                    fail("tree_directory_bounds")
                yield relative, "directory", stat.S_IMODE(info.st_mode), None
            elif stat.S_ISLNK(info.st_mode) and not metadata:
                count += 1
                target = os.readlink(candidate)
                total += len(os.fsencode(target))
                yield relative, "symlink", stat.S_IMODE(info.st_mode), target
            elif stat.S_ISREG(info.st_mode):
                count += 1
                digest, size, mode, _ = regular(candidate, MAX_ARCHIVE if metadata else 1024 * 1024 * 1024)
                total += size
                yield relative, "regular", mode, (digest, size)
            else:
                fail("tree_type")
            if count > (250000 if metadata else MAX_FILES) or total > (MAX_ARCHIVE if metadata else 1024 * 1024 * 1024):
                fail("tree_expansion_bounds")


def compare_tree(expected, actual):
    # Compare every file, including ignored files, not only Git's visible diff.
    if sorted(walk(expected)) != sorted(walk(actual)):
        fail("published_tree_mismatch")


def check_metadata(expected, actual):
    def protected(root):
        return sorted(walk(root, metadata=True, exclude=("index", "logs", "objects")))
    # Git verifies the object graph and index separately; do not hash packs or
    # apply the archive-size cap to expanded Git metadata during adoption.
    for root in (expected, actual):
        directory(root / "objects")
        index = root / "index"
        if index.exists() or index.is_symlink():
            info = index.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                fail("git_index_type")
        logs = root / "logs"
        if logs.exists() or logs.is_symlink():
            directory(logs)
    if protected(expected) != protected(actual):
        fail("published_git_metadata_mismatch")


def native_entries(root, relative):
    directory(root)
    if stat.S_IMODE(root.stat().st_mode) != 0o700:
        fail("private_native_directory")
    expected_dirs = {parent.as_posix() for parent in pathlib.PurePosixPath(relative).parents if parent.as_posix() != "."}
    found = False
    for name, kind, mode, _ in walk(root, metadata=True):
        if kind == "directory" and name in expected_dirs and mode == 0o700:
            continue
        if kind != "regular" or name != relative or mode != 0o600 or found:
            fail("native_upload_entries")
        found = True
    if not found:
        fail("native_upload_entries")


def sync_tree(root):
    for parent, dirs, files in os.walk(root, topdown=False, followlinks=False):
        parent = pathlib.Path(parent)
        for name in files:
            candidate = parent / name
            if candidate.is_symlink():
                continue
            fd = os.open(candidate, os.O_RDONLY | os.O_NOFOLLOW)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        sync_dir(parent)


def publish_directory(source, destination):
    # First publication must not replace even an empty, unverified destination.
    libc = ctypes.CDLL(None, use_errno=True)
    if sys.platform == "linux":
        result = libc.renameat2(-100, os.fsencode(source), -100, os.fsencode(destination), 1)
    elif sys.platform == "darwin":
        result = libc.renamex_np(os.fsencode(source), os.fsencode(destination), 4)
    else:
        fail("atomic_noreplace_unavailable")
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    sync_dir(destination.parent)
    if source.parent != destination.parent:
        sync_dir(source.parent)


def copy_upload(source, destination):
    private_directory(source)
    destination.mkdir(mode=0o700)
    files = directories = total = 0
    for parent, dirs, names in os.walk(source, followlinks=False):
        parent = pathlib.Path(parent)
        for name in dirs:
            path = parent / name
            private_directory(path)
            directories += 1
            if directories > MAX_DIRECTORIES:
                fail("upload_directory_bounds")
            (destination / path.relative_to(source)).mkdir(mode=0o700)
        for name in names:
            path = parent / name
            relative = path.relative_to(source)
            if len(relative.as_posix()) > 4096:
                fail("upload_path_bounds")
            source_fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(source_fd, "rb") as incoming:
                info = os.fstat(incoming.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > MAX_ARCHIVE:
                    fail("upload_type_bounds")
                os.fchmod(incoming.fileno(), 0o600)
                os.fsync(incoming.fileno())
                files += 1
                total += info.st_size
                if files > MAX_FILES + 2 or total > MAX_ARCHIVE:
                    fail("upload_expansion_bounds")
                target = destination / relative
                out_fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                with os.fdopen(out_fd, "wb") as outgoing:
                    count = 0
                    while True:
                        block = incoming.read(min(1024 * 1024, info.st_size - count + 1))
                        if not block:
                            break
                        count += len(block)
                        if count > info.st_size:
                            fail("changed_upload")
                        outgoing.write(block)
                    if count != info.st_size:
                        fail("changed_upload")
                    outgoing.flush()
                    os.fsync(outgoing.fileno())
    sync_tree(destination)


def run_verifier(source, arguments, context):
    previous = sys.argv
    sys.argv = ["crew-isolated-verifier", *map(str, arguments)]
    try:
        # Trusted shipped code; the narrowly validated recovery context does not
        # rewrite either verifier and cannot alter the legacy import protocol.
        with contextlib.redirect_stdout(io.StringIO()):
            try:
                exec(compile(source, "<crew-isolated-verifier>", "exec"), {"CREW_ISOLATED_RECOVERY": context})
            except SystemExit as error:
                if error.code not in (None, 0):
                    raise
    finally:
        sys.argv = previous




def main():
    if len(sys.argv) != 3:
        fail("arguments")
    descriptor_path, attempt = map(pathlib.Path, sys.argv[1:])
    if not descriptor_path.is_absolute() or not attempt.is_absolute():
        fail("absolute_paths")
    descriptor_bytes = regular(descriptor_path, MAX_JSON, True)[3]
    descriptor = json.loads(descriptor_bytes)
    if not isinstance(descriptor, dict) or descriptor.get("version") != "crew.native-isolated-recovery.v1" or descriptor.get("provenance") != "legacy-recapture":
        fail("descriptor_version")
    operation_id = descriptor.get("operation_id")
    namespace = descriptor.get("namespace")
    if not text(operation_id) or not operation_id.startswith("crew-handoff:") or not operation_id.endswith(":isolated-recovery") or not sha(namespace) or hashlib.sha256(operation_id.encode()).hexdigest() != namespace:
        fail("operation_namespace")
    if not text(descriptor.get("sandbox_id")) or not isinstance(descriptor.get("scope"), dict) or not isinstance(descriptor.get("environment"), dict) or not text(descriptor.get("source_selector")) or not text(descriptor.get("source_cwd")):
        fail("descriptor_authority")
    native = descriptor.get("native")
    if not isinstance(native, dict) or not sha(native.get("archive_sha256")) or not sha(native.get("sha256")) or type(native.get("archive_bytes")) is not int or not 0 < native["archive_bytes"] <= MAX_ARCHIVE or type(native.get("byte_count")) is not int or not 0 < native["byte_count"] <= MAX_NATIVE or not text(native.get("native_session_id"), 128) or not text(native.get("cwd")):
        fail("native_descriptor")
    identity = bytearray(bytes.fromhex(namespace)[:16])
    identity[6] = (identity[6] & 15) | 128
    identity[8] = (identity[8] & 63) | 128
    target_native_id = str(uuid.UUID(bytes=bytes(identity)))
    if descriptor.get("target_native_id") != target_native_id or target_native_id == native["native_session_id"]:
        fail("target_native_identity")
    relative = native.get("storage_relative_path")
    if not text(relative) or "\\" in relative or any(p in ("", ".", "..") for p in relative.split("/")) or pathlib.PurePosixPath(relative).is_absolute() or not relative.endswith(".jsonl"):
        fail("native_relative_path")
    if pathlib.PurePosixPath(relative).name == "recovered-" + namespace + ".jsonl":
        fail("legacy_native_basename_collision")
    if type(descriptor.get("session_only")) is not bool:
        fail("session_only")
    repository = descriptor.get("repository")
    if descriptor["session_only"]:
        if repository is not None:
            fail("session_only_repository")
        cwd = str(PLATFORM)
    else:
        if not isinstance(repository, dict) or not sha(repository.get("archive_sha256")) or type(repository.get("archive_bytes")) is not int or not 0 < repository["archive_bytes"] <= MAX_ARCHIVE or not sha(repository.get("manifest_sha256")) or not isinstance(repository.get("base_sha"), str) or re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", repository["base_sha"]) is None or type(repository.get("entry_count")) is not int or not 0 <= repository["entry_count"] <= 25000:
            fail("repository_descriptor")
        cwd_relative = repository.get("cwd_relative_path")
        if not isinstance(cwd_relative, str) or len(cwd_relative) > 4096 or "\\" in cwd_relative or (cwd_relative and any(part in ("", ".", "..") for part in cwd_relative.split("/"))):
            fail("repository_cwd")
        cwd = str(PLATFORM.parent / ("crew-native-" + namespace) / cwd_relative)
    if descriptor.get("published_cwd") != cwd:
        fail("published_cwd")
    scaffold = PLATFORM / ".scaffold"
    directory(scaffold)
    operation = scaffold / ("crew-native-" + namespace)
    directory(operation)
    directory(operation / "uploads")
    if attempt.parent != operation / "uploads" or re.fullmatch(r"[0-9a-f]{32}|[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", attempt.name) is None or descriptor_path.parent != attempt:
        fail("upload_attempt_namespace")
    directory(attempt)
    if descriptor_path.resolve(strict=True) != descriptor_path:
        fail("descriptor_containment")
    # The upload API can create 0755 directories/0644 text files. Tighten only
    # validated operation-owned paths, never platform or legacy ancestors.
    for owned in (operation, operation / "uploads", attempt):
        private_directory(owned)
    private_file(descriptor_path, MAX_JSON)
    uploaded_helper = attempt / "recovery.py"
    if uploaded_helper.exists() or uploaded_helper.is_symlink():
        private_file(uploaded_helper, 1024 * 1024)
    lock_fd = os.open(operation / "lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    try:
        info = os.fstat(lock_fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            fail("lock_inode")
        os.set_inheritable(lock_fd, True)
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        os.fchmod(lock_fd, 0o600)
        os.fsync(lock_fd)
        for name in ("descriptor.json", "phase.json", "receipt.json"):
            record = operation / name
            if record.exists() or record.is_symlink():
                private_file(record, MAX_JSON)
        sync_dir(operation)
        atomic(operation / "descriptor.json", descriptor_bytes, immutable=True)
        descriptor_sha = hashlib.sha256(descriptor_bytes).hexdigest()
        phase_path = operation / "phase.json"
        phases = ["sealed", "repository", "native", "completed"]
        current = None
        if phase_path.exists() or phase_path.is_symlink():
            state = json.loads(regular(phase_path, MAX_JSON, True)[3])
            if not isinstance(state, dict) or state.get("descriptor_sha256") != descriptor_sha or state.get("phase") not in phases:
                fail("phase_record")
            current = state["phase"]
        def phase(value):
            nonlocal current
            if current is None or phases.index(value) > phases.index(current):
                atomic(phase_path, encoded({"descriptor_sha256": descriptor_sha, "phase": value}))
                current = value
        context = {"namespace": namespace, "operation_root": str(operation), "session_only": descriptor["session_only"], "target_native_id": target_native_id, "lock_fd": lock_fd, "phase": phase, "compare_tree": compare_tree, "check_metadata": check_metadata, "sync_tree": sync_tree, "publish_directory": publish_directory}
        sealed = operation / "sealed"
        if not sealed.exists():
            candidate = operation / ("seal-" + uuid.uuid4().hex)
            candidate.mkdir(mode=0o700)
            try:
                copy_upload(attempt / "native", candidate / "native")
                validate = dict(context, validate_only=True)
                if not descriptor["session_only"]:
                    copy_upload(attempt / "repository", candidate / "repository")
                    repo_args = [candidate / "repository", repository["manifest_sha256"], repository["base_sha"], repository["entry_count"], json.dumps(repository["cwd_relative_path"])]
                    run_verifier(REPOSITORY_SOURCE, repo_args, validate)
                native_args = [candidate / "native", relative, native["sha256"], native["byte_count"], native["native_session_id"], native["cwd"], cwd]
                run_verifier(NATIVE_SOURCE, native_args, validate)
                native_entries(candidate / "native", relative)
                sync_tree(candidate)
                publish_directory(candidate, sealed)
            finally:
                if candidate.exists():
                    shutil.rmtree(candidate)
        directory(sealed)
        expected_sealed = {"native"} if descriptor["session_only"] else {"native", "repository"}
        if stat.S_IMODE(sealed.stat().st_mode) != 0o700:
            fail("private_sealed_directory")
        found_sealed = set()
        with os.scandir(sealed) as entries:
            for entry in entries:
                if entry.name not in expected_sealed or not entry.is_dir(follow_symlinks=False):
                    fail("sealed_entries")
                found_sealed.add(entry.name)
        if found_sealed != expected_sealed:
            fail("sealed_entries")
        native_entries(sealed / "native", relative)
        phase("sealed")
        if not descriptor["session_only"]:
            repo_args = [sealed / "repository", repository["manifest_sha256"], repository["base_sha"], repository["entry_count"], json.dumps(repository["cwd_relative_path"])]
            run_verifier(REPOSITORY_SOURCE, repo_args, context)
            if context["cwd"] != cwd:
                fail("verified_cwd")
        native_args = [sealed / "native", relative, native["sha256"], native["byte_count"], native["native_session_id"], native["cwd"], cwd]
        run_verifier(NATIVE_SOURCE, native_args, context)
        receipt = {"descriptor_sha256": descriptor_sha, "cwd": cwd, "native_session_id": target_native_id, "source_native_session_id": native["native_session_id"], "source_sha256": native["sha256"], "rebound_sha256": context["rebound_sha256"], "rebound_bytes": context["rebound_bytes"], "launch_selector": context["launch_selector"]}
        # A receipt is evidence only after the current files have passed readback.
        atomic(operation / "receipt.json", encoded(receipt), immutable=True)
        phase("completed")
        # Only this invocation's successful upload may be discarded. Never scan
        # or clean old attempts, legacy staging, or any published workspace.
        for name in ("repository", "native"):
            uploaded = attempt / name
            if uploaded.exists() or uploaded.is_symlink():
                directory(uploaded)
                shutil.rmtree(uploaded)
        sync_dir(attempt)
        print(encoded(receipt).decode())
    finally:
        os.close(lock_fd)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        print(("crew_isolated_recovery:" + str(error)).encode("utf-8")[:2048].decode("utf-8", "ignore"), file=sys.stderr)
        raise SystemExit(60)
