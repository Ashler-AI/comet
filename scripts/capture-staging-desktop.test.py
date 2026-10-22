#!/usr/bin/env python3
"""Offline only: python3 scripts/capture-staging-desktop.test.py (no network)."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("capture", Path(__file__).with_name("capture-staging-desktop.py"))
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)
OWNER = "owner@example.test"
CHAT = "00000000-0000-0000-0000-000000000001"


def scope(doc_id, deployment=None, owner=OWNER):
    return {"project": capture.PROJECT, "principal": owner, "deployment": deployment, "session": doc_id}


def database(root, rows=None):
    path = root / "copied.sqlite3"
    with contextlib.closing(sqlite3.connect(path)) as db, db:
        db.executescript("CREATE TABLE document_scopes(doc_id TEXT PRIMARY KEY,scope TEXT); CREATE TABLE snapshots(doc_id TEXT PRIMARY KEY,bytes BLOB);")
        rows = rows or {CHAT: scope(CHAT), "workspace3": scope("workspace3", owner="other-project-collaborator")}
        for doc_id, value in rows.items():
            db.execute("INSERT INTO document_scopes VALUES (?,?)", (doc_id, json.dumps(value)))
            db.execute("INSERT INTO snapshots VALUES (?,?)", (doc_id, b"unchanged-cache"))
    return path


def directories(root):
    capture.private_directory(root)
    for name in ("intents", "receipts", "parts", "raw", "raw/sessions"):
        capture.private_directory(root / name)


class Stream:
    """A large valid JSON envelope, generated without materializing its body."""
    def __init__(self, prefix, padding, suffix, headers=None):
        self.prefix, self.padding, self.suffix = prefix, padding, suffix
        self.position = 0
        self.headers = headers or {}
        self.total = len(prefix) + padding + len(suffix)

    def getheader(self, name, default=None):
        return self.headers.get(name, default)

    def read(self, size):
        assert 0 < size <= capture.CHUNK, "unbounded body read"
        result = bytearray()
        while len(result) < size and self.position < self.total:
            if self.position < len(self.prefix):
                part = self.prefix[self.position:self.position + size - len(result)]
            elif self.position < len(self.prefix) + self.padding:
                part = b" " * min(size - len(result), len(self.prefix) + self.padding - self.position)
            else:
                offset = self.position - len(self.prefix) - self.padding
                part = self.suffix[offset:offset + size - len(result)]
            self.position += len(part)
            result.extend(part)
        return bytes(result)


def export(entry, migration, padding=0):
    authority = ({"projectId": capture.PROJECT, "workspaceId": "ws4/" + capture.PROJECT,
                  "authority": "project", "exportedBySubject": "original-collaborator"}
                 if entry["generation"] == "workspace" else
                 {"ownerSubject": OWNER, "projectId": capture.PROJECT,
                  "deploymentId": entry["sourceScope"]["deployment"], "logicalSessionId": entry["id"]})
    header = {"schemaVersion": 1, "format": capture.FORMAT, "sourceGeneration": entry["generation"],
              "migrationId": migration, "scope": authority, "frozenAt": "2026-10-21T00:00:00.000Z"}
    prefix = json.dumps(header, separators=(",", ":")).encode()[:-1] + b',"meta":['
    suffix = b'],"blobs":[],"updates":[]}'
    stream = Stream(prefix, padding, suffix)
    digest = hashlib.sha256()
    while chunk := stream.read(capture.CHUNK):
        digest.update(chunk)
    receipt = {"version": 1, "format": capture.FORMAT, "sourceGeneration": entry["generation"],
               "migrationId": migration, "scope": authority, "frozen": True, "frozenAt": header["frozenAt"],
               "sha256": digest.hexdigest(), "bytes": stream.total}
    return receipt, (prefix, padding, suffix)


class FakeApi:
    def __init__(self, plan, fail=False, padding=0):
        self.calls, self.exports, self.fail = [], {}, fail
        self.encoding = None
        self.corrupt = False
        for entry in plan["entries"]:
            if entry["disposition"] == "capture":
                migration = plan["reuseReceipts"].get(entry["id"], {}).get("migrationId", plan["migrationId"])
                self.exports[entry["path"]] = export(entry, migration, padding)

    def owner(self):
        self.calls.append(("IDENTITY", None))
        return OWNER

    def json(self, origin, path, body=None):
        assert origin == capture.EDGE
        self.calls.append(("POST", path, body))
        if self.fail:
            self.fail = False
            raise OSError("lost response after durable freeze")
        receipt = self.exports[path][0]
        assert body == {"migrationId": receipt["migrationId"]}
        return receipt

    @contextlib.contextmanager
    def response(self, origin, path):
        assert origin == capture.EDGE
        self.calls.append(("GET", path))
        for route, (receipt, parts) in self.exports.items():
            if path == route + ("&" if "?" in route else "?") + "migrationId=" + receipt["migrationId"]:
                headers = {"Content-Length": str(receipt["bytes"]), "x-crew-migration-sha256": receipt["sha256"]}
                if self.encoding:
                    headers["Content-Encoding"] = self.encoding
                if self.corrupt:
                    parts = (parts[0], parts[1], b"X" + parts[2][1:])
                yield Stream(*parts, headers)
                return
        raise AssertionError("wrong exact migration route")


class CaptureTest(unittest.TestCase):
    def test_inventory_is_complete_deterministic_and_preserves_unknown_scopes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            rows = {str(__import__("uuid").UUID(int=i + 1)): scope(str(__import__("uuid").UUID(int=i + 1)), None if i < 1037 else capture.PROJECT)
                    for i in range(1048)}
            rows["workspace3"] = scope("workspace3", owner="different-workspace-attribution")
            rows["local-chat-native-1"] = scope("local-chat-native-1")
            alias = CHAT + "::session::" + CHAT
            rows[alias] = scope(CHAT)
            path = database(root, rows)
            plan = capture.inventory(path, OWNER, "batch")
            self.assertEqual(plan, capture.inventory(path, OWNER, "batch"))
            self.assertEqual(len(plan["entries"]), 1051)
            self.assertEqual(sum(row["disposition"] == "capture" for row in plan["entries"]), 1049)
            self.assertEqual(plan["blockers"], [])
            self.assertEqual(sum("unscoped=true" in row.get("path", "") for row in plan["entries"]), 1037)
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute("INSERT INTO document_scopes VALUES (?,?)", ("unknown", json.dumps(scope("unknown"))))
                db.execute("INSERT INTO document_scopes VALUES (?,?)", ("foreign", json.dumps(scope("foreign", owner="foreign"))))
                db.execute("INSERT INTO snapshots VALUES ('unbound',X'01')")
            blocked = capture.inventory(path, OWNER, "batch")
            self.assertEqual({row["id"] for row in blocked["blockers"]}, {"unknown", "foreign", "unbound"})
            api = FakeApi(blocked)
            with self.assertRaisesRegex(ValueError, "blockers"):
                capture.capture(api, blocked, root)
            self.assertEqual(api.calls, [])

    def test_only_invoked_native_hash_bound_inert_classification_waives_unknown_scope(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = database(root)
            inert_id, snapshot = "bootstrap-native-classifier-evidence", b"opaque-native-snapshot"
            with contextlib.closing(sqlite3.connect(source)) as db, db:
                db.execute("INSERT INTO snapshots VALUES (?,?)", (inert_id, snapshot))
            unclassified = capture.inventory(source, OWNER, "batch")
            self.assertEqual(unclassified["blockers"], [{"id": inert_id, "reason": "snapshot_without_scope"}])
            snapshot_sha256 = hashlib.sha256(snapshot).hexdigest()
            self.assertEqual(unclassified["unscopedSnapshots"][inert_id], {"sha256": snapshot_sha256, "bytes": len(snapshot)})
            report = {"version": 1, "sourceDatabaseSha256": unclassified["databaseSha256"], "inertBootstrapSnapshots": {
                inert_id: {"snapshotSha256": snapshot_sha256, "disposition": "inert-bootstrap"}}}
            binary = root / "native-binary"
            binary.write_bytes(b"native executable identity; subprocess is mocked in this contract test")
            native_root = root / "copied-native"
            native_root.mkdir()
            result = capture.subprocess.CompletedProcess([], 0, json.dumps(report).encode())
            with patch.object(capture.subprocess, "run", return_value=result) as run:
                classification = capture.inspect_native(binary, source, [native_root], root)
            self.assertEqual(run.call_args.args[0], [str(binary), "inspect-backup", "--source-backup-root", str(root.resolve()),
                             "--database-relative", source.name, "--writers-stopped", "--native-history-root", str(native_root)])
            self.assertEqual(run.call_args.kwargs["stdin"], capture.subprocess.DEVNULL)
            plan = capture.inventory(source, OWNER, "batch", classification=classification)
            self.assertEqual(plan["blockers"], [])
            evidence = next(entry for entry in plan["entries"] if entry["id"] == inert_id)
            self.assertEqual(evidence, {"id": inert_id, "sourceScope": None, "disposition": "inert-bootstrap",
                                       "snapshotSha256": snapshot_sha256, "bytes": len(snapshot)})
            output = root / "capture"
            directories(output)
            api = FakeApi(plan)
            capture.capture(api, plan, output)
            self.assertFalse(any(inert_id in str(call) for call in api.calls))
            self.assertNotIn(inert_id, capture.load(output / "context.json")["sourceWitnesses"])
            altered = json.loads(json.dumps(classification))
            altered["report"]["sourceDatabaseSha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "database_mismatch"):
                capture.inventory(source, OWNER, "batch", classification=altered)
            altered = json.loads(json.dumps(classification))
            altered["report"]["inertBootstrapSnapshots"][inert_id]["snapshotSha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "snapshot_mismatch"):
                capture.inventory(source, OWNER, "batch", classification=altered)
            with contextlib.closing(sqlite3.connect(source)) as db, db:
                db.execute("INSERT INTO snapshots VALUES ('unknown-history',X'01')")
            with self.assertRaisesRegex(ValueError, "database_mismatch"):
                capture.inventory(source, OWNER, "batch", classification=classification)
            classification["report"]["sourceDatabaseSha256"] = capture.hash_file(source)[0]
            still_blocked = capture.inventory(source, OWNER, "batch", classification=classification)
            self.assertEqual(still_blocked["blockers"], [{"id": "unknown-history", "reason": "snapshot_without_scope"}])

    def test_default_cli_dryrun_has_no_remote_writes_and_no_raw_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = database(root)
            auth = root / "auth"
            auth.write_text("private-bearer")
            plan = capture.inventory(path, OWNER, "batch")
            api = FakeApi(plan)
            output = root / "capture"
            with patch.object(capture, "Api", return_value=api), contextlib.redirect_stdout(io.StringIO()):
                result = capture.main(["--database", str(path), "--auth-file", str(auth), "--output", str(output), "--migration-id", "batch"])
            self.assertEqual(result, 0)
            self.assertEqual(api.calls, [("IDENTITY", None)])
            self.assertFalse((output / "raw").exists())
            self.assertEqual(capture.load(output / "plan.json"), plan)

    def test_ambiguous_freeze_requires_explicit_same_id_resume(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = capture.inventory(database(root), OWNER, "one-batch")
            output = root / "capture"
            directories(output)
            api = FakeApi(plan, fail=True)
            with self.assertRaises(OSError):
                capture.capture(api, plan, output)
            with self.assertRaisesRegex(ValueError, "explicit_resume"):
                capture.capture(api, plan, output)
            self.assertEqual(len(api.calls), 1)
            capture.capture(api, plan, output, resume=True)
            posts = [call for call in api.calls if call[0] == "POST"]
            self.assertTrue(all(call[2] == {"migrationId": "one-batch"} for call in posts))
            first_get = next(i for i, call in enumerate(api.calls) if call[0] == "GET")
            self.assertFalse(any(call[0] == "POST" for call in api.calls[first_get:]))
            calls = list(api.calls)
            capture.capture(api, plan, output, resume=True)
            self.assertEqual(api.calls, calls)
            witness = capture.load(output / "context.json")["sourceWitnesses"]
            self.assertEqual(set(witness), {CHAT, "workspace3"})
            for file in output.rglob("*"):
                if file.is_file():
                    self.assertEqual(file.stat().st_mode & 0o777, 0o600)
            self.assertEqual(capture.hash_file(output / "raw" / "workspace.json")[0], witness["workspace3"]["sha256"])

    def test_existing_s4_receipt_never_refreezes_or_relabels(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = database(root, {CHAT: scope(CHAT, capture.PROJECT), "workspace3": scope("workspace3")})
            original = capture.inventory(path, OWNER, "batch")
            entry = next(row for row in original["entries"] if row["id"] == CHAT)
            receipt, _ = export(entry, "cloud-original-token")
            plan = capture.inventory(path, OWNER, "batch", {CHAT: receipt})
            output = root / "capture"
            directories(output)
            api = FakeApi(plan)
            capture.capture(api, plan, output)
            self.assertFalse(any(call[0] == "POST" and call[1] == entry["path"] for call in api.calls))
            self.assertTrue(any(call[0] == "GET" and "migrationId=cloud-original-token" in call[1] for call in api.calls))
            witness = capture.load(output / "context.json")["sourceWitnesses"]
            self.assertEqual(witness[CHAT]["migrationId"], "cloud-original-token")
            self.assertEqual(witness["workspace3"]["migrationId"], "batch")
            receipt["scope"]["ownerSubject"] = "foreign"
            with self.assertRaisesRegex(ValueError, "authority"):
                capture.inventory(path, OWNER, "batch", {CHAT: receipt})

    def test_cloud_plan_is_selector_only_and_uses_authenticated_original_receipt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = database(root, {CHAT: scope(CHAT, capture.PROJECT), "workspace3": scope("workspace3")})
            cloud = {"records": [{"id": "sandbox-a", "disposition": "requires-fenced-import", "coordinatorLifecycleEpoch": 8,
                "sourceDisposition": "terminal-original-epoch-candidate", "scope": {
                "sandboxId": "sandbox-a", "ownerSubject": OWNER, "projectId": capture.PROJECT,
                "deploymentId": capture.PROJECT, "logicalSessionId": CHAT, "deviceId": "comet-scaffold-sandbox-a", "lifecycleEpoch": 7}}]}
            selectors = capture.cloud_selectors(cloud, OWNER)
            plan = capture.inventory(source, OWNER, "desktop-batch", cloud=selectors, terminal=capture.terminal_selectors(cloud))
            entry = next(row for row in plan["entries"] if row["id"] == CHAT)
            receipt, parts = export(entry, "actual-cloud-claim")
            authority = {"version": 1, "kind": "terminal-source-authority", "scope": cloud["records"][0]["scope"], "coordinatorLifecycleEpoch": 8,
                         "migrationId": receipt["migrationId"], "sourceSha256": receipt["sha256"], "terminal": {
                             "status": "stopped", "providerAction": "kill", "terminatedAt": "2026-10-20T00:00:00Z",
                             "terminationReason": "timeout", "physicalSandboxId": "sandbox-a",
                             "providerNotFoundObservedAt": "2026-10-20T01:00:00Z", "retainedSnapshotIds": []}}
            authority_bytes = (json.dumps(authority, indent=3) + "\n").encode()
            class CoordinatorApi(FakeApi):
                def json(self, origin, path, body=None):
                    if origin == capture.CONTROL:
                        assert path == "/api/sessions/sandbox-a/crew-migration/source-receipt" and body is None
                        self.calls.append(("GET-RECEIPT", path))
                        return receipt
                    return super().json(origin, path, body)

                @contextlib.contextmanager
                def response(self, origin, path):
                    if origin == capture.CONTROL and path.endswith("/terminal-source-authority"):
                        assert path == "/api/sessions/sandbox-a/crew-migration/terminal-source-authority"
                        self.calls.append(("GET-TERMINAL", path))
                        yield io.BytesIO(authority_bytes)
                    elif origin == capture.CONTROL:
                        assert path == "/api/sessions/sandbox-a/crew-migration/source"
                        self.calls.append(("GET", path))
                        yield Stream(*parts, {"Content-Length": str(receipt["bytes"]), "x-crew-migration-sha256": receipt["sha256"]})
                    else:
                        with super().response(origin, path) as response:
                            yield response
            output = root / "capture"
            directories(output)
            api = CoordinatorApi(plan)
            capture.capture(api, plan, output)
            self.assertEqual(capture.load(output / "context.json")["sourceWitnesses"][CHAT],
                             {"migrationId": "actual-cloud-claim", "sha256": receipt["sha256"]})
            self.assertFalse((output / "intents" / (CHAT + ".json")).exists())
            self.assertFalse(any(call[0] == "POST" and call[1] == entry["path"] for call in api.calls))
            self.assertEqual(capture.hash_file(output / "raw" / entry["file"]), (receipt["sha256"], receipt["bytes"]))
            retained = output / "terminal-authority" / "sessions" / (CHAT + ".json")
            self.assertEqual(retained.read_bytes(), authority_bytes)
            self.assertEqual(capture.load(output / "complete.json")["terminalAuthoritySha256"][CHAT],
                             hashlib.sha256(authority_bytes).hexdigest())
            self.assertEqual(plan["terminalSources"][CHAT][0]["scope"]["lifecycleEpoch"], 7)
            self.assertEqual(plan["terminalSources"][CHAT][0]["coordinatorLifecycleEpoch"], 8)
            for fence in (None, 7, True):
                invalid_authority = {**authority, "coordinatorLifecycleEpoch": fence}
                if fence is None:
                    del invalid_authority["coordinatorLifecycleEpoch"]
                authority_bytes = json.dumps(invalid_authority).encode()
                rejected_epoch = root / ("rejected-epoch-" + str(fence))
                directories(rejected_epoch)
                with self.assertRaisesRegex(ValueError, "terminal_source_authority_mismatch"):
                    capture.capture(api, plan, rejected_epoch)
                self.assertFalse((rejected_epoch / "complete.json").exists())
            rejected = root / "rejected-terminal"
            directories(rejected)
            authority_bytes = json.dumps({**authority, "migrationId": "wrong-original-epoch"}).encode()
            with self.assertRaisesRegex(ValueError, "terminal_source_authority_mismatch"):
                capture.capture(api, plan, rejected)
            self.assertFalse((rejected / "complete.json").exists())
            self.assertFalse((rejected / "terminal-authority" / "sessions" / (CHAT + ".json")).exists())
            duplicate = {"id": "sandbox-b", "disposition": "requires-fenced-import", "coordinatorLifecycleEpoch": 10,
                         "scope": {**cloud["records"][0]["scope"], "sandboxId": "sandbox-b", "deviceId": "comet-scaffold-sandbox-b-e9", "lifecycleEpoch": 9}}
            cloud["records"].append(duplicate)
            with self.assertRaisesRegex(ValueError, "duplicate_cloud_logical_identity"):
                capture.cloud_selectors(cloud, OWNER)
            # An exact prior receipt selects the frozen source, not the newest
            # physical record. Its authenticated bytes still have to match.
            self.assertEqual(capture.cloud_selectors(cloud, OWNER, {CHAT: receipt}), {})
            resolved = capture.inventory(source, OWNER, "desktop-batch", {CHAT: receipt},
                                         capture.cloud_selectors(cloud, OWNER, {CHAT: receipt}), capture.terminal_selectors(cloud))
            self.assertEqual(resolved["reuseReceipts"][CHAT]["migrationId"], "actual-cloud-claim")
            self.assertEqual(resolved["cloudSources"], {})
            self.assertEqual(resolved["blockers"], [{"id": CHAT, "reason": "terminal_authority_ambiguous_physical_epoch"}])
            before_calls = list(api.calls)
            with self.assertRaisesRegex(ValueError, "blockers"):
                capture.capture(api, resolved, output)
            self.assertEqual(api.calls, before_calls)
            cloud["records"][0]["scope"]["ownerSubject"] = "foreign"
            with self.assertRaisesRegex(ValueError, "foreign"):
                capture.cloud_selectors(cloud, OWNER)

    def test_fresh_source_identity_never_asserts_native_deployment(self):
        api = capture.Api("private-token")
        cp = {"ok": True, "resource": capture.CONTROL, "actor": {"sub": OWNER.upper(), "auth": "iap"},
              "scopes": ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:lifecycle"]}
        edge = {"credential": "scaffold", "userId": OWNER, "projectScope": capture.PROJECT,
                "capabilities": ["session.read", "session.chat", "session.control", "session.annotate",
                                 "session.invite", "session.files", "session.environment"]}
        with patch.object(api, "json", side_effect=[cp, edge]):
            self.assertEqual(api.owner(), OWNER)
        edge["deploymentId"] = "native-target"
        with patch.object(api, "json", side_effect=[cp, edge]):
            with self.assertRaisesRegex(ValueError, "legacy_source"):
                api.owner()
        cp["scopes"].remove("remote_code:write")
        with patch.object(api, "json", return_value=cp):
            with self.assertRaisesRegex(ValueError, "verified_owner"):
                api.owner()

    def test_large_export_streams_and_bad_encoding_or_hash_never_publishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan = capture.inventory(database(root), OWNER, "batch")
            entry = next(row for row in plan["entries"] if row["id"] == CHAT)
            output = root / "capture"
            directories(output)
            api = FakeApi(plan, padding=193 * 1024 * 1024)
            receipt = api.exports[entry["path"]][0]
            capture.download(api, entry, plan, receipt, output)
            self.assertEqual(capture.hash_file(output / "raw" / entry["file"]), (receipt["sha256"], receipt["bytes"]))
            workspace = next(row for row in plan["entries"] if row["id"] == "workspace3")
            receipt = api.exports[workspace["path"]][0]
            api.encoding = "gzip"
            with self.assertRaisesRegex(ValueError, "encoded"):
                capture.download(api, workspace, plan, receipt, output)
            api.encoding = None
            api.corrupt = True
            with self.assertRaisesRegex(ValueError, "bytes_mismatch"):
                capture.download(api, workspace, plan, receipt, output)
            self.assertFalse((output / "raw" / "workspace.json").exists())

    def test_bridge_is_fixed_loopback_and_requires_real_health_and_owner(self):
        api = capture.Api("private-token", via_staging_bridge=True)
        cp = {"ok": True, "resource": capture.CONTROL, "actor": {"sub": OWNER, "auth": "iap"},
              "scopes": ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec"]}
        edge = {"credential": "scaffold", "userId": OWNER, "projectScope": capture.PROJECT,
                "capabilities": ["session.read", "session.chat", "session.control", "session.annotate",
                                 "session.invite", "session.files", "session.environment"]}
        with self.assertRaisesRegex(ValueError, "bridge_not_verified"):
            with api.response(capture.CONTROL, "/api/sessions/sandbox/crew-migration/source"):
                self.fail("unverified bridge used")
        with patch.object(api, "json", side_effect=[cp, {"service": "wrong", "workerConfigured": True}]):
            with self.assertRaisesRegex(ValueError, "bridge_not_configured"):
                api.owner()
        health = {"service": "scaffold-edge-bridge", "workerConfigured": True}
        wrong_owner = {**cp, "actor": {"auth": "iap", "sub": "foreign"}}
        with patch.object(api, "json", side_effect=[cp, health, wrong_owner]):
            with self.assertRaisesRegex(ValueError, "bridge_owner_mismatch"):
                api.owner()
        with patch.object(api, "json", side_effect=[cp, health, cp, edge]) as calls:
            self.assertEqual(api.owner(), OWNER)
            self.assertTrue(calls.call_args_list[0].kwargs["canonical"])
            self.assertEqual([call.args[:2] for call in calls.call_args_list], [
                (capture.CONTROL, "/api/code-sandboxes/auth/session"), (capture.CONTROL, "/health"),
                (capture.CONTROL, "/api/code-sandboxes/auth/session"), (capture.EDGE, "/auth/session")])
        routes = []
        class Connection:
            def __init__(self, host, port=None, timeout=None):
                routes.append((host, port))
            def request(self, method, path, body, headers):
                assert method == "GET" and body is None and "private-token" not in path
                assert headers["Authorization"] == "Bearer private-token"
                assert headers["Accept-Encoding"] == "identity"
            def getresponse(self):
                return type("Response", (), {"status": 200, "getheader": lambda self, name, default=None: default})()
            def close(self):
                pass
        with patch.object(capture.http.client, "HTTPConnection", Connection), patch.object(capture.http.client, "HTTPSConnection", Connection):
            with api.response(capture.CONTROL, "/api/sessions/sandbox/crew-migration/source"):
                pass
            with api.response(capture.EDGE, "/auth/session"):
                pass
            with api.response(capture.CONTROL, "/api/code-sandboxes/auth/session", canonical=True):
                pass
        self.assertEqual(routes, [("127.0.0.1", 18081), ("comet-staging.internal.ashler.com", None),
                                  ("scaffold-staging.internal.ashler.com", None)])

    def test_tokens_stay_headers_and_redirects_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "auth"
            for value in ("secret", '{"accessToken":"secret"}', '{"access_token":"secret"}'):
                path.write_text(value)
                self.assertEqual(capture.token_from_file(path), "secret")
            path.write_text('{"accessToken":"secret","resource":"https://foreign.test"}')
            with self.assertRaisesRegex(ValueError, "not_staging"):
                capture.token_from_file(path)
        class Connection:
            def __init__(self, host, timeout):
                assert host == "comet-staging.internal.ashler.com"
            def request(self, method, path, body, headers):
                assert "secret" not in path and body is None
                assert headers["Authorization"] == "Bearer secret"
                assert headers["Accept-Encoding"] == "identity"
            def getresponse(self):
                return type("Redirect", (), {"status": 302})()
            def close(self):
                pass
        with patch.object(capture.http.client, "HTTPSConnection", Connection):
            with self.assertRaisesRegex(ValueError, "http_302"):
                with capture.Api("secret").response(capture.EDGE, "/auth/session"):
                    self.fail("redirect accepted")


if __name__ == "__main__":
    unittest.main()
