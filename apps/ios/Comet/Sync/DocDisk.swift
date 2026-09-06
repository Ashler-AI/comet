// On-device Loro doc persistence — the old mobile app's snapshot cache
// (kv.ts/loro-room.ts) and the engine's DocsStore, in file form: one snapshot
// per doc under Application Support. Docs load BEFORE the room join, so the
// UI renders instantly from local state (offline included) and the join's
// version vector turns the backfill incremental instead of a full snapshot.

import Foundation
import Loro
import os

enum DocDisk {
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

    /// Import the saved snapshot, if any. Returns whether anything loaded.
    @discardableResult
    static func load(into doc: LoroDoc, id: String) -> Bool {
        guard let data = try? Data(contentsOf: url(for: id)), !data.isEmpty else { return false }
        guard let status = try? doc.importWith(bytes: data, origin: "disk") else { return false }
        return (status.pending?.isEmpty ?? true)
            && !doc.isDetached() && doc.stateVv() == doc.oplogVv()
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
        let local = previous.fork()
        let baseFrontiers = local.vvToFrontiers(vv: common)
        guard let reconstructed = local.frontiersToVv(frontiers: baseFrontiers),
              reconstructed == common
        else { return false }
        let base = try local.forkAt(frontiers: baseFrontiers)
        guard base.stateVv() == common, base.oplogVv() == common else { return false }
        let delta = try local.diff(a: baseFrontiers, b: local.oplogFrontiers())
        let candidate = replacement.fork()

        // applyDiff is a state replay, NOT a concurrent CRDT merge. Map value
        // edits commute on disjoint keys; positional/list/tree/text changes do
        // not. Container creation/replacement also remaps IDs and can silently
        // skip unreachable children. Fail closed for those cases rather than
        // flattening containers or overwriting concurrent server changes.
        let replay = DiffBatch()
        for entry in delta.getDiff() {
            guard case .map(let changes) = entry.diff,
                  let baseMap = base.getContainer(id: entry.cid)?.asLoroMap(),
                  let serverMap = candidate.getContainer(id: entry.cid)?.asLoroMap(),
                  !baseMap.isDeleted(), !serverMap.isDeleted(),
                  case .map(let baseValues) = baseMap.getValue(),
                  case .map(let serverValues) = serverMap.getValue()
            else { return false }
            var pending: [String: ValueOrContainer?] = [:]
            for (key, updated) in changes.updated {
                let desired: LoroValue?
                if let updated {
                    guard let value = updated.asValue(), isRecoveryValue(value)
                    else { return false }
                    desired = value
                } else {
                    desired = nil
                }
                // The host may already have materialized the same intent under
                // other operation IDs. Do not rewrite it or change precedence.
                if serverValues[key] == desired { continue }
                guard baseValues[key] == serverValues[key],
                      isRecoveryValue(baseValues[key])
                else { return false }
                // updateValue retains a nil payload as a deletion entry.
                pending.updateValue(updated, forKey: key)
            }
            if !pending.isEmpty {
                if let _ = replay.push(cid: entry.cid, diff: .map(diff: MapDelta(updated: pending))) {
                    return false
                }
            }
        }
        if replay.getDiff().isEmpty { return true }
        try candidate.applyDiff(diff: replay)
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

    /// LRU-prune session snapshots (the workspace doc is always kept).
    static func prune(keep: Int) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: directory,
                                                      includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        let sessions = files.filter { !$0.lastPathComponent.hasPrefix("ws4_") }
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
    func replaceDocument(with replacement: LoroDoc) {
        saveTask?.cancel()
        saveTask = nil
        saveDeadline = nil
        doc = replacement
        poke()
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
