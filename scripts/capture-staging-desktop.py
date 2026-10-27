#!/usr/bin/env python3
"""Capture ALL copied-profile desktop sources; never import or mutate the copy.

Dry run (default; authenticated identity GET, no remote writes):
  python3 scripts/capture-staging-desktop.py --database /copy/docs.sqlite3 \
    --auth-file /private/auth.json --output /private/capture --migration-id ID
Freeze/capture the exact same inventory: append --apply. Interrupted recovery:
append --resume instead; this EXPLICITLY authorizes same-ID POSTs whose durable
receipts are missing. No automatic mutation retries, new IDs, or scope fallback.
For already cloud-frozen s4 rooms, --reuse-receipts FILE supplies a JSON object
mapping canonical UUID -> exact authenticated freeze receipt. Those rooms NEVER
receive a POST; their immutable GET uses the original migration ID. Supply this
same map on dry run/apply/resume; it is pinned in plan.json. No raw rewriting.
Prefer --cloud-plan PREPARED_CLOUD_PLAN for overlapping s4 rooms: records[].scope
is a selector ONLY; authenticated coordinator source-receipt/source GETs provide
the original authority and bytes. There is no Edge POST or fallback for matches.
--via-staging-bridge routes ONLY coordinator requests through the existing fixed
127.0.0.1:18081 service bridge after health and matching-owner verification.
Canonical owner verification and every legacy Edge request remain direct HTTPS.

Successful output: raw/{workspace.json,sessions/UUID.json} and context.json for
comet export-backup --source-storage-directory raw/ --source-context context.json.
plan.json retains every original scope; receipts/ retains authenticated freeze
receipts; intents/ records possible writes BEFORE dispatch; complete.json is
written only after every raw export validates. Partial downloads stay in parts/.
Do not hand raw/ to the consumer until complete.json exists. Copy must have a
checkpointed SQLite DB, no nonempty WAL; stop original writers before copying.
Credentials come only from the auth FILE and are sent only as HTTPS headers.
Context requests the native staging target; it is NOT fresh target-deployment
proof. The final importer must freshly verify the native deployment after swap.
Terminal-original-epoch candidates additionally retain exact authenticated JSON
at terminal-authority/sessions/UUID.json. Pass terminal-authority/ to the existing
comet export-backup --terminal-authority-root only after complete.json exists.
An exact raw reuse receipt does NOT resolve ambiguous terminal physical epochs.
Optional --native-cli /ABS/COMET --source-backup-root /ABS/COPY --writers-stopped
invokes the native read-only inspect-backup classifier; repeat
--native-history-root /ABS/COPY/ROOT for every disjoint copied native history
root. Only DB/snapshot-hash-bound inert results waive a scope-less room fetch.
No caller classification JSON, guessed Loro heuristic, or ID allowlist.
"""
import argparse
import codecs
import contextlib
import fcntl
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import subprocess
import sys
import uuid
from urllib.parse import quote, urlencode, urlsplit

EDGE = "https://comet-staging.internal.ashler.com"
CONTROL = "https://scaffold-staging.internal.ashler.com"
PROJECT = "ashler-staging"
FORMAT = "legacy-session-sql-v1"
CHUNK = 256 * 1024
UUID = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\Z")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def regular(path, private=False):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode), "nonregular_file")
    if private:
        require(info.st_mode & 0o777 == 0o600, "evidence_permissions_changed")


def hash_file(path):
    regular(path)
    digest, size = hashlib.sha256(), 0
    with path.open("rb") as stream:
        while chunk := stream.read(CHUNK):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def exclusive_bytes(path, raw):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    sync_directory(path.parent)


def exclusive(path, value):
    exclusive_bytes(path, encoded(value))


def load(path):
    regular(path, private=True)
    return json.loads(path.read_bytes())


def keep(path, value):
    if path.exists():
        require(load(path) == value, "persisted_evidence_changed")
    else:
        exclusive(path, value)


def private_directory(path):
    if not path.exists():
        path.mkdir(mode=0o700)
        sync_directory(path.parent)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_mode & 0o077 == 0, "private_directory_required")


def token_from_file(path):
    regular(path)
    text = path.read_text().strip()
    if text.startswith("{"):
        auth = json.loads(text)
        require(auth.get("resource", CONTROL) == CONTROL and auth.get("edgeOrigin", EDGE) == EDGE, "auth_file_not_staging")
        text = auth.get("accessToken", auth.get("access_token"))
    require(isinstance(text, str) and bool(text.strip()) and not re.search(r"[\r\n]", text), "invalid_auth_file")
    return text


class Api:
    def __init__(self, token, via_staging_bridge=False):
        self.token = token
        self.via_staging_bridge = via_staging_bridge
        self.bridge_verified = False

    @contextlib.contextmanager
    def response(self, origin, path, body=None, canonical=False):
        require(origin in (EDGE, CONTROL) and path.startswith("/") and not path.startswith("//"), "invalid_staging_address")
        bridge = self.via_staging_bridge and origin == CONTROL and not canonical
        require(not bridge or self.bridge_verified or path in ("/health", "/api/code-sandboxes/auth/session"), "bridge_not_verified")
        connection = (http.client.HTTPConnection("127.0.0.1", 18081, timeout=900) if bridge
                      else http.client.HTTPSConnection(urlsplit(origin).hostname, timeout=900))
        headers = {"Authorization": "Bearer " + self.token, "Accept-Encoding": "identity"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        try:
            connection.request("POST" if body is not None else "GET", path,
                               body=encoded(body) if body is not None else None, headers=headers)
            response = connection.getresponse()
            require(response.status == 200, "http_" + str(response.status))
            require(response.getheader("Content-Encoding", "identity").lower() == "identity", "encoded_response_rejected")
            yield response
        finally:
            connection.close()

    def json(self, origin, path, body=None, canonical=False):
        with self.response(origin, path, body, canonical=canonical) as response:
            raw = response.read(1024 * 1024 + 1)
            require(len(raw) <= 1024 * 1024, "oversized_control_response")
            return json.loads(raw)

    def owner(self):
        identity = self.json(CONTROL, "/api/code-sandboxes/auth/session", canonical=True)
        subject = identity.get("actor", {}).get("sub")
        scopes = identity.get("scopes", [])
        require(identity.get("ok") is True and identity.get("resource", "").rstrip("/") == CONTROL
                and identity.get("actor", {}).get("auth") == "iap"
                and isinstance(subject, str) and bool(subject.strip()) and isinstance(scopes, list)
                and all(scope in scopes for scope in ("remote_code:create", "remote_code:read", "remote_code:write"))
                and any(scope in scopes for scope in ("remote_code:exec", "remote_code:lifecycle")), "verified_owner_required")
        subject = subject.strip().lower()
        if self.via_staging_bridge:
            self.bridge_verified = False
            health = self.json(CONTROL, "/health")
            require(health.get("service") == "scaffold-edge-bridge" and health.get("workerConfigured") is True, "bridge_not_configured")
            upstream = self.json(CONTROL, "/api/code-sandboxes/auth/session")
            require(upstream.get("ok") is True and upstream.get("resource", "").rstrip("/") == CONTROL
                    and upstream.get("actor", {}).get("auth") == "iap"
                    and upstream.get("actor", {}).get("sub") == identity["actor"]["sub"], "bridge_owner_mismatch")
            self.bridge_verified = True
        source = self.json(EDGE, "/auth/session")
        require(source.get("credential") == "scaffold" and source.get("userId") == subject
                and source.get("projectScope") == PROJECT and source.get("deploymentId") is None
                and all(capability in source.get("capabilities", []) for capability in
                        ("session.read", "session.chat", "session.control", "session.annotate",
                         "session.invite", "session.files", "session.environment")), "verified_legacy_source_required")
        return subject


def cloud_selectors(cloud, owner, reused=None):
    require(isinstance(cloud, dict) and isinstance(cloud.get("records"), list), "invalid_cloud_plan")
    require(reused is None or isinstance(reused, dict), "invalid_reuse_receipts")
    result = {}
    sandbox_ids = set()
    for record in cloud["records"]:
        require(isinstance(record, dict) and isinstance(record.get("id"), str)
                and record["id"] and record["id"] not in sandbox_ids, "invalid_cloud_record")
        sandbox_ids.add(record["id"])
        if record.get("disposition") == "not-crew":
            require("scope" not in record, "unexpected_noncrew_scope")
            continue
        scope = record.get("scope")
        require(isinstance(scope, dict) and scope.get("sandboxId") == record["id"]
                and scope.get("ownerSubject") == owner and scope.get("projectId") == PROJECT
                and scope.get("deploymentId") == PROJECT and isinstance(scope.get("logicalSessionId"), str)
                and UUID.fullmatch(scope["logicalSessionId"]), "unknown_or_foreign_cloud_selector")
        doc_id = scope["logicalSessionId"]
        # An explicit exact receipt selects the frozen source, never a physical
        # sandbox winner. Inventory validates it before any remote mutation.
        if doc_id in (reused or {}):
            continue
        require(doc_id not in result, "duplicate_cloud_logical_identity")
        result[doc_id] = record["id"]
    return result


def terminal_selectors(cloud):
    groups, candidates = {}, set()
    for record in cloud["records"]:
        scope = record.get("scope")
        if scope is not None:
            doc_id = scope["logicalSessionId"]
            groups.setdefault(doc_id, []).append({"scope": scope, "coordinatorLifecycleEpoch": record.get("coordinatorLifecycleEpoch")})
            if record.get("sourceDisposition") == "terminal-original-epoch-candidate":
                candidates.add(doc_id)
    return {doc_id: groups[doc_id] for doc_id in sorted(candidates)}


def cloud_path(sandbox):
    return "/api/sessions/" + quote(sandbox, safe="") + "/crew-migration"


def inspect_native(binary, database, roots, source_backup_root):
    binary = binary.absolute()
    binary_sha256, _ = hash_file(binary)
    require(source_backup_root.is_dir() and not source_backup_root.is_symlink(), "copied_source_backup_root_required")
    source_backup_root = source_backup_root.resolve(strict=True)
    database_relative = database.resolve(strict=True).relative_to(source_backup_root)
    command = [str(binary), "inspect-backup", "--source-backup-root", str(source_backup_root),
               "--database-relative", str(database_relative), "--writers-stopped"]
    for root in sorted({str(path.absolute()) for path in roots}):
        require(Path(root).is_dir() and not Path(root).is_symlink(), "copied_native_history_root_required")
        Path(root).resolve(strict=True).relative_to(source_backup_root)
        command.extend(["--native-history-root", root])
    result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, check=False)
    require(result.returncode == 0, "native_source_inspection_failed")
    require(hash_file(binary)[0] == binary_sha256, "native_classifier_changed")
    return {"command": command, "executableSha256": binary_sha256, "report": json.loads(result.stdout)}


def inventory(database, owner, migration, reused=None, cloud=None, terminal=None, classification=None):
    require(re.fullmatch(r"[A-Za-z0-9_-]{1,128}", migration) is not None, "invalid_migration_id")
    before, size = hash_file(database)
    wal = Path(str(database) + "-wal")
    require(not wal.exists() or (not wal.is_symlink() and wal.stat().st_size == 0), "uncheckpointed_source_wal")
    scopes, blockers, unscoped_snapshots = {}, [], {}
    with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro&immutable=1", uri=True)) as db:
        for doc_id, raw in db.execute("SELECT doc_id,scope FROM document_scopes ORDER BY doc_id"):
            require(isinstance(doc_id, str) and doc_id not in scopes, "duplicate_or_invalid_document_id")
            try:
                scopes[doc_id] = json.loads(raw)
            except (ValueError, TypeError):
                scopes[doc_id] = {"invalidOriginalScope": raw}
                blockers.append({"id": doc_id, "reason": "invalid_scope_json"})
        snapshots = []
        for rowid, doc_id in db.execute("SELECT rowid,doc_id FROM snapshots ORDER BY doc_id"):
            snapshots.append(doc_id)
            if doc_id not in scopes:
                digest, byte_count = hashlib.sha256(), 0
                with db.blobopen("snapshots", "bytes", rowid, readonly=True) as snapshot:
                    while chunk := snapshot.read(CHUNK):
                        digest.update(chunk)
                        byte_count += len(chunk)
                unscoped_snapshots[doc_id] = {"sha256": digest.hexdigest(), "bytes": byte_count}
    require(hash_file(database) == (before, size), "copied_database_changed")
    inert = {}
    if classification is not None:
        require(isinstance(classification, dict) and set(classification) == {"command", "executableSha256", "report"}, "invalid_native_classification")
        report = classification["report"]
        require(isinstance(report, dict) and set(report) == {"version", "sourceDatabaseSha256", "inertBootstrapSnapshots"}
                and type(report["version"]) is int and report["version"] == 1
                and report["sourceDatabaseSha256"] == before
                and isinstance(report["inertBootstrapSnapshots"], dict), "native_classification_database_mismatch")
        inert = report["inertBootstrapSnapshots"]
        for doc_id, evidence in inert.items():
            require(doc_id in unscoped_snapshots and isinstance(evidence, dict)
                    and set(evidence) == {"snapshotSha256", "disposition"}
                    and evidence["disposition"] == "inert-bootstrap"
                    and evidence["snapshotSha256"] == unscoped_snapshots[doc_id]["sha256"], "native_classification_snapshot_mismatch")
    for doc_id in snapshots:
        if doc_id not in scopes and doc_id not in inert:
            blockers.append({"id": doc_id, "reason": "snapshot_without_scope"})
    if len(set(snapshots)) != len(snapshots):
        blockers.append({"id": None, "reason": "duplicate_snapshot"})
    if "workspace3" not in scopes:
        blockers.append({"id": "workspace3", "reason": "missing_workspace_scope"})
    entries = []
    for doc_id, scope in scopes.items():
        entry = {"id": doc_id, "sourceScope": scope, "disposition": "blocked"}
        entries.append(entry)
        reason = None
        if not isinstance(scope, dict) or set(scope) != {"project", "principal", "deployment", "session"}:
            reason = "unknown_scope_schema"
        elif scope["project"] != PROJECT:
            reason = "foreign_or_unknown_project"
        elif doc_id == "workspace3":
            if (not isinstance(scope["principal"], str) or not scope["principal"].strip()
                    or scope["session"] not in (None, doc_id) or scope["deployment"] not in (None, PROJECT)):
                reason = "invalid_workspace_scope"
            else:
                entry.update(disposition="capture", path="/workspace/ashler-staging/migration-export", file="workspace.json", generation="workspace")
        elif scope["principal"] != owner:
            reason = "foreign_or_unknown_owner"
        elif UUID.fullmatch(doc_id):
            if scope["session"] != doc_id or scope["deployment"] not in (None, PROJECT):
                reason = "invalid_session_scope"
            else:
                selector = {"unscoped": "true"} if scope["deployment"] is None else {"deploymentId": PROJECT}
                entry.update(disposition="capture", path=f"/session/{doc_id}/migration-export?{urlencode(selector)}", file=f"sessions/{doc_id}.json", generation="session")
        elif re.fullmatch(r"local-chat-[A-Za-z0-9-]+", doc_id):
            if scope["session"] != doc_id or scope["deployment"] is not None:
                reason = "invalid_local_scope"
            else:
                entry["disposition"] = "local-only"
        else:
            parts = doc_id.split("::session::")
            canonical = parts[0]
            original = scopes.get(canonical)
            if (len(parts) > 1 and UUID.fullmatch(canonical) and all(part == canonical for part in parts)
                    and isinstance(original, dict) and original.get("principal") == owner
                    and original.get("project") == PROJECT and original.get("session") == canonical
                    and scope["session"] in (doc_id, canonical) and scope["deployment"] in (None, PROJECT)
                    and scope["deployment"] == original.get("deployment")):
                entry["disposition"] = "local-only"
            else:
                reason = "unknown_id_or_invalid_self_alias"
        if reason:
            blockers.append({"id": doc_id, "reason": reason})
    for doc_id in sorted(inert):
        entries.append({"id": doc_id, "sourceScope": None, "disposition": "inert-bootstrap",
                        "snapshotSha256": unscoped_snapshots[doc_id]["sha256"], "bytes": unscoped_snapshots[doc_id]["bytes"]})
    plan = {"version": 1, "database": str(database), "databaseSha256": before, "databaseBytes": size,
            "ownerSubject": owner, "migrationId": migration, "sourceOrigin": EDGE, "sourceResource": CONTROL,
            "entries": entries, "snapshotIds": snapshots, "unscopedSnapshots": unscoped_snapshots, "blockers": blockers,
            "reuseReceipts": {} if reused is None else reused, "cloudSources": {}, "terminalSources": {},
            "nativeClassification": classification}
    require(isinstance(plan["reuseReceipts"], dict), "invalid_reuse_receipts")
    by_id = {entry["id"]: entry for entry in entries}
    for doc_id, sandbox in (cloud or {}).items():
        entry = by_id.get(doc_id)
        if (entry is not None and entry["disposition"] == "capture" and entry["generation"] == "session"
                and entry["sourceScope"]["deployment"] == PROJECT):
            require(isinstance(sandbox, str) and bool(sandbox.strip()), "invalid_cloud_sandbox")
            plan["cloudSources"][doc_id] = sandbox
    require(not set(plan["reuseReceipts"]) & set(plan["cloudSources"]), "duplicate_source_authority_selectors")
    for doc_id, candidates in (terminal or {}).items():
        entry = by_id.get(doc_id)
        if (entry is None or entry["disposition"] != "capture" or entry["generation"] != "session"
                or entry["sourceScope"]["deployment"] != PROJECT):
            continue  # An unscoped s3 source is not the cloud physical epoch.
        require(isinstance(candidates, list) and bool(candidates), "invalid_terminal_candidates")
        for candidate in candidates:
            require(isinstance(candidate, dict) and set(candidate) == {"scope", "coordinatorLifecycleEpoch"}, "invalid_terminal_candidate")
            scope = candidate["scope"]
            require(isinstance(scope, dict) and set(scope) == {"ownerSubject", "projectId", "deploymentId",
                    "logicalSessionId", "sandboxId", "deviceId", "lifecycleEpoch"}
                    and scope["ownerSubject"] == owner and scope["projectId"] == PROJECT
                    and scope["deploymentId"] == PROJECT and scope["logicalSessionId"] == doc_id
                    and isinstance(scope["sandboxId"], str) and bool(scope["sandboxId"])
                    and scope["deviceId"] in ("comet-scaffold-" + scope["sandboxId"],
                                              "comet-scaffold-" + scope["sandboxId"] + "-e" + str(scope["lifecycleEpoch"]))
                    and type(scope["lifecycleEpoch"]) is int and 0 < scope["lifecycleEpoch"] <= 2**53 - 1,
                    "invalid_terminal_scope")
            require(type(candidate["coordinatorLifecycleEpoch"]) is int
                    and scope["lifecycleEpoch"] <= candidate["coordinatorLifecycleEpoch"] <= 2**53 - 1,
                    "invalid_terminal_coordinator_epoch")
        plan["terminalSources"][doc_id] = candidates
        if len(candidates) != 1:
            blockers.append({"id": doc_id, "reason": "terminal_authority_ambiguous_physical_epoch"})
    for doc_id, receipt in plan["reuseReceipts"].items():
        entry = by_id.get(doc_id)
        require(entry is not None and entry["disposition"] == "capture" and entry["generation"] == "session"
                and entry["sourceScope"]["deployment"] == PROJECT, "reuse_requires_exact_inventoried_s4")
        validate_receipt(receipt, entry, plan)
    return plan


def context(plan, receipts):
    return {"migrationId": plan["migrationId"], "ownerSubject": plan["ownerSubject"], "projectId": PROJECT,
            "deploymentId": PROJECT, "sourceOrigin": EDGE, "sourceResource": CONTROL,
            "targetOrigin": EDGE, "targetResource": CONTROL,
            "sourceWitnesses": {doc_id: {"migrationId": receipt["migrationId"], "sha256": receipt["sha256"]}
                                for doc_id, receipt in receipts.items()}}


def validate_receipt(receipt, entry, plan):
    require(isinstance(receipt, dict) and set(receipt) == {"version", "format", "sourceGeneration", "migrationId", "scope", "frozen", "frozenAt", "sha256", "bytes"}, "invalid_receipt_schema")
    require(receipt["version"] == 1 and receipt["format"] == FORMAT and receipt["frozen"] is True
            and receipt["sourceGeneration"] == entry["generation"]
            and isinstance(receipt["migrationId"], str) and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", receipt["migrationId"])
            and (entry["id"] in plan["cloudSources"] or receipt["migrationId"] == plan["reuseReceipts"].get(entry["id"], {}).get("migrationId", plan["migrationId"]))
            and isinstance(receipt["sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", receipt["sha256"])
            and type(receipt["bytes"]) is int and 0 < receipt["bytes"] <= 2**53 - 1
            and isinstance(receipt["frozenAt"], str) and re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z", receipt["frozenAt"]), "invalid_freeze_receipt")
    scope = receipt["scope"]
    if entry["generation"] == "workspace":
        require(isinstance(scope, dict) and set(scope) == {"projectId", "workspaceId", "authority", "exportedBySubject"}
                and scope["projectId"] == PROJECT and scope["workspaceId"] == "ws4/" + PROJECT
                and scope["authority"] == "project" and isinstance(scope["exportedBySubject"], str)
                and bool(scope["exportedBySubject"].strip()), "workspace_authority_mismatch")
    else:
        require(scope == {"ownerSubject": plan["ownerSubject"], "projectId": PROJECT,
                          "deploymentId": entry["sourceScope"]["deployment"], "logicalSessionId": entry["id"]}, "session_authority_mismatch")


def validate_envelope_header(path, receipt):
    # Only the bounded producer identity prefix is decoded, never the SQL arrays.
    with path.open("rb") as stream:
        text = codecs.getincrementaldecoder("utf-8")().decode(stream.read(65536), final=False)
    decoder = json.JSONDecoder()
    expected = {"schemaVersion": 1, "format": FORMAT, "sourceGeneration": receipt["sourceGeneration"],
                "migrationId": receipt["migrationId"], "scope": receipt["scope"], "frozenAt": receipt["frozenAt"]}
    offset = 0
    def punctuation(character):
        nonlocal offset
        while offset < len(text) and text[offset].isspace():
            offset += 1
        require(text[offset:offset + 1] == character, "invalid_export_identity_prefix")
        offset += 1
    punctuation("{")
    for key, value in expected.items():
        while offset < len(text) and text[offset].isspace():
            offset += 1
        actual_key, offset = decoder.raw_decode(text, offset)
        punctuation(":")
        while offset < len(text) and text[offset].isspace():
            offset += 1
        actual_value, offset = decoder.raw_decode(text, offset)
        require(actual_key == key and actual_value == value, "export_identity_mismatch")
        punctuation(",")
    while offset < len(text) and text[offset].isspace():
        offset += 1
    key, offset = decoder.raw_decode(text, offset)
    require(key == "meta", "invalid_export_identity_prefix")
    punctuation(":")
    punctuation("[")


def download(api, entry, plan, receipt, root):
    destination = root / "raw" / entry["file"]
    if destination.exists():
        regular(destination, private=True)
        require(hash_file(destination) == (receipt["sha256"], receipt["bytes"]), "retained_raw_bytes_changed")
        validate_envelope_header(destination, receipt)
        return
    sandbox = plan["cloudSources"].get(entry["id"])
    path = (cloud_path(sandbox) + "/source" if sandbox else entry["path"]
            + ("&" if "?" in entry["path"] else "?") + urlencode({"migrationId": receipt["migrationId"]}))
    temporary = root / "parts" / (entry["id"] + "-" + uuid.uuid4().hex + ".part")
    with api.response(CONTROL if sandbox else EDGE, path) as response:
        require(response.getheader("Content-Encoding", "identity").lower() == "identity", "encoded_response_rejected")
        require(response.getheader("Content-Length") == str(receipt["bytes"])
                and response.getheader("x-crew-migration-sha256") == receipt["sha256"], "export_headers_mismatch")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        digest, size = hashlib.sha256(), 0
        with os.fdopen(fd, "wb") as stream:
            while chunk := response.read(CHUNK):
                size += len(chunk)
                require(size <= receipt["bytes"], "export_too_long")
                digest.update(chunk)
                stream.write(chunk)
            stream.flush()
            os.fsync(stream.fileno())
        require((digest.hexdigest(), size) == (receipt["sha256"], receipt["bytes"]), "export_bytes_mismatch")
    validate_envelope_header(temporary, receipt)
    os.link(temporary, destination)  # Atomic, exclusive publication: never clobber evidence.
    sync_directory(destination.parent)
    temporary.unlink()


def capture_terminal_authorities(api, plan, receipts, root):
    if not plan["terminalSources"]:
        require(not (root / "terminal-authority").exists(), "unexpected_terminal_authority_directory")
        return {}
    directory = root / "terminal-authority"
    private_directory(directory)
    private_directory(directory / "sessions")
    hashes = {}
    for doc_id, candidates in plan["terminalSources"].items():
        require(len(candidates) == 1, "terminal_authority_ambiguous_physical_epoch")
        candidate = candidates[0]
        scope = candidate["scope"]
        receipt = receipts[doc_id]
        path = directory / "sessions" / (doc_id + ".json")
        if path.exists():
            regular(path, private=True)
            require(path.stat().st_size <= 1024 * 1024, "oversized_terminal_authority")
            raw = path.read_bytes()
        else:
            # A reused raw receipt must also match this exact physical record's
            # authenticated coordinator receipt, not merely the logical UUID.
            coordinator_receipt = api.json(CONTROL, cloud_path(scope["sandboxId"]) + "/source-receipt")
            require(coordinator_receipt == receipt, "terminal_coordinator_source_receipt_mismatch")
            with api.response(CONTROL, cloud_path(scope["sandboxId"]) + "/terminal-source-authority") as response:
                raw = response.read(1024 * 1024 + 1)
                require(len(raw) <= 1024 * 1024, "oversized_terminal_authority")
        authority = json.loads(raw)
        require(isinstance(authority, dict) and set(authority) == {"version", "kind", "scope", "coordinatorLifecycleEpoch", "migrationId", "sourceSha256", "terminal"}
                and authority["version"] == 1 and authority["kind"] == "terminal-source-authority"
                and authority["scope"] == scope and authority["migrationId"] == receipt["migrationId"]
                and type(authority["coordinatorLifecycleEpoch"]) is int
                and authority["coordinatorLifecycleEpoch"] == candidate["coordinatorLifecycleEpoch"]
                and authority["sourceSha256"] == receipt["sha256"], "terminal_source_authority_mismatch")
        terminal = authority["terminal"]
        require(isinstance(terminal, dict) and set(terminal) == {"status", "providerAction", "terminatedAt", "terminationReason",
                "physicalSandboxId", "providerNotFoundObservedAt", "retainedSnapshotIds"}
                and terminal["status"] in ("stopped", "failed") and terminal["providerAction"] == "kill"
                and terminal["terminationReason"] in ("timeout", "provider_missing")
                and terminal["physicalSandboxId"] == scope["sandboxId"] and terminal["retainedSnapshotIds"] == [],
                "invalid_terminal_source_authority")
        if not path.exists():
            exclusive_bytes(path, raw)  # Preserve the exact authenticated response, including whitespace.
        hashes[doc_id] = hashlib.sha256(raw).hexdigest()
    require({entry.name for entry in directory.iterdir()} == {"sessions"}
            and {entry.name for entry in (directory / "sessions").iterdir()} == {doc_id + ".json" for doc_id in hashes},
            "unexpected_terminal_authority_inventory")
    return hashes


def capture(api, plan, root, resume=False):
    require(not plan["blockers"], "inventory_contains_blockers")
    targets = [entry for entry in plan["entries"] if entry["disposition"] == "capture"]
    receipts = {}
    # Freeze every exact inventoried room before downloading any export.
    for entry in targets:
        intent_path = root / "intents" / (entry["id"] + ".json")
        receipt_path = root / "receipts" / (entry["id"] + ".json")
        reused = plan["reuseReceipts"].get(entry["id"])
        intent = {"migrationId": reused["migrationId"] if reused else plan["migrationId"],
                  "path": entry["path"], "sourceScope": entry["sourceScope"]}
        sandbox = plan["cloudSources"].get(entry["id"])
        if sandbox:
            require(not intent_path.exists(), "coordinator_room_has_mutation_intent")
            if receipt_path.exists():
                receipt = load(receipt_path)
            else:
                receipt = api.json(CONTROL, cloud_path(sandbox) + "/source-receipt")
                validate_receipt(receipt, entry, plan)
                exclusive(receipt_path, receipt)
        elif reused:
            require(not intent_path.exists(), "reused_room_has_mutation_intent")
            keep(receipt_path, reused)
            receipt = reused
        elif receipt_path.exists():
            require(intent_path.exists() and load(intent_path) == intent, "receipt_without_exact_intent")
            receipt = load(receipt_path)
        else:
            if intent_path.exists():
                require(resume, "ambiguous_freeze_requires_explicit_resume")
            keep(intent_path, intent)  # fsync precedes dispatch, including first attempt.
            receipt = api.json(EDGE, entry["path"], {"migrationId": plan["migrationId"]})
            validate_receipt(receipt, entry, plan)
            exclusive(receipt_path, receipt)
        validate_receipt(receipt, entry, plan)
        receipts[entry["id"]] = receipt
    keep(root / "context.json", context(plan, receipts))
    for entry in targets:
        download(api, entry, plan, receipts[entry["id"]], root)
    expected = {entry["file"] for entry in targets}
    actual = set()
    for child in (root / "raw").iterdir():
        if child.name == "sessions" and child.is_dir() and not child.is_symlink():
            for item in child.iterdir():
                regular(item, private=True)
                actual.add("sessions/" + item.name)
        else:
            regular(child, private=True)
            actual.add(child.name)
    require(actual == expected, "unexpected_raw_inventory")
    terminal_hashes = capture_terminal_authorities(api, plan, receipts, root)
    require(inventory(Path(plan["database"]), plan["ownerSubject"], plan["migrationId"],
                      plan["reuseReceipts"], plan["cloudSources"], plan["terminalSources"], plan["nativeClassification"]) == plan, "copied_inventory_changed")
    keep(root / "complete.json", {"version": 1, "planSha256": hashlib.sha256(encoded(plan)).hexdigest(),
                                  "files": {entry["file"]: receipts[entry["id"]] for entry in targets},
                                  "terminalAuthoritySha256": terminal_hashes})


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database", required=True, type=Path)
    parser.add_argument("--auth-file", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--migration-id", required=True)
    parser.add_argument("--reuse-receipts", type=Path)
    parser.add_argument("--cloud-plan", type=Path)
    parser.add_argument("--via-staging-bridge", action="store_true")
    parser.add_argument("--native-cli", type=Path)
    parser.add_argument("--source-backup-root", type=Path)
    parser.add_argument("--native-history-root", action="append", type=Path, default=[])
    parser.add_argument("--writers-stopped", action="store_true")
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--apply", action="store_true")
    modes.add_argument("--resume", action="store_true")
    modes.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    require(not args.native_history_root or args.native_cli is not None, "native_history_roots_require_native_cli")
    require(args.native_cli is None or args.writers_stopped, "native_inspection_requires_writers_stopped")
    require(args.native_cli is None or args.source_backup_root is not None, "native_inspection_requires_source_backup_root")
    # Refuse a symlink before resolving the copied database path.
    regular(args.database)
    database = args.database.absolute()
    api = Api(token_from_file(args.auth_file), via_staging_bridge=args.via_staging_bridge)
    reused = load(args.reuse_receipts) if args.reuse_receipts else {}
    owner = api.owner()
    cloud_plan = load(args.cloud_plan) if args.cloud_plan else None
    cloud = cloud_selectors(cloud_plan, owner, reused) if cloud_plan else {}
    terminal = terminal_selectors(cloud_plan) if cloud_plan else {}
    classification = inspect_native(args.native_cli, database, args.native_history_root, args.source_backup_root) if args.native_cli else None
    plan = inventory(database, owner, args.migration_id, reused, cloud, terminal, classification)
    root = args.output.absolute()
    if args.resume:
        require(root.exists() and (root / "plan.json").exists(), "resume_requires_existing_plan")
    private_directory(root)
    lock_fd = os.open(root / "lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, "wb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        keep(root / "plan.json", plan)
        if args.apply or args.resume:
            require(not plan["blockers"], "inventory_contains_blockers")
            for directory in ("intents", "receipts", "parts", "raw", "raw/sessions"):
                private_directory(root / directory)
            capture(api, plan, root, resume=args.resume)
    print(json.dumps({"mode": "resume" if args.resume else "apply" if args.apply else "dry-run",
                      "capture": sum(entry["disposition"] == "capture" for entry in plan["entries"]),
                      "localOnly": sum(entry["disposition"] == "local-only" for entry in plan["entries"]),
                      "terminalAuthorities": len(plan["terminalSources"]),
                      "inertBootstrap": sum(entry["disposition"] == "inert-bootstrap" for entry in plan["entries"]),
                      "blockers": plan["blockers"]}, sort_keys=True))
    return 2 if plan["blockers"] else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        # HTTP/library errors may contain credentials or private server data.
        print("capture_stopped_preserve_evidence; inspect plan/intents/receipts; use --resume only to explicitly retry the same frozen migration", file=sys.stderr)
        sys.exit(1)
