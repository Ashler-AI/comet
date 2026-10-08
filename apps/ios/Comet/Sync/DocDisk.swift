// On-device Loro doc persistence — the old mobile app's snapshot cache
// (kv.ts/loro-room.ts) and the engine's DocsStore, in file form: one snapshot
// per doc under Application Support. Docs load BEFORE the room join, so the
// UI renders instantly from local state (offline included) and the join's
// version vector turns the backfill incremental instead of a full snapshot.

import Foundation
import Loro
import os

enum DocDisk {
    /// Identity-local navigation metadata, never execution or lifecycle authority.
    struct BrowsePointer: Codable, Hashable {
        var projection: SessionRoomProjection
        var sandboxId: String?
        var scope: CollaborationScope {
            CollaborationScope(projectId: projection.projectId,
                deploymentId: projection.deploymentId, sessionId: projection.sessionId)
        }
    }
    struct RecordIntent: Codable {
        var id = UUID().uuidString.lowercased()
        var root: String
        var key: String
        var before: Data?
        var after: Data?
        var version: Data?
        var intermediates: [Data?] = []
        var index: String { root + ":" + key }

        enum CodingKeys: String, CodingKey { case id, root, key, before, after, version, intermediates }
        init(root: String, key: String, before: Data?) {
            self.root = root; self.key = key; self.before = before
            after = nil; version = nil
        }
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            id = try c.decode(String.self, forKey: .id)
            root = try c.decode(String.self, forKey: .root); key = try c.decode(String.self, forKey: .key)
            before = try c.decodeIfPresent(Data.self, forKey: .before)
            after = try c.decodeIfPresent(Data.self, forKey: .after)
            version = try c.decodeIfPresent(Data.self, forKey: .version)
            intermediates = try c.decodeIfPresent([Data?].self, forKey: .intermediates) ?? []
        }
    }

    static let workspaceIdentityFields = ["id", "chatId", "userId", "deviceId", "ownerSubject", "ownerDeviceId", "ownerPrincipal", "projectId", "deploymentId"]

    /// A membership may learn its first route; known scopes and owners cannot change.
    static func workspaceRoutesCompatible(root: String, rows: [[String: LoroValue]]) -> Bool {
        guard let first = rows.first else { return true }
        func route(_ row: [String: LoroValue], _ field: String) -> LoroValue? {
            let value = row["environment"]?.mapValue?[field]
            return value == .null ? nil : value
        }
        for field in ["scope", "ownerPrincipal"] {
            let baseline = route(first, field)
            var known = baseline
            for row in rows.dropFirst() {
                let current = route(row, field)
                if root != "sessionRefs" || baseline != nil {
                    if current != baseline { return false }
                } else if let current {
                    if let known, known != current { return false }
                    known = current
                }
            }
        }
        return true
    }

    private static func selfSessionId(_ id: String) -> Substring? {
        let bytes = id.utf8
        let first = bytes.prefix(36)
        guard first.count == 36, bytes.count > 36,
              first.enumerated().allSatisfy({ index, byte in
                  (index == 8 || index == 13 || index == 18 || index == 23) ? byte == 45 : (48...57).contains(byte) || (97...102).contains(byte)
              }) else { return nil }
        var rest = bytes.dropFirst(36)
        while !rest.isEmpty {
            guard rest.starts(with: "::session::".utf8) else { return nil }
            rest = rest.dropFirst(11)
            guard rest.starts(with: first) else { return nil }
            rest = rest.dropFirst(36)
        }
        return id[..<id.index(id.startIndex, offsetBy: 36)]
    }

    static func normalizeWorkspaceIntent(_ original: RecordIntent) throws -> RecordIntent {
        guard ["chats", "sessions", "sessionRefs", "worktreeDeletions"].contains(original.root) else { return original }
        let identity = original.root == "chats" ? "id" : "chatId"
        let values = try ([original.before] + original.intermediates + [original.after]).map { try recordValue($0) }
        guard let base = values.compactMap({ $0?.mapValue }).first, base[identity]?.stringValue != nil else { return original }
        let user = base["userId"]?.stringValue
        var keyId = original.key
        if original.root == "sessionRefs" {
            guard let user, keyId.hasPrefix("\(user.utf8.count):\(user):") else {
                throw MobileSessionError.unavailable("Crew cannot recover membership for a different principal.")
            }
            keyId = String(keyId.dropFirst("\(user.utf8.count):\(user):".count))
        }
        guard let publicId = selfSessionId(keyId) ?? values.compactMap({ $0?.mapValue?[identity]?.stringValue }).compactMap({ selfSessionId($0) }).first else { return original }
        guard keyId == publicId || selfSessionId(keyId) == publicId else { throw MobileSessionError.unavailable("Crew retained a conflicting session identity.") }
        let owners = original.root == "sessionRefs" ? ["userId"] : original.root == "worktreeDeletions" ? ["ownerSubject", "ownerDeviceId"] : ["deviceId"]
        guard owners.allSatisfy({ !(base[$0]?.stringValue ?? "").isEmpty }) else { throw MobileSessionError.unavailable("Crew retained an unowned session alias.") }
        for value in values.compactMap({ $0 }) {
            guard let row = value.mapValue, let id = row[identity]?.stringValue, id == publicId || selfSessionId(id) == publicId,
                  workspaceIdentityFields.filter({ $0 != identity }).allSatisfy({ row[$0] == base[$0] }) else {
                throw MobileSessionError.unavailable("Crew cannot migrate a foreign session identity; original edits are retained.")
            }
        }
        guard workspaceRoutesCompatible(root: original.root, rows: values.compactMap({ $0?.mapValue })) else {
            throw MobileSessionError.unavailable("Crew cannot migrate a conflicting membership route; original edits are retained.")
        }
        let canonicalId = String(publicId)
        func normalize(_ bytes: Data?) throws -> Data? {
            guard var row = try recordValue(bytes)?.mapValue else { return bytes }
            row[identity] = .string(value: canonicalId)
            return try recordData(.map(value: row))
        }
        var intent = original
        intent.key = original.root == "sessionRefs" ? "\(user!.utf8.count):\(user!):\(canonicalId)" : canonicalId
        intent.before = try normalize(original.before); intent.after = try normalize(original.after)
        intent.intermediates = try original.intermediates.map { try normalize($0) }
        return intent
    }

    static func canonicalCreationCoversAlias(_ alias: RecordIntent, canonical: RecordIntent, cached: LoroDoc) throws -> Bool {
        guard alias.root == canonical.root, alias.key != canonical.key, alias.before == nil, canonical.before == nil,
              alias.intermediates.isEmpty, canonical.intermediates.isEmpty,
              let aliasBytes = alias.version, let canonicalBytes = canonical.version,
              let aliasVersion = try? VersionVector.decode(bytes: aliasBytes),
              let canonicalVersion = try? VersionVector.decode(bytes: canonicalBytes),
              canonicalVersion.includesVv(other: aliasVersion),
              canonicalVersion.includesVv(other: cached.oplogVv()), cached.oplogVv().includesVv(other: canonicalVersion) else { return false }
        let local = try normalizeWorkspaceIntent(alias), target = try normalizeWorkspaceIntent(canonical)
        guard local.key == target.key, target.key == canonical.key else { return false }
        for (original, normalized) in [(alias, local), (canonical, target)] {
            var evidence = RecordIntent(root: original.root, key: original.key, before: nil)
            evidence.after = try recordData(recordValue(in: cached, root: original.root, key: original.key))
            guard try recordValue(normalizeWorkspaceIntent(evidence).after) == recordValue(normalized.after),
                  cached.getMap(id: original.root).getLastEditor(key: original.key) != nil else { return false }
        }
        if target.after == nil { return true }
        guard let local = try recordValue(local.after)?.mapValue, let target = try recordValue(target.after)?.mapValue else { return false }
        let observations: Set<String>
        switch alias.root {
        case "chats": observations = ["createdAt", "harnessSessionId", "harnessSessionCwd", "lastSeenAt", "lastMessageAt", "lastMessagePreview"]
        case "sessions": observations = ["status", "startedAt", "updatedAt"]
        case "sessionRefs": observations = ["addedAt"]
        default: return false
        }
        guard Set(local.keys).union(target.keys).allSatisfy({ local[$0] == target[$0] || observations.contains($0) }) else { return false }
        for clock in alias.root == "sessions" ? ["updatedAt"] : ["lastSeenAt", "lastMessageAt"] {
            if let localClock = local[clock]?.i64Value, target[clock]?.i64Value.map({ $0 >= localClock }) != true { return false }
        }
        if alias.root == "sessions", local["updatedAt"] == target["updatedAt"], ["status", "startedAt"].contains(where: { local[$0] != target[$0] }) { return false }
        if alias.root == "chats", local["lastMessageAt"] == target["lastMessageAt"], local["lastMessagePreview"] != target["lastMessagePreview"] { return false }
        return true
    }

    /// Work on a fork before binding it: failures leave the cache and its evidence intact.
    static func migrateWorkspaceRows(in doc: LoroDoc) throws -> Bool {
        var changed = false
        for root in ["chats", "sessions", "sessionRefs", "worktreeDeletions"] {
            let map = doc.getMap(id: root)
            for (key, value) in map.getDeepValue().mapValue ?? [:] {
                var original = RecordIntent(root: root, key: key, before: nil)
                original.after = try recordData(value)
                let normalized = try normalizeWorkspaceIntent(original)
                guard normalized.key != key || normalized.after != original.after else { continue }
                guard isRecoveryValue(value), let fields = try recordValue(normalized.after)?.mapValue else { continue }
                var targetIntent = RecordIntent(root: root, key: normalized.key, before: nil)
                targetIntent.after = try recordData(recordValue(in: doc, root: root, key: normalized.key))
                let remote = try recordValue(normalizeWorkspaceIntent(targetIntent).after)?.mapValue
                if normalized.key != key, let remote {
                    guard workspaceIdentityFields.allSatisfy({ remote[$0] == fields[$0] }),
                          workspaceRoutesCompatible(root: root, rows: [remote, fields]) else {
                        throw MobileSessionError.unavailable("Crew cannot merge a foreign canonical workspace record.")
                    }
                }
                let deleted = map.get(key: normalized.key) == nil && map.getLastEditor(key: normalized.key) != nil
                if !deleted {
                    let row = try map.getOrCreateContainer(key: normalized.key, child: LoroMap())
                    let newerStatus = root == "sessions" && (fields["updatedAt"]?.i64Value ?? Int64.min) > (remote?["updatedAt"]?.i64Value ?? Int64.min)
                    for (field, value) in fields where (key == normalized.key && field == (root == "chats" ? "id" : "chatId")) || (newerStatus && ["status", "startedAt", "updatedAt"].contains(field)) || (row.get(key: field) == nil && row.getLastEditor(key: field) == nil) {
                        try row.insert(key: field, v: value)
                    }
                }
                if key != normalized.key { try map.delete(key: key) }
                changed = true
            }
        }
        if changed { doc.commit() }
        return changed
    }

    static func recordValue(in doc: LoroDoc, root: String, key: String) throws -> LoroValue? {
        guard let item = doc.getMap(id: root).get(key: key) else { return nil }
        guard let value = item.asValue() ?? item.asLoroMap()?.getDeepValue() else {
            throw MobileSessionError.unavailable("Crew cannot reconcile the retained \(root) record \(key). Its original is retained.")
        }
        return value
    }

    static func recordData(_ value: LoroValue?) throws -> Data? {
        guard let value else { return nil }
        return try JSONSerialization.data(withJSONObject: value.jsonObject, options: [.sortedKeys, .fragmentsAllowed])
    }

    static func recordValue(_ bytes: Data?) throws -> LoroValue? {
        guard let bytes else { return nil }
        return LoroValue.fromJSON(try JSONSerialization.jsonObject(with: bytes, options: .fragmentsAllowed))
    }

    /// Replay logical records, never command lists or remote-owned identities.
    static func applyRecordChange(root: String, key: String, before: LoroValue?, after: LoroValue?, in doc: LoroDoc,
                                  alternatives: [LoroValue?] = []) throws {
        let server = try recordValue(in: doc, root: root, key: key)
        if let identity = after?.mapValue ?? before?.mapValue {
            let rows = ([before] + alternatives + [server, after]).compactMap({ $0?.mapValue })
            for row in rows {
                guard workspaceIdentityFields.allSatisfy({ row[$0] == identity[$0] }) else {
                    throw MobileSessionError.unavailable("Crew recovery cannot change retained record ownership or project/deployment identity.")
                }
            }
            guard workspaceRoutesCompatible(root: root, rows: rows) else {
                throw MobileSessionError.unavailable("Crew recovery cannot replace a known record owner or deployment route.")
            }
        }
        if server == after { return }
        var before = before
        if let desired = after?.mapValue, let remote = server?.mapValue, !alternatives.isEmpty {
            let bases = [before] + alternatives
            guard let match = bases.firstIndex(where: { value in
                let base = value?.mapValue ?? [:]
                return Set(base.keys).union(desired.keys).allSatisfy { field in
                    base[field] == desired[field] || remote[field] == desired[field] || remote[field] == base[field] ||
                    (root == "chats" && field == "lastSeenAt" && desired[field]?.i64Value != nil && remote[field]?.i64Value != nil)
                }
            }) else { throw MobileSessionError.unavailable("Crew recovery conflicts with retained intermediate edits on \(root)/\(key). Original intents are retained.") }
            before = bases[match]
        }
        let map = doc.getMap(id: root)
        if let desired = after?.mapValue {
            let base = before?.mapValue ?? [:]
            let remote = server?.mapValue ?? [:]
            guard (before == nil || before?.mapValue != nil),
                  (server == nil || server?.mapValue != nil),
                  before == nil || server != nil else {
                throw MobileSessionError.unavailable("Crew recovery conflicts with the authoritative \(root) record \(key), including a remote deletion. The original intent is retained.")
            }
            guard server != nil || map.getLastEditor(key: key) == nil else {
                throw MobileSessionError.unavailable("Crew recovery cannot revive a deleted record.")
            }
            let row = try map.getOrCreateContainer(key: key, child: LoroMap())
            // Legacy scalar records may become containers. Preserve every
            // remote field when doing so, not just this intent's changed fields.
            if row.getDeepValue().mapValue?.isEmpty == true, !remote.isEmpty {
                for (field, value) in remote { try row.insert(key: field, v: value) }
            }
            for field in Set(base.keys).union(desired.keys) where base[field] != desired[field] {
                if remote[field] == desired[field] { continue }
                if root == "chats", field == "lastSeenAt", let value = desired[field],
                   let desiredAt = value.i64Value, let serverAt = remote[field]?.i64Value {
                    if desiredAt > serverAt { try row.insert(key: field, v: value) }
                    continue
                }
                guard remote[field] == base[field], isRecoveryValue(desired[field]) else {
                    throw MobileSessionError.unavailable("Crew recovery conflicts on \(root)/\(key)/\(field). Both the original intent and authoritative record are retained.")
                }
                if let value = desired[field] { try row.insert(key: field, v: value) }
                else { try row.delete(key: field) }
            }
        } else {
            guard server == before, isRecoveryValue(after) else {
                throw MobileSessionError.unavailable("Crew recovery conflicts on \(root)/\(key). The original intent is retained.")
            }
            if let after { try map.insert(key: key, v: after) }
            else { try map.delete(key: key) }
        }
    }
    static var directory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory,
                                            in: .userDomainMask)[0]
            .appendingPathComponent("CometDocs", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    static func url(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("\(safe).loro")
    }

    static func intentURL(for id: String) -> URL {
        url(for: id).deletingPathExtension().appendingPathExtension("intents")
    }

    static func saveIntents<T: Encodable>(_ value: T, id: String) throws {
        let data = try JSONEncoder().encode(value)
        guard data.count <= 64 * 1024 * 1024 else {
            throw MobileSessionError.unavailable("The retained Crew intent journal exceeds its safe size limit; existing originals are retained.")
        }
        try data.write(to: intentURL(for: id), options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    static func loadIntents<T: Decodable>(_ type: T.Type, id: String) throws -> T? {
        let url = intentURL(for: id)
        guard FileManager.default.fileExists(atPath: url.path) else { return nil }
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        guard let size = attributes[.size] as? NSNumber, size.int64Value <= 64 * 1024 * 1024 else {
            throw MobileSessionError.unavailable("The retained Crew intent journal exceeds its safe size limit; the original is retained.")
        }
        return try JSONDecoder().decode(type, from: Data(contentsOf: url))
    }

    static func retainOutcome<T: Encodable>(_ value: T, id: String, commandId: String) throws {
        guard UUID(uuidString: commandId) != nil else {
            throw MobileSessionError.unavailable("Crew retained an invalid command identity.")
        }
        let url = intentURL(for: id).appendingPathExtension("\(commandId).outcome")
        try JSONEncoder().encode(value).write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }

    /// Retain legacy/blocked records before adopting an unrelated ancestry.
    static func retainRecoveryOriginal(doc: LoroDoc, id: String) throws {
        let url = self.url(for: id).appendingPathExtension("recovery")
        // The first original is the valuable one; later stale replicas must not
        // overwrite evidence or silently revive a legacy command.
        guard !FileManager.default.fileExists(atPath: url.path) else { return }
        try doc.export(mode: .snapshot).write(to: url, options: .atomic)
    }

    /// Import the saved snapshot, if any. Returns whether anything loaded.
    @discardableResult
    static func load(into doc: LoroDoc, id: String) -> Bool {
        guard let data = try? Data(contentsOf: url(for: id)), !data.isEmpty else { return false }
        guard let status = try? doc.importWith(bytes: data, origin: "disk") else { return false }
        return (status.pending?.isEmpty ?? true)
            && !doc.isDetached() && doc.stateVv() == doc.oplogVv()
    }

    /// Load into an isolated replica so cancellation or a failed import cannot
    /// mutate a store that has stopped, restarted, or switched deployments.
    static func loadReplica(id: String) -> LoroDoc? {
        guard !Task.isCancelled else { return nil }
        let replica = LoroDoc()
        guard load(into: replica, id: id), !Task.isCancelled else { return nil }
        return replica
    }

    /// Only a checksum-checked snapshot that materializes completely in an
    /// empty replica is eligible to replace a warm cache. An update envelope
    /// (even an independently importable one) is never a replacement snapshot.
    static func replacementSnapshot(bytes: Data) -> LoroDoc? {
        guard let metadata = try? decodeImportBlobMeta(bytes: bytes, checkChecksum: true)
        else { return nil }
        switch metadata.mode {
        case "snapshot", "shallow-snapshot", "outdated-snapshot":
            break
        default:
            return nil
        }
        let replacement = LoroDoc()
        guard let status = try? replacement.importWith(bytes: bytes, origin: "remote"),
              status.pending?.isEmpty ?? true,
              !replacement.isDetached(),
              replacement.stateVv() == replacement.oplogVv(),
              replacement.oplogVv().includesVv(other: metadata.partialEndVv)
        else { return nil }
        return replacement
    }

    /// The store calls this in the SAME main-actor turn as its binding swap.
    /// Commit/export here, not on the room actor: local edits may have landed
    /// while the validated server replica was waiting for the main actor.
    /// Prefer retaining operation identities. Across a shallow-history gap,
    /// reissue only a safely reconstructible local state delta as new operations.
    @MainActor
    static func preserveLocalOperations(from previous: LoroDoc, in replacement: LoroDoc) -> Bool {
        previous.commit()
        guard !previous.isDetached(), previous.stateVv() == previous.oplogVv(),
              !replacement.isDetached(), replacement.stateVv() == replacement.oplogVv()
        else { return false }
        let required = previous.oplogVv()
        let snapshotVersion = replacement.oplogVv()
        if snapshotVersion.includesVv(other: required) { return true }

        // An unsuccessful import can leave pending/partially imported operations.
        // Never let those contaminate the replica used for semantic replay.
        let merged = replacement.fork()
        do {
            let missing = try previous.export(mode: .updates(from: snapshotVersion))
            let status = try merged.importWith(bytes: missing, origin: "recovery")
            if status.pending?.isEmpty ?? true,
               !merged.isDetached(), merged.stateVv() == merged.oplogVv(),
               merged.oplogVv().includesVv(other: required) {
                return try importRecovery(from: merged, into: replacement, since: snapshotVersion)
            }
        } catch {
            // Compaction may have removed dependencies of otherwise valid local
            // operations. The isolated semantic path below does not need them.
        }
        do {
            return try rebaseLocalChanges(from: previous, into: replacement, since: snapshotVersion)
        } catch {
            roomLog.error("snapshot recovery could not rebase local changes: \(String(describing: error), privacy: .public)")
            return false
        }
    }

    /// Import only fully materialized recovery operations, preserving the entire
    /// server VV. In the semantic path these have fresh IDs: the old missing VV
    /// must NOT be advertised as received, since those operations were not imported.
    private static func importRecovery(
        from candidate: LoroDoc, into replacement: LoroDoc, since snapshotVersion: VersionVector
    ) throws -> Bool {
        guard !candidate.isDetached(), candidate.stateVv() == candidate.oplogVv(),
              candidate.oplogVv().includesVv(other: snapshotVersion)
        else { return false }
        let updates = try candidate.export(mode: .updates(from: snapshotVersion))
        let status = try replacement.importWith(bytes: updates, origin: "recovery")
        return (status.pending?.isEmpty ?? true)
            && !replacement.isDetached()
            && replacement.stateVv() == replacement.oplogVv()
            && replacement.oplogVv() == candidate.oplogVv()
    }

    private static func rebaseLocalChanges(
        from previous: LoroDoc, into replacement: LoroDoc, since snapshotVersion: VersionVector
    ) throws -> Bool {
        // The intersection is the local replica's last server-covered version,
        // not the server's current state (which contains changes absent locally).
        let localVersion = previous.oplogVv()
        let common = try VersionVector.decode(bytes: localVersion.encode())
        for (peer, span) in localVersion.diff(rhs: snapshotVersion).retreat {
            common.setEnd(id: Id(peer: peer, counter: span.start))
        }
        guard common.includesVv(other: previous.shallowSinceVv()) else { return false }

        // diff/checkout can change attachment/state in SDK versions. Work only
        // on forks, keeping the old binding usable if any step fails.
        let base = previous.fork()
        let baseFrontiers = base.vvToFrontiers(vv: common)
        guard let reconstructed = base.frontiersToVv(frontiers: baseFrontiers),
              reconstructed == common
        else { return false }
        // forkAt is unsupported for shallow docs. Reuse this isolated fork as
        // the read-only base; its oplog deliberately retains the local edits.
        try base.checkout(frontiers: baseFrontiers)
        guard base.stateVv() == common else { return false }
        let candidate = replacement.fork()
        guard let before = base.getDeepValue().mapValue,
              let after = previous.getDeepValue().mapValue else { return false }
        // Mobile-owned workspace edits are records, not positional operations.
        // Recreate new record containers by logical key; never replay command
        // lists or overwrite a concurrent host mutation.
        let writable: Set<String> = ["chats", "spaces", "sessionRefs", "devices", "worktreeDeletions"]
        for name in Set(before.keys).union(after.keys) where before[name] != after[name] {
            guard writable.contains(name),
                  let old = before[name]?.mapValue ?? (before[name] == nil ? [:] : nil),
                  let desired = after[name]?.mapValue else { return false }
            for key in Set(old.keys).union(desired.keys) where old[key] != desired[key] {
                try applyRecordChange(root: name, key: key, before: old[key], after: desired[key], in: candidate)
            }
        }
        candidate.commit()
        return try importRecovery(from: candidate, into: replacement, since: snapshotVersion)
    }

    /// Immutable JSON values are replayable, but container references at any
    /// depth require identity-aware remapping and are deliberately not coerced.
    private static func isRecoveryValue(_ value: LoroValue?) -> Bool {
        guard let value else { return true }
        switch value {
        case .container:
            return false
        case .list(let values):
            return values.allSatisfy { isRecoveryValue($0) }
        case .map(let values):
            return values.values.allSatisfy { isRecoveryValue($0) }
        default:
            return true
        }
    }

    /// Atomically persist the doc's snapshot.
    static func save(doc: LoroDoc, id: String) {
        guard let data = try? doc.export(mode: .snapshot) else { return }
        try? data.write(to: url(for: id), options: .atomic)
    }

    static func saveReplacement(doc: LoroDoc, id: String) throws {
        let bytes = try doc.export(mode: .snapshot)
        guard !doc.isDetached(), doc.stateVv() == doc.oplogVv() else {
            throw MobileSessionError.unavailable("Crew recovery snapshot did not materialize completely.")
        }
        try bytes.write(to: url(for: id), options: .atomic)
    }

    /// LRU-prune session snapshots (the workspace doc is always kept).
    static func prune(keep: Int) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: directory,
                                                      includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        // Intent journals and recovery originals are never disposable caches.
        let sessions = files.filter { $0.pathExtension == "loro" && $0.lastPathComponent.hasPrefix("scoped-") }
        guard sessions.count > keep else { return }
        let sorted = sessions.sorted {
            let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            return a > b
        }
        for stale in sorted.dropFirst(keep) {
            try? fm.removeItem(at: stale)
        }
    }

    /// Sign-out hygiene: local doc state belongs to the signed-in identity.
    static func wipeAll() {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// Debounced snapshot persistence shared by the doc stores: poke on every
/// change; the snapshot writes ~1.5s after the last poke, and `flush` forces
/// it (backgrounding, store teardown).
@MainActor
final class DocSaver {
    private let docId: String
    private var doc: LoroDoc
    private var saveTask: Task<Void, Never>?
    private var saveDeadline: UInt64?
    private var dirty = false

    init(docId: String, doc: LoroDoc) {
        self.docId = docId
        self.doc = doc
    }

    /// Keep one saver/timer bound to the adopted replica. A pending old-cache
    /// write must never overwrite the recovered snapshot after the handoff.
    func replaceDocument(with replacement: LoroDoc, recordIntents: [DocDisk.RecordIntent]? = nil) throws {
        if let recordIntents { try DocDisk.saveIntents(recordIntents, id: docId) }
        try DocDisk.saveReplacement(doc: replacement, id: docId)
        saveTask?.cancel()
        saveTask = nil
        saveDeadline = nil
        doc = replacement
        dirty = false
    }

    func poke() {
        dirty = true
        saveDeadline = DispatchTime.now().uptimeNanoseconds + 1_500_000_000
        // Streaming moves the deadline, not the timer: keep one sleeper per
        // doc instead of one suspended task for every update in the window.
        guard saveTask == nil else { return }
        saveTask = Task { @MainActor [weak self] in
            while let deadline = self?.saveDeadline {
                let now = DispatchTime.now().uptimeNanoseconds
                if now >= deadline {
                    self?.flush()
                    return
                }
                do {
                    try await Task.sleep(nanoseconds: deadline - now)
                } catch {
                    return
                }
            }
        }
    }

    func flush() {
        saveTask?.cancel()
        saveTask = nil
        saveDeadline = nil
        guard dirty else { return }
        dirty = false
        DocDisk.save(doc: doc, id: docId)
    }
}

#if DEBUG
extension DocDisk {
    @MainActor
    static func runRecordRecoveryRegression() -> Bool {
        let cacheId = "recovery-regression-\(UUID().uuidString.lowercased())"
        defer {
            try? FileManager.default.removeItem(at: url(for: cacheId))
            try? FileManager.default.removeItem(at: intentURL(for: cacheId))
        }
        do {
            let publicId = "018eeb58-6508-78e8-a544-44682ab94c50"
            let alias = publicId + "::session::" + publicId + "::session::" + publicId
            guard selfSessionId(alias).map({ $0 == publicId }) == true,
                  selfSessionId(alias + "::session::018eeb58-6508-78e8-a544-44682ab94c51") == nil,
                  selfSessionId(publicId + "::session::018eeb58-6508-78e8-a544-44682ab94c51::session::" + publicId) == nil,
                  selfSessionId(alias + "::session::") == nil else { return false }
            let aliasDoc = LoroDoc()
            let aliasRow = try aliasDoc.getMap(id: "chats").getOrCreateContainer(key: alias, child: LoroMap())
            try aliasRow.insert(key: "id", v: publicId); try aliasRow.insert(key: "deviceId", v: "owner")
            try aliasRow.insert(key: "title", v: "Retained")
            aliasDoc.commit()
            guard try migrateWorkspaceRows(in: aliasDoc), !(try migrateWorkspaceRows(in: aliasDoc)),
                  try recordValue(in: aliasDoc, root: "chats", key: publicId)?.mapValue?["title"]?.stringValue == "Retained" else { return false }
            var aliasIntent = RecordIntent(root: "chats", key: alias, before: try recordData(.map(value: ["id": .string(value: alias), "deviceId": .string(value: "owner"), "title": .string(value: "Before")])))
            aliasIntent.after = try recordData(.map(value: ["id": .string(value: publicId), "deviceId": .string(value: "owner"), "title": .string(value: "After")]))
            aliasIntent.intermediates = [aliasIntent.before, aliasIntent.after]
            let normalized = try normalizeWorkspaceIntent(aliasIntent)
            guard normalized.key == publicId, normalized.id == aliasIntent.id,
                  try recordValue(normalized.before)?.mapValue?["id"]?.stringValue == publicId,
                  try normalized.intermediates.allSatisfy({ try recordValue($0)?.mapValue?["id"]?.stringValue == publicId }) else { return false }
            var mixed = aliasIntent
            mixed.key = publicId; mixed.before = aliasIntent.after; mixed.after = aliasIntent.before
            guard try recordValue(normalizeWorkspaceIntent(mixed).after)?.mapValue?["id"]?.stringValue == publicId else { return false }
            mixed.intermediates = [try recordData(.map(value: ["id": .string(value: alias), "deviceId": .string(value: "foreign")]))]
            do { _ = try normalizeWorkspaceIntent(mixed); return false } catch MobileSessionError.unavailable(_) {}
            var membership = RecordIntent(root: "sessionRefs", key: "5:owner:" + alias, before: try recordData(.map(value: ["userId": .string(value: "owner"), "chatId": .string(value: alias)])))
            membership.intermediates = [membership.before]
            membership.after = try recordData(LoroValue.fromJSON(["userId": "owner", "chatId": publicId, "environment": ["ownerPrincipal": "session-owner", "scope": ["projectId": "project", "deploymentId": "deployment"]]]))
            let enrichedMembership = try normalizeWorkspaceIntent(membership)
            guard try recordValue(enrichedMembership.after)?.mapValue?["environment"]?.mapValue?["scope"]?.mapValue?["deploymentId"]?.stringValue == "deployment" else { return false }
            membership.before = enrichedMembership.after; membership.intermediates = []
            membership.after = try recordData(LoroValue.fromJSON(["userId": "owner", "chatId": publicId, "environment": ["ownerPrincipal": "foreign", "scope": ["projectId": "project", "deploymentId": "deployment"]]]))
            do { _ = try normalizeWorkspaceIntent(membership); return false } catch MobileSessionError.unavailable(_) {}
            let foreign = aliasDoc.fork()
            let foreignAlias = try foreign.getMap(id: "chats").getOrCreateContainer(key: alias, child: LoroMap())
            try foreignAlias.insert(key: "id", v: alias); try foreignAlias.insert(key: "deviceId", v: "foreign")
            foreign.commit()
            do { _ = try migrateWorkspaceRows(in: foreign); return false } catch MobileSessionError.unavailable(_) {}
            try aliasDoc.getMap(id: "chats").delete(key: publicId)
            let lateAlias = try aliasDoc.getMap(id: "chats").getOrCreateContainer(key: alias, child: LoroMap())
            try lateAlias.insert(key: "id", v: alias); try lateAlias.insert(key: "deviceId", v: "owner")
            aliasDoc.commit()
            guard try migrateWorkspaceRows(in: aliasDoc), aliasDoc.getMap(id: "chats").get(key: publicId) == nil else { return false }
            let source = LoroDoc()
            let row = try source.getMap(id: "chats").getOrCreateContainer(key: "existing", child: LoroMap())
            try row.insert(key: "id", v: "existing")
            try row.insert(key: "deviceId", v: "owner")
            try row.insert(key: "title", v: "Original")
            try row.insert(key: "lastSeenAt", v: Int64(10))
            source.commit()
            let retainedBase = try recordData(recordValue(in: source, root: "chats", key: "existing"))
            let local = source.fork()
            let offlineRow = try local.getMap(id: "chats").getOrCreateContainer(key: "offline", child: LoroMap())
            try offlineRow.insert(key: "id", v: "offline")
            try offlineRow.insert(key: "deviceId", v: "owner")
            try offlineRow.insert(key: "title", v: "Created offline")
            try offlineRow.insert(key: "config", v: LoroValue.fromJSON(["model": "retained", "nested": ["sandbox": "workspace-write"]]))
            local.commit()
            try offlineRow.insert(key: "title", v: "Renamed offline")
            let localExisting = local.getMap(id: "chats").get(key: "existing")!.asLoroMap()!
            try localExisting.insert(key: "title", v: "Edited offline")
            try localExisting.insert(key: "lastSeenAt", v: Int64(20))
            local.commit()
            save(doc: local, id: cacheId)
            guard let cached = loadReplica(id: cacheId) else { return false }
            try row.insert(key: "lastSeenAt", v: Int64(30))
            try row.insert(key: "archived", v: true)
            source.commit()
            let floor = source.stateFrontiers()
            try row.insert(key: "lastSeenAt", v: Int64(40))
            source.commit()
            guard let replacement = replacementSnapshot(bytes: try source.export(mode: .shallowSnapshot(frontiers: floor))),
                  preserveLocalOperations(from: cached, in: replacement),
                  let records = replacement.getDeepValue().mapValue?["chats"]?.mapValue,
                  records["offline"]?.mapValue?["title"]?.stringValue == "Renamed offline",
                  records["offline"]?.mapValue?["config"]?.mapValue?["nested"]?.mapValue?["sandbox"]?.stringValue == "workspace-write",
                  records["existing"]?.mapValue?["title"]?.stringValue == "Edited offline",
                  records["existing"]?.mapValue?["archived"]?.boolValue == true,
                  records["existing"]?.mapValue?["lastSeenAt"]?.i64Value == 40,
                  replacement.stateVv().includesVv(other: source.stateVv()) else { return false }
            // A deleted authoritative record cannot be revived by an offline edit.
            try source.getMap(id: "chats").delete(key: "existing")
            source.commit()
            guard let tombstone = replacementSnapshot(bytes: try source.export(mode: .shallowSnapshot(frontiers: source.stateFrontiers()))) else { return false }
            let authoritative = tombstone.getDeepValue()
            let original = cached.getDeepValue()
            let originalVersion = cached.stateVv()
            let cacheBytes = try Data(contentsOf: url(for: cacheId))
            var intent = RecordIntent(root: "chats", key: "existing", before: retainedBase)
            intent.after = try recordData(recordValue(in: cached, root: "chats", key: "existing"))
            try saveIntents([intent], id: cacheId)
            let journalBytes = try Data(contentsOf: intentURL(for: cacheId))
            let snapshotVersion = tombstone.stateVv()
            do {
                _ = try rebaseLocalChanges(from: cached, into: tombstone, since: snapshotVersion)
                return false
            } catch MobileSessionError.unavailable(_) {
                // The authoritative deletion conflicts with the retained edit.
            }
            guard tombstone.stateVv() == snapshotVersion, tombstone.oplogVv() == snapshotVersion,
                  tombstone.getDeepValue() == authoritative,
                  tombstone.getMap(id: "chats").get(key: "existing") == nil,
                  cached.stateVv() == originalVersion, cached.oplogVv() == originalVersion,
                  cached.getDeepValue() == original,
                  try Data(contentsOf: url(for: cacheId)) == cacheBytes,
                  try Data(contentsOf: intentURL(for: cacheId)) == journalBytes,
                  loadReplica(id: cacheId)?.getDeepValue() == original else { return false }
            E2ERunner.log("OK Crew saved record recovery: nested create rename edit, monotonic seen, remote tombstone conflict retained")
            return true
        } catch { E2ERunner.log("FAIL Crew saved record recovery: \(error)"); return false }
    }
}
#endif
