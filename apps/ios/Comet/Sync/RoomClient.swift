// Loro room client — a Swift port of crates/sync/src/room.rs.
//
// One client per room (workspace doc or session doc), one WebSocket carrying
// two sub-rooms: the `%LOR` doc room and the `%EPH` presence room. The client
// joins with its local oplog VV, imports the server's backfill, resubmits
// anything the server lacks (covers unacked updates across reconnects), and
// relays local commits as DocUpdate batches until acked.

import Foundation
import Loro
import os

enum RoomEvent {
    case connected
    case disconnected
    case remoteUpdate
    case ephemeralUpdate
    case recoveryBlocked(String)
    case localChangesAcknowledged(Data)
}

/// Sync must never fail silently (2026-07-31: a send that never left the
/// device was indistinguishable from a working one — `try?` all the way
/// down). Visible in Console.app / `log stream` under this subsystem.
let roomLog = Logger(subsystem: Bundle.main.bundleIdentifier ?? "ai.ashler.crew", category: "sync")

actor RoomClient {
    // Constants mirrored from room.rs.
    static let fragmentBytes = 200_000
    static let pingIntervalNs: UInt64 = 30_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    static let backoffBaseMs = 250
    static let backoffCapMs = 30_000
    // Match room.rs: serialize a bounded number of full heals per connection.
    static let maxFullResyncs = 3
    static let maxFragmentCount: UInt64 = 4096
    static let maxReassembledBytes = 64 * 1024 * 1024
    // Room-level liveness (room.rs, 2026-07-30 incident): the silence lease
    // above is TRANSPORT-only — the CF runtime auto-answers our text pings
    // without ever waking the DO, so a wedged room looks healthy forever if
    // pongs are all we judge by. Room liveness is judged on %LOR frames plus
    // a hard join-answer deadline; %EPH presence traffic deliberately does
    // not count (the edge's eph path never touches the doc machinery).
    static let joinDeadlineNs: UInt64 = 15_000_000_000
    static let roomProbeAfterNs: UInt64 = 900_000_000_000
    static let roomProbeMaxNs: UInt64 = 4 * 3_600_000_000_000
    static let probeReplyGraceNs: UInt64 = 30_000_000_000
    static let probeOnDemandMinQuietNs: UInt64 = 30_000_000_000
    static let livenessTickNs: UInt64 = 5_000_000_000

    let roomId: String
    private(set) var doc: LoroDoc
    let eph: EphemeralStore
    private let urlProvider: @Sendable () async -> URL?
    private let events: @Sendable (RoomEvent) -> Void
    private let adoptSnapshot: @MainActor @Sendable (LoroDoc, LoroDoc) -> Bool
    private let readOnly: Bool
    private var recoverApplicationIntents: Bool

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var livenessTask: Task<Void, Never>?
    private var reconnectTask: Task<Void, Never>?
    private var pending: [BatchId: [[UInt8]]] = [:]
    // Only catch-up uploads gate convergence; ordinary live writes do not
    // toggle connectivity while awaiting their acknowledgements.
    private var catchupBatches: Set<BatchId> = []
    private var fragments: [BatchId: FragmentBuffer] = [:]
    private var joinedLor = false
    private var fullResyncs = 0
    private var snapshotRecovery: LoroDoc?
    private var serverVersion: VersionVector?
    // Join advertisements and pending imports may not advance either local VV.
    // Keep their target across redials so a stale server reply cannot heal it.
    private let requiredRemoteVersion = VersionVector()
    private var historyRepairAttempted = false
    private var historyRepairBatch: BatchId?
    private var backoffMs = RoomClient.backoffBaseMs
    // Survives recovery redials: a join answer alone does not prove a healed doc.
    private var recovering = false
    private var deferredFullResync = false
    private var lastInbound = DispatchTime.now()
    private var closed = false
    private var generation = 0
    // Room-liveness state (room.rs Session::{join_sent_at, join_is_probe,
    // last_lor_rx}): the instant of the last %LOR JoinRequest still awaiting
    // JoinResponseOk, whether that join is a liveness probe on an established
    // session (its answer must not replay join side effects), and the last
    // inbound %LOR frame — the clock feeding both the deadline and the probe.
    private var joinSentAt: DispatchTime?
    private var backfillStartedAt: DispatchTime?
    private var joinIsProbe = false
    private var lastLorRx = DispatchTime.now()
    private var lastPushedRx = DispatchTime.now()
    private var probeIntervalNs = RoomClient.roomProbeAfterNs
    private var lastProbeAt: DispatchTime?

    private struct FragmentBuffer {
        var crdt: CrdtType
        var parts: [[UInt8]?]
        var received: Int
        var totalSize: Int
    }

    init(roomId: String,
         doc: LoroDoc,
         ephTimeoutMs: Int64 = 30_000,
         readOnly: Bool = false,
         recoverApplicationIntents: Bool = false,
         urlProvider: @escaping @Sendable () async -> URL?,
         events: @escaping @Sendable (RoomEvent) -> Void,
         adoptSnapshot: @escaping @MainActor @Sendable (LoroDoc, LoroDoc) -> Bool) {
        self.roomId = roomId
        self.doc = doc
        self.eph = EphemeralStore(timeout: ephTimeoutMs)
        self.urlProvider = urlProvider
        self.events = events
        self.adoptSnapshot = adoptSnapshot
        self.readOnly = readOnly
        self.recoverApplicationIntents = recoverApplicationIntents
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        connect()
    }


    private func blockRecovery(_ message: String) {
        stop()
        events(.disconnected)
        events(.recoveryBlocked(message))
    }

    func stop() {
        closed = true
        generation += 1
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        reconnectTask?.cancel()
        reconnectTask = nil
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joinedLor = false
        pending.removeAll(); catchupBatches.removeAll(); fragments.removeAll()
        snapshotRecovery = nil
    }

    /// Match desktop foreground probes: ACKs and pongs do not prove that
    /// broadcasts are reaching this replica. Coalesce behind any active join.
    func probe(at instant: DispatchTime? = nil, force: Bool = false) async {
        let now = (instant ?? .now()).uptimeNanoseconds
        if !closed, !joinedLor, reconnectTask != nil {
            reconnectTask?.cancel()
            reconnectTask = nil
            connect()
            return
        }
        guard !closed, joinedLor, joinSentAt == nil, backfillStartedAt == nil,
              force || (now >= lastPushedRx.uptimeNanoseconds && now - lastPushedRx.uptimeNanoseconds >= RoomClient.probeOnDemandMinQuietNs) else { return }
        probeIntervalNs = RoomClient.roomProbeAfterNs
        await sendProbe()
    }

    private func connect() {
        guard !closed else { return }
        generation += 1
        let gen = generation
        joinedLor = false
        fullResyncs = 0
        snapshotRecovery = nil
        serverVersion = nil
        historyRepairAttempted = false
        historyRepairBatch = nil
        deferredFullResync = false
        // Batch ids belong to the old socket. Rejoin exports all missing
        // operations from the durable doc's VV under fresh ids; keeping the
        // old payloads here would retain them forever when their acks were lost.
        pending.removeAll()
        catchupBatches.removeAll()
        fragments.removeAll()
        joinSentAt = nil
        backfillStartedAt = nil
        joinIsProbe = false
        lastLorRx = .now()
        lastPushedRx = lastLorRx
        probeIntervalNs = RoomClient.roomProbeAfterNs
        lastProbeAt = nil

        Task {
            guard let url = await urlProvider() else {
                // No URL = no token (refresh failed or signed out) — the
                // single most confusing silent failure: everything cached
                // renders, nothing syncs.
                roomLog.error("room \(self.roomId, privacy: .public): no socket URL (token unavailable); backing off")
                await self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) {
        guard gen == generation, !closed else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.currentSocket(gen: gen) else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message, gen: gen)
                } catch {
                    await self.onSocketError(gen: gen)
                    return
                }
            }
        }

        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    try await Task.sleep(nanoseconds: RoomClient.pingIntervalNs)
                } catch {
                    return
                }
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        livenessTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    try await Task.sleep(nanoseconds: RoomClient.livenessTickNs)
                } catch {
                    return
                }
                guard let self else { return }
                await self.livenessTick(gen: gen)
            }
        }

        // Join the doc room with our local VV (empty VV asks for a snapshot).
        Task { await self.sendJoinLoro(version: self.recoverApplicationIntents ? [] : self.localVersionBytes()) }
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) {
        guard gen == generation, !closed else { return }
        roomLog.warning("room \(self.roomId, privacy: .public): session ended (joined=\(self.joinedLor)); redialing in \(self.backoffMs)ms")
        events(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed else { return }
        // Invalidate callbacks before cancelling the socket. Its receive
        // failure can otherwise schedule a second redial for this generation,
        // leaving duplicate sockets and periodic tasks after both connect.
        generation += 1
        let reconnectGeneration = generation
        reconnectTask?.cancel()
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        joinedLor = false
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        let delay = backoffMs
        backoffMs = min(backoffMs * 2, RoomClient.backoffCapMs)
        reconnectTask = Task { [weak self] in
            do {
                try await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000)
            } catch {
                return
            }
            await self?.reconnectIfCurrent(gen: reconnectGeneration)
        }
    }

    private func reconnectIfCurrent(gen: Int) {
        guard gen == generation, !closed else { return }
        reconnectTask = nil
        connect()
    }

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > RoomClient.silenceLeaseNs {
            roomLog.warning("room \(self.roomId, privacy: .public): socket silent past lease; treating as dead")
            onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    /// Two-tier room liveness (room.rs run_session): an in-flight %LOR
    /// JoinRequest has a hard answer deadline; otherwise a long-quiet room
    /// gets an idempotent rejoin probe. The deadline runs from the LATER of
    /// join-sent and last %LOR frame, so a join queued behind a draining
    /// backfill that keeps producing %LOR acks is never killed mid-push. A
    /// hibernating-but-healthy DO is simply woken by the probe and answers —
    /// hibernation is NOT death, which is why probes run in minutes while
    /// the transport lease runs in seconds.
    private func livenessTick(gen: Int, at instant: DispatchTime = .now()) async {
        guard gen == generation, !closed else { return }
        let now = instant.uptimeNanoseconds
        if let sent = joinSentAt {
            let base = max(sent.uptimeNanoseconds, lastLorRx.uptimeNanoseconds)
            if now - base > RoomClient.joinDeadlineNs {
                // The 2026-07-30 hang: a room that accepted the socket but
                // never answered the join. Redial via the backoff loop — one
                // fresh dial re-instantiates a wedged DO.
                roomLog.warning("room \(self.roomId, privacy: .public): no JoinResponseOk within deadline; room presumed wedged, redialing")
                onSocketError(gen: gen)
            }
            return
        }
        if let started = backfillStartedAt {
            let base = max(started.uptimeNanoseconds, lastLorRx.uptimeNanoseconds)
            if now - base > RoomClient.joinDeadlineNs {
                roomLog.warning("room \(self.roomId, privacy: .public): joined but backfill did not complete; redialing")
                onSocketError(gen: gen)
            }
            return
        }
        if joinedLor, now - lastPushedRx.uptimeNanoseconds > probeIntervalNs {
            probeIntervalNs = min(probeIntervalNs * 2, RoomClient.roomProbeMaxNs)
            await sendProbe()
        }
    }

    private func sendProbe() async {
        // Arm before send suspends; the answer may arrive during that await.
        joinSentAt = .now()
        joinIsProbe = true
        lastProbeAt = .now()
        await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [],
                                version: localVersionBytes()))
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        switch message {
        case .string:
            return  // "pong" — lease already refreshed
        case .data(let data):
            guard let frame = LoroWire.decode(data) else { return }
            await handleFrame(frame, gen: gen)
        @unknown default:
            return
        }
    }

    private func handleFrame(_ frame: ProtocolMessage, gen: Int) async {
        // All %LOR frames feed the join deadline. Only server-pushed frames
        // feed the probe clock: own-write ACKs can mask a broken broadcast path.
        if crdtOf(frame) == .loro {
            lastLorRx = .now()
            if case .ack = frame {
                // ACK proves upload admission, not download freshness.
            } else {
                lastPushedRx = lastLorRx
                if lastProbeAt.map({
                    lastLorRx.uptimeNanoseconds - $0.uptimeNanoseconds
                        <= RoomClient.probeReplyGraceNs
                }) != true {
                    probeIntervalNs = RoomClient.roomProbeAfterNs
                }
            }
        }
        switch frame {
        case .joinResponseOk(let crdt, _, _, let version, _):
            await onJoinOk(crdt: crdt, version: version)

        case .joinError(let crdt, _, let code, let message):
            roomLog.error("room \(self.roomId, privacy: .public): join error \(String(describing: code), privacy: .public): \(message, privacy: .public)")
            if crdt == .loro {
                if code == .versionUnknown {
                    joinSentAt = nil  // The rejected join has been answered.
                    // Server can't diff from our VV — full snapshot backfill.
                    await requestFullSnapshot()
                } else if code == .appError, message == "incomplete_history" {
                    await repairIncompleteHistory()
                } else {
                    if code == .authFailed || message.contains("not_found") || message.contains("invalid_session") {
                        blockRecovery("Crew cannot access this room: \(message)")
                    } else {
                        onSocketError(gen: gen)
                    }
                }
            }

        case .docUpdate(let crdt, _, let updates, _):
            await applyRemote(crdt: crdt, updates: updates)

        case .docUpdateFragmentHeader(let crdt, _, let batchId, let count, let total):
            guard count > 0, count <= RoomClient.maxFragmentCount,
                  total <= UInt64(RoomClient.maxReassembledBytes) else { return }
            fragments[batchId] = FragmentBuffer(crdt: crdt, parts: Array(repeating: nil, count: Int(count)),
                                                received: 0, totalSize: Int(total))

        case .docUpdateFragment(_, _, let batchId, let index, let fragment):
            await onFragment(batchId: batchId, index: Int(index), fragment: fragment)

        case .ack(let crdt, _, let refId, let status):
            await onAck(crdt: crdt, refId: refId, status: status)

        case .roomError(_, _, let code, _):
            if code == .evicted {
                onSocketError(gen: gen)
            } else {
                await sendJoinLoro(version: localVersionBytes())
            }

        case .joinRequest, .leave:
            return
        }
    }

    private func crdtOf(_ frame: ProtocolMessage) -> CrdtType {
        switch frame {
        case .joinRequest(let crdt, _, _, _),
             .joinResponseOk(let crdt, _, _, _, _),
             .joinError(let crdt, _, _, _),
             .docUpdate(let crdt, _, _, _),
             .docUpdateFragmentHeader(let crdt, _, _, _, _),
             .docUpdateFragment(let crdt, _, _, _, _),
             .roomError(let crdt, _, _, _),
             .ack(let crdt, _, _, _),
             .leave(let crdt, _):
            return crdt
        }
    }

    private func onJoinOk(crdt: CrdtType, version: [UInt8]) async {
        let gen = generation
        switch crdt {
        case .loro:
            joinSentAt = nil  // join answered — disarm the deadline
            let wasProbe = joinIsProbe
            joinIsProbe = false
            joinedLor = true
            // Resubmit-from-VV: push everything the server lacks. Gated on
            // the VERSION VECTORS, not the export bytes: the export returns a
            // non-empty envelope even when there is nothing to say, so a
            // byte-length gate made every liveness probe upload a no-op
            // DocUpdate that dirtied the room's caches (room.rs finding).
            serverVersion = version.isEmpty ? VersionVector()
                : try? VersionVector.decode(bytes: Data(version))
            if let serverVersion { requiredRemoteVersion.merge(other: serverVersion) }
            // The edge answers the join BEFORE sending its snapshot/deltas.
            // An accepted socket is not a usable replica until that advertised
            // version has reached materialized state, including on first login.
            if !hasMaterializedRemoteVersion(doc)
                || serverVersion.map({ !$0.includesVv(other: doc.oplogVv()) }) == true {
                if !recovering { events(.disconnected) }
                recovering = true
                backfillStartedAt = .now()
            }
            if !wasProbe { recovering = true }
            if await adoptRecoveredSnapshotIfCaughtUp() { events(.remoteUpdate) }
            await resubmitMissingUpdates()
            guard gen == generation, !closed, joinedLor else { return }
            if wasProbe {
                finishRecoveryIfCaughtUp()
                if recovering, deferredFullResync { await requestFullSnapshot() }
                // A probe answer on an established session proves the room is
                // alive — that is ALL it is for. Re-running the side effects
                // below would re-join %EPH (re-uploading full presence) and
                // re-broadcast .connected on a timer.
                return
            }
            roomLog.info("room \(self.roomId, privacy: .public): joined")
            // Join presence once the doc room is up.
            await send(.joinRequest(crdt: .loroEphemeral, roomId: roomId, auth: [], version: []))
            guard gen == generation, !closed, joinedLor else { return }
            finishRecoveryIfCaughtUp()
            if recovering, deferredFullResync { await requestFullSnapshot() }
        case .loroEphemeral:
            let all = eph.encodeAll()
            if !all.isEmpty {
                await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId,
                                      updates: [[UInt8](all)], batchId: .random()))
            }
        }
    }

    private func applyRemote(crdt: CrdtType, updates: [[UInt8]]) async {
        switch crdt {
        case .loro:
            var imported = false
            for update in updates where !update.isEmpty {
                let bytes = Data(update)
                if let metadata = try? decodeImportBlobMeta(bytes: bytes, checkChecksum: true) {
                    requiredRemoteVersion.merge(other: metadata.partialEndVv)
                    serverVersion?.merge(other: metadata.partialEndVv)
                }
                if (readOnly || recoverApplicationIntents), snapshotRecovery == nil,
                   let candidate = DocDisk.replacementSnapshot(bytes: bytes) {
                    // Viewport intents are outside Loro. A server snapshot is
                    // authoritative; stale legacy command branches are retained
                    // locally, never uploaded as newly admitted instructions.
                    snapshotRecovery = candidate
                    if await adoptRecoveredSnapshotIfCaughtUp() { imported = true }
                    continue
                }
                if let candidate = snapshotRecovery {
                    guard (try? candidate.importWith(bytes: bytes, origin: "remote")) != nil else {
                        snapshotRecovery = nil
                        await requestFullSnapshot()
                        continue
                    }
                } else {
                    let status = try? doc.importWith(bytes: bytes, origin: "remote")
                    let complete = status.map { ($0.pending?.isEmpty ?? true) } ?? false
                    if complete, !doc.isDetached(), doc.stateVv() == doc.oplogVv() {
                        imported = imported || !(status?.success.isEmpty ?? true)
                        await resubmitMissingUpdates()
                        finishRecoveryIfCaughtUp()
                        continue
                    }
                    // A persisted shallow baseline can precede many server deltas.
                    // Keep it isolated until the entire advertised frontier arrives.
                    guard let candidate = DocDisk.replacementSnapshot(bytes: bytes) else {
                        await requestFullSnapshot()
                        continue
                    }
                    snapshotRecovery = candidate
                }
                if await adoptRecoveredSnapshotIfCaughtUp() { imported = true }
            }
            if imported { events(.remoteUpdate) }
        case .loroEphemeral:
            var applied = false
            for update in updates where !update.isEmpty {
                if (try? eph.apply(data: Data(update))) != nil { applied = true }
            }
            if applied { events(.ephemeralUpdate) }
        }
    }

    private func adoptRecoveredSnapshotIfCaughtUp() async -> Bool {
        guard let candidate = snapshotRecovery, hasMaterializedRemoteVersion(candidate) else { return false }
        let previous = doc
        guard await adoptSnapshot(previous, candidate) else {
            snapshotRecovery = nil
            roomLog.error("room \(self.roomId, privacy: .public): snapshot handoff rejected; retaining current replica")
            blockRecovery("Crew recovery is blocked by conflicting local records. The original data is retained on this device.")
            return false
        }
        doc = candidate
        recoverApplicationIntents = false
        snapshotRecovery = nil
        await resubmitMissingUpdates()
        finishRecoveryIfCaughtUp()
        roomLog.info("room \(self.roomId, privacy: .public): adopted complete snapshot and retained local operations")
        return true
    }

    private func requestFullSnapshot() async {
        if !recovering {
            recovering = true
            events(.disconnected)
        }
        // Coalesce failures behind the outstanding join. Exhausted attempts
        // redial through existing backoff, which only a verified heal resets.
        guard reconnectTask == nil else { return }
        guard joinSentAt == nil else {
            deferredFullResync = true
            return
        }
        deferredFullResync = false
        guard fullResyncs < RoomClient.maxFullResyncs else {
            onSocketError(gen: generation)
            return
        }
        fullResyncs += 1
        serverVersion = nil
        snapshotRecovery = nil
        roomLog.warning("room \(self.roomId, privacy: .public): incomplete import; requesting full snapshot")
        await sendJoinLoro(version: [])
    }
    private func finishRecoveryIfCaughtUp() {
        guard recovering, !recoverApplicationIntents, joinedLor, catchupBatches.isEmpty,
              hasMaterializedRemoteVersion(doc) else { return }
        recovering = false
        deferredFullResync = false
        backfillStartedAt = nil
        fullResyncs = 0
        backoffMs = RoomClient.backoffBaseMs
        if joinedLor { events(.connected) }
        if let serverVersion { events(.localChangesAcknowledged(serverVersion.encode())) }
    }

    private func hasMaterializedRemoteVersion(_ replica: LoroDoc) -> Bool {
        guard let serverVersion, !replica.isDetached() else { return false }
        let state = replica.stateVv()
        return state == replica.oplogVv() && state.includesVv(other: serverVersion)
            && state.includesVv(other: requiredRemoteVersion)
    }

    private func resubmitMissingUpdates() async {
        guard recovering, joinedLor, catchupBatches.isEmpty,
              hasMaterializedRemoteVersion(doc), let serverVersion,
              !serverVersion.includesVv(other: doc.oplogVv()) else { return }
        guard !recoverApplicationIntents else { return }
        if readOnly { await requestFullSnapshot(); return }
        backfillStartedAt = .now()
        // A shallow replica cannot export dependencies older than its retained
        // history. A snapshot carries that state without dropping local edits.
        do {
            var missing: Data
            if serverVersion.isEmpty() || !serverVersion.includesVv(other: doc.shallowSinceVv()) {
                missing = try doc.export(mode: .snapshot)
            } else {
                missing = try doc.export(mode: .updates(from: serverVersion))
                if missing.count > RoomClient.fragmentBytes,
                   let snapshot = try? doc.export(mode: .snapshot), snapshot.count < missing.count {
                    missing = snapshot
                }
            }
            if !missing.isEmpty { await sendLoroUpdates([[UInt8](missing)], catchup: true) }
        } catch {
            roomLog.error("room \(self.roomId, privacy: .public): cannot export missing operations: \(String(describing: error), privacy: .public)")
            onSocketError(gen: generation)
        }
    }

    private func onFragment(batchId: BatchId, index: Int, fragment: [UInt8]) async {
        guard var buffer = fragments[batchId] else { return }
        guard index < buffer.parts.count else {
            fragments.removeValue(forKey: batchId)
            return
        }
        if buffer.parts[index] == nil { buffer.received += 1 }
        buffer.parts[index] = fragment
        if buffer.received < buffer.parts.count {
            fragments[batchId] = buffer
            return
        }
        fragments.removeValue(forKey: batchId)
        var total: [UInt8] = []
        total.reserveCapacity(buffer.totalSize)
        for part in buffer.parts { total.append(contentsOf: part ?? []) }
        await applyRemote(crdt: buffer.crdt, updates: [total])
    }

    /// The edge grants only snapshot-repair capability on this authenticated
    /// socket. Never join, upload deltas, or report readiness until its ACK.
    private func repairIncompleteHistory() async {
        if readOnly {
            blockRecovery("Crew room history is unavailable. Its original cache is retained; this phone cannot republish legacy instructions.")
            return
        }
        joinedLor = false
        serverVersion = nil
        if !recovering { events(.disconnected) }
        recovering = true
        guard !historyRepairAttempted, !doc.isDetached(),
              !doc.oplogVv().isEmpty(), doc.stateVv() == doc.oplogVv(),
              doc.stateVv().includesVv(other: requiredRemoteVersion),
              let snapshot = try? doc.export(mode: .snapshot) else {
            onSocketError(gen: generation)
            return
        }
        historyRepairAttempted = true
        let batchId = BatchId.random()
        historyRepairBatch = batchId
        joinSentAt = .now()  // Bound the repair ACK wait with the join deadline.
        let bytes = [UInt8](snapshot)
        if bytes.count > RoomClient.fragmentBytes {
            await sendFragmented(bytes, batchId: batchId)
        } else {
            await sendBatch([bytes], batchId: batchId)
        }
    }

    private func onAck(crdt: CrdtType, refId: BatchId, status: UpdateStatusCode) async {
        guard crdt == .loro else { return }
        if historyRepairBatch == refId {
            historyRepairBatch = nil
            pending.removeValue(forKey: refId)
            if status == .ok {
                await sendJoinLoro(version: localVersionBytes())
            } else {
                onSocketError(gen: generation)
            }
            return
        }
        guard pending[refId] != nil else { return }
        if status == .ok {
            let acknowledged = pending.removeValue(forKey: refId) ?? []
            let accepted = serverVersion ?? VersionVector()
            for update in acknowledged {
                if let metadata = try? decodeImportBlobMeta(bytes: Data(update), checkChecksum: false) {
                    accepted.merge(other: metadata.partialEndVv)
                }
            }
            serverVersion = accepted
            events(.localChangesAcknowledged(accepted.encode()))
            catchupBatches.remove(refId)
            finishRecoveryIfCaughtUp()
        } else {
            // Every rejected operation remains in the durable document. Retry
            // through bounded socket backoff, never a lifetime counter that
            // leaves a permanently rejected upload showing as connected.
            roomLog.error("room \(self.roomId, privacy: .public): update rejected (\(String(describing: status), privacy: .public)); redialing")
            recovering = true
            if status == .permissionDenied || status == .invalidUpdate || status == .payloadTooLarge {
                blockRecovery("Crew could not publish retained records (\(String(describing: status))). The original intents are retained; resolve the permission or record conflict before retrying.")
            } else { onSocketError(gen: generation) }
        }
    }

    // MARK: Outbound

    /// Called by the doc store on local commit (subscribeLocalUpdate bytes).
    func sendLocalUpdate(_ update: [UInt8]) async {
        guard !readOnly else { return }
        guard joinedLor else {
            // Not lost — the commit is durable in the doc and the next
            // successful join resubmits from VV. But it IS invisible to the
            // user, so say so loudly (2026-07-31: sends queued behind a
            // failing join looked exactly like a working app).
            roomLog.warning("room \(self.roomId, privacy: .public): local update (\(update.count)B) deferred — not joined; will resubmit on join")
            return
        }
        await sendLoroUpdates([update])
    }

    /// Broadcast the presence store's local delta.
    func sendEphemeralUpdate(_ update: [UInt8]) async {
        guard joinedLor, !update.isEmpty else { return }
        await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId, updates: [update], batchId: .random()))
    }

    private func sendJoinLoro(version: [UInt8]) async {
        // Arm the answer deadline BEFORE the frame leaves: an unanswered join
        // used to hang the session forever (room.rs, 2026-07-30). Joins
        // default to non-probe; the probe branch in livenessTick flags itself
        // after this returns.
        joinSentAt = .now()
        joinIsProbe = false
        await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [], version: version))
    }

    /// Batch small updates, fragment any single update above the payload budget.
    private func sendLoroUpdates(_ updates: [[UInt8]], catchup: Bool = false) async {
        var small: [[UInt8]] = []
        var smallBytes = 0
        for update in updates where !update.isEmpty {
            if update.count > RoomClient.fragmentBytes {
                await sendFragmented(update, catchup: catchup)
                continue
            }
            if smallBytes + update.count > RoomClient.fragmentBytes {
                await sendBatch(small, catchup: catchup)
                small = []
                smallBytes = 0
            }
            small.append(update)
            smallBytes += update.count
        }
        if !small.isEmpty { await sendBatch(small, catchup: catchup) }
    }

    private func sendBatch(_ updates: [[UInt8]], batchId: BatchId = .random(), catchup: Bool = false) async {
        pending[batchId] = updates
        if catchup { catchupBatches.insert(batchId) }
        await send(.docUpdate(crdt: .loro, roomId: roomId, updates: updates, batchId: batchId))
    }

    private func sendFragmented(_ update: [UInt8], batchId: BatchId = .random(), catchup: Bool = false) async {
        pending[batchId] = [update]
        if catchup { catchupBatches.insert(batchId) }
        let chunks = stride(from: 0, to: update.count, by: RoomClient.fragmentBytes).map {
            Array(update[$0..<min($0 + RoomClient.fragmentBytes, update.count)])
        }
        await send(.docUpdateFragmentHeader(crdt: .loro, roomId: roomId, batchId: batchId,
                                            fragmentCount: UInt64(chunks.count),
                                            totalSizeBytes: UInt64(update.count)))
        for (ix, chunk) in chunks.enumerated() {
            await send(.docUpdateFragment(crdt: .loro, roomId: roomId, batchId: batchId,
                                          index: UInt64(ix), fragment: chunk))
        }
    }

    private func send(_ message: ProtocolMessage) async {
        #if DEBUG
        if let regressionSend { regressionSend(message); return }
        #endif
        guard let socket, let data = LoroWire.encode(message) else { return }
        let gen = generation
        do { try await socket.send(.data(data)) }
        catch { onSocketError(gen: gen) }
    }

    /// Self-seeded wire fixture, not a remote edge/engine convergence claim.
    @MainActor
    static func runFragmentedBackfillRegression() async -> Bool {
        let seededSessionId = "b9d4882d-a59b-4af6-bbbe-0e3e5c66fedb"
        let source = LoroDoc()
        var expected: [String: String] = [:]
        var random: UInt64 = 0x9e3779b97f4a7c15
        do {
            for index in 0..<40 {
                var entropy: [UInt8] = []
                entropy.reserveCapacity(16_384)
                for _ in 0..<16_384 {
                    random ^= random << 13
                    random ^= random >> 7
                    random ^= random << 17
                    entropy.append(UInt8(truncatingIfNeeded: random))
                }
                let id = "fragmented-\(index)"
                let text = "Crew canonical fragmented fixture \(seededSessionId) message \(index): "
                    + Data(entropy).base64EncodedString()
                expected[id] = text
                let row = try source.getList(id: "messages").pushContainer(child: LoroMap())
                try row.insert(key: "id", v: id)
                try row.insert(key: "role", v: "user")
                try row.insert(key: "createdAt", v: Int64(index + 1))
                try row.insert(key: "deviceId", v: "fixture-host")
                try row.insert(key: "status", v: "complete")
                try row.insert(key: "parts", v: LoroValue.fromJSON([["id": "text", "kind": "text", "text": text]]))
            }
            source.commit()
            let client = RoomClient(roomId: seededSessionId, doc: LoroDoc(), urlProvider: { nil },
                                    events: { _ in }, adoptSnapshot: { _, _ in false })
            let passed = await client.exerciseFragmentedBackfill(source: source, expected: expected)
            await client.stop()
            E2ERunner.log(passed
                ? "OK Crew fragmented backfill: self-seeded canonical session, production wire reassembly, exact distinctive content for every recovered message"
                : "FAIL Crew canonical fragmented backfill fixture")
            return passed
        } catch { E2ERunner.log("FAIL Crew fragmented backfill fixture: \(error)"); return false }
    }

    private func exerciseFragmentedBackfill(source: LoroDoc, expected: [String: String]) async -> Bool {
        guard AppConfig.canonicalSessionId(roomId) == roomId,
              let snapshot = try? source.export(mode: .snapshot), snapshot.count > Self.fragmentBytes else { return false }
        let join = ProtocolMessage.joinResponseOk(crdt: .loro, roomId: roomId, permission: "read",
                                                  version: [UInt8](source.oplogVv().encode()), extra: [])
        guard let joinBytes = LoroWire.encode(join) else { return false }
        await handleInbound(.data(joinBytes), gen: generation)
        guard recovering, doc.getList(id: "messages").len() == 0 else { return false }
        let bytes = [UInt8](snapshot)
        let chunks = stride(from: 0, to: bytes.count, by: Self.fragmentBytes).map {
            Array(bytes[$0..<min($0 + Self.fragmentBytes, bytes.count)])
        }
        let batchId = BatchId.random()
        let header = ProtocolMessage.docUpdateFragmentHeader(crdt: .loro, roomId: roomId, batchId: batchId,
            fragmentCount: UInt64(chunks.count), totalSizeBytes: UInt64(bytes.count))
        guard let headerBytes = LoroWire.encode(header) else { return false }
        await handleInbound(.data(headerBytes), gen: generation)
        for index in chunks.indices.reversed() {
            let frame = ProtocolMessage.docUpdateFragment(crdt: .loro, roomId: roomId, batchId: batchId,
                                                          index: UInt64(index), fragment: chunks[index])
            guard let frameBytes = LoroWire.encode(frame) else { return false }
            await handleInbound(.data(frameBytes), gen: generation)
            if index != 0, !recovering || doc.getList(id: "messages").len() != 0 { return false }
        }
        guard !recovering, doc.stateVv() == source.stateVv(),
              let entries = SessionStore.decodeEntries(from: doc), entries.count == expected.count else { return false }
        return entries.allSatisfy { entry in
            guard entry.parts.count == 1, case .text(_, let text) = entry.parts[0] else { return false }
            return expected[entry.id] == text
        } && Set(entries.map(\.id)) == Set(expected.keys)
    }

    #if DEBUG
    private var regressionSend: ((ProtocolMessage) -> Void)?

    static func runRepeatedRecoveryRegression() async -> Bool {
        for serverSeen in [Int64(200), Int64(400)] {
            let warmClient = RoomClient(roomId: "ws4/warm-regression", doc: LoroDoc(),
                                        urlProvider: { nil }, events: { _ in },
                                        adoptSnapshot: { previous, replacement in
                                            DocDisk.preserveLocalOperations(from: previous, in: replacement)
                                        })
            guard await warmClient.exerciseWarmShallowRecovery(serverSeen: serverSeen) else {
                await E2ERunner.log("FAIL Crew warm shallow cache session/status recovery: seen=\(serverSeen)")
                return false
            }
        }
        let probeClient = RoomClient(roomId: "ws4/probe-regression", doc: LoroDoc(),
                                     urlProvider: { nil }, events: { _ in },
                                     adoptSnapshot: { _, _ in false })
        guard await probeClient.exerciseBroadcastProbe() else {
            await E2ERunner.log("FAIL Crew stale-broadcast foreground recovery")
            return false
        }
        for userId in ["reader-alpha", "reader-beta"] {
            let connectionEvents = OSAllocatedUnfairLock(initialState: [Bool]())
            let client = RoomClient(roomId: "ws4/synthetic-project", doc: LoroDoc(),
                                    urlProvider: { nil }, events: { event in
                                        switch event {
                                        case .connected: connectionEvents.withLock { $0.append(true) }
                                        case .disconnected: connectionEvents.withLock { $0.append(false) }
                                        default: break
                                        }
                                    }, adoptSnapshot: { _, _ in false })
            guard await client.exerciseJoinReadiness(connectionEvents, userId: userId) else {
                await E2ERunner.log("FAIL Crew fresh-principal readiness: \(userId)")
                return false
            }
            guard await client.exercisePendingReadiness(connectionEvents) else {
                await E2ERunner.log("FAIL Crew pending-import readiness: \(userId)")
                return false
            }
        }
        for hasSnapshot in [false, true] {
            let repairClient = RoomClient(roomId: "regression", doc: LoroDoc(),
                                          urlProvider: { nil }, events: { _ in },
                                          adoptSnapshot: { _, _ in false })
            guard await repairClient.exerciseIncompleteHistoryRepair(hasSnapshot: hasSnapshot) else {
                await E2ERunner.log("FAIL Crew incomplete-history recovery: snapshot=\(hasSnapshot)")
                return false
            }
        }
        let shallowClient = RoomClient(roomId: "regression", doc: LoroDoc(),
                                       urlProvider: { nil }, events: { _ in },
                                       adoptSnapshot: { _, _ in false })
        guard await shallowClient.exerciseShallowResubmission() else {
            await E2ERunner.log("FAIL Crew shallow-history resubmission")
            return false
        }
        for status in [UpdateStatusCode.ok, .invalidUpdate, .permissionDenied] {
            let connectionEvents = OSAllocatedUnfairLock(initialState: [Bool]())
            let client = RoomClient(roomId: "regression", doc: LoroDoc(),
                                    urlProvider: { nil }, events: { event in
                                        switch event {
                                        case .connected: connectionEvents.withLock { $0.append(true) }
                                        case .disconnected: connectionEvents.withLock { $0.append(false) }
                                        default: break
                                        }
                                    }, adoptSnapshot: { _, _ in false })
            guard await client.exerciseCatchupAcknowledgement(status, connectionEvents) else {
                await E2ERunner.log("FAIL Crew catch-up acknowledgement: \(status)")
                return false
            }
        }
        let recoveryClient = RoomClient(roomId: "regression", doc: LoroDoc(),
                                        urlProvider: { nil }, events: { _ in },
                                        adoptSnapshot: { _, _ in false })
        guard await recoveryClient.exerciseRepeatedRecovery() else {
            await E2ERunner.log("FAIL Crew repeated room recovery")
            return false
        }
        return true
    }

    private func exerciseWarmShallowRecovery(serverSeen: Int64) async -> Bool {
        var uploads: [([[UInt8]], BatchId)] = []
        regressionSend = { message in
            if case .docUpdate(.loro, _, let bytes, let batch) = message { uploads.append((bytes, batch)) }
        }
        defer { regressionSend = nil; stop() }
        do {
            let source = LoroDoc()
            let chat = try source.getMap(id: "chats").getOrCreateContainer(key: "old", child: LoroMap())
            for (key, value) in ["id": "old", "deviceId": "host", "title": "Cached session"] {
                try chat.insert(key: key, v: value)
            }
            try chat.insert(key: "lastSeenAt", v: Int64(100))
            let ref = try source.getMap(id: "sessionRefs").getOrCreateContainer(key: "old", child: LoroMap())
            try ref.insert(key: "userId", v: "reader")
            try ref.insert(key: "chatId", v: "old")
            try ref.insert(key: "addedAt", v: Int64(1))
            let status = try source.getMap(id: "sessions").getOrCreateContainer(key: "old", child: LoroMap())
            try status.insert(key: "chatId", v: "old")
            try status.insert(key: "deviceId", v: "host")
            try status.insert(key: "status", v: "idle")
            try status.insert(key: "updatedAt", v: Int64(1))
            source.commit()
            doc = LoroDoc()
            _ = try doc.importWith(bytes: source.export(mode: .shallowSnapshot(frontiers: source.stateFrontiers())), origin: "disk")
            guard let localChat = doc.getMap(id: "chats").get(key: "old")?.asLoroMap() else { return false }
            try localChat.insert(key: "lastSeenAt", v: Int64(300))
            doc.commit()

            try chat.insert(key: "lastSeenAt", v: serverSeen)
            source.commit()
            // Two committed advances leave a dependency below the new shallow
            // boundary; a one-op advance can still merge directly into the cache.
            try chat.insert(key: "lastMessageAt", v: Int64(1_000))
            source.commit()
            let baselineVersion = source.oplogVv()
            let baseline = try source.export(mode: .shallowSnapshot(frontiers: source.stateFrontiers()))
            let newest = try source.getMap(id: "chats").getOrCreateContainer(key: "new", child: LoroMap())
            try newest.insert(key: "id", v: "new")
            try newest.insert(key: "deviceId", v: "host")
            try newest.insert(key: "title", v: "Newest recovered session")
            try newest.insert(key: "lastMessageAt", v: Int64(2_000))
            let newRef = try source.getMap(id: "sessionRefs").getOrCreateContainer(key: "new", child: LoroMap())
            try newRef.insert(key: "userId", v: "reader")
            try newRef.insert(key: "chatId", v: "new")
            try newRef.insert(key: "addedAt", v: Int64(2))
            try status.insert(key: "status", v: "working")
            try status.insert(key: "updatedAt", v: Int64(2_000))
            source.commit()
            let tail = try source.export(mode: .updates(from: baselineVersion))
            let receiver = LoroDoc()
            _ = try receiver.importWith(bytes: baseline, origin: "server")
            _ = try receiver.importWith(bytes: tail, origin: "server")

            await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
            guard uploads.isEmpty, recovering else { return false }
            await applyRemote(crdt: .loro, updates: [[UInt8](baseline)])
            guard uploads.isEmpty, recovering,
                  doc.getMap(id: "chats").get(key: "new") == nil else { return false }
            await applyRemote(crdt: .loro, updates: [[UInt8](tail)])
            if serverSeen < 300 {
                guard !uploads.isEmpty, recovering else { return false }
                for (updates, batch) in uploads {
                    for bytes in updates {
                        let imported = try receiver.importWith(bytes: Data(bytes), origin: "phone")
                        guard imported.pending?.isEmpty ?? true else { return false }
                    }
                    await onAck(crdt: .loro, refId: batch, status: .ok)
                }
            } else {
                guard uploads.isEmpty else { return false }
            }
            guard !recovering, let projected = WorkspaceStore.decodeProjection(from: doc, userId: "reader") else { return false }
            return projected.lists.overviewChats.first?.id == "new"
                && projected.chats.first(where: { $0.id == "old" })?.lastSeenAt == max(serverSeen, 300)
                && projected.sessions["old"]?.updatedAt == 2_000
                && effectiveStatus(projected.sessions["old"], now: 2_000) == .working
                && receiver.getMap(id: "chats").get(key: "old")?.asLoroMap()?
                    .get(key: "lastSeenAt")?.asValue()?.i64Value == max(serverSeen, 300)
        } catch {
            await E2ERunner.log("FAIL Crew warm shallow recovery error: \(error)")
            return false
        }
    }

    private func exerciseBroadcastProbe() async -> Bool {
        var joins = 0
        regressionSend = { message in
            if case .joinRequest(.loro, _, _, _) = message { joins += 1 }
        }
        defer { regressionSend = nil; stop() }
        do {
            await onJoinOk(crdt: .loro, version: [])
            let pushed = lastPushedRx
            await handleFrame(.ack(crdt: .loro, roomId: roomId, refId: .random(), status: .ok), gen: generation)
            await livenessTick(gen: generation, at: DispatchTime(
                uptimeNanoseconds: pushed.uptimeNanoseconds + RoomClient.roomProbeAfterNs + 1))
            guard joins == 1, joinIsProbe else { return false }
            await probe() // Outstanding join must not be replaced.
            guard joins == 1 else { return false }
            await handleFrame(.joinResponseOk(crdt: .loro, roomId: roomId, permission: "write",
                                             version: [], extra: []), gen: generation)
            await probe()
            guard joins == 1 else { return false } // Fresh pushes need no probe.
            await probe(force: true) // Foreground cannot wait for the quiet lease.
            guard joins == 2, joinIsProbe else { return false }
            await probe(force: true)
            guard joins == 2 else { return false }
            await handleFrame(.joinResponseOk(crdt: .loro, roomId: roomId, permission: "write",
                                             version: [], extra: []), gen: generation)
            let quiet = DispatchTime(uptimeNanoseconds: lastPushedRx.uptimeNanoseconds
                                     + RoomClient.probeOnDemandMinQuietNs)
            await handleFrame(.ack(crdt: .loro, roomId: roomId, refId: .random(), status: .ok), gen: generation)
            await probe(at: quiet)
            guard joins == 3, joinIsProbe else { return false }

            // The rejoin backfills the missing newest member before readiness.
            let source = LoroDoc()
            let chat = try source.getMap(id: "chats").getOrCreateContainer(key: "newest", child: LoroMap())
            try chat.insert(key: "id", v: "newest")
            try chat.insert(key: "deviceId", v: "host")
            try chat.insert(key: "title", v: "Newest session")
            let ref = try source.getMap(id: "sessionRefs").getOrCreateContainer(key: "member", child: LoroMap())
            try ref.insert(key: "userId", v: "reader")
            try ref.insert(key: "chatId", v: "newest")
            try ref.insert(key: "addedAt", v: Int64(1))
            source.commit()
            await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
            guard recovering else { return false }
            await applyRemote(crdt: .loro, updates: [[UInt8](try source.export(mode: .snapshot))])
            return !recovering && WorkspaceStore.decodeProjection(from: doc, userId: "reader")?
                .lists.overviewChats.first?.title == "Newest session"
        } catch { return false }
    }

    private func exerciseCatchupAcknowledgement(
        _ status: UpdateStatusCode, _ connectionEvents: OSAllocatedUnfairLock<[Bool]>
    ) async -> Bool {
        var batches: [BatchId] = []
        regressionSend = { message in
            if case .docUpdate(.loro, _, _, let batchId) = message { batches.append(batchId) }
        }
        defer { regressionSend = nil; stop() }
        do {
            try doc.getMap(id: "meta").insert(key: "offline", v: "retained")
            doc.commit()
            await onJoinOk(crdt: .loro, version: [])
            guard batches.count == 1, recovering,
                  connectionEvents.withLock({ $0 == [false] }) else { return false }
            // Download coverage alone must not bypass outstanding admission.
            finishRecoveryIfCaughtUp()
            await onAck(crdt: .loro, refId: .random(), status: .ok)
            guard connectionEvents.withLock({ $0 == [false] }) else { return false }
            await onAck(crdt: .loro, refId: batches[0], status: status)
            if status == .ok {
                guard !recovering, connectionEvents.withLock({ $0 == [false, true] }) else { return false }
                let prior = doc.oplogVv()
                try doc.getMap(id: "meta").insert(key: "live", v: "retained")
                doc.commit()
                await sendLocalUpdate([UInt8](try doc.export(mode: .updates(from: prior))))
                guard batches.count == 2, !recovering,
                      connectionEvents.withLock({ $0 == [false, true] }) else { return false }
                await onAck(crdt: .loro, refId: batches[1], status: .permissionDenied)
                guard connectionEvents.withLock({ $0 == [false, true, false] }) else { return false }
            } else {
                guard connectionEvents.withLock({ $0 == [false, false] }) else { return false }
            }
            finishRecoveryIfCaughtUp()
            let permanent = status == .ok || status == .permissionDenied || status == .invalidUpdate || status == .payloadTooLarge
            return recovering && !joinedLor && (permanent ? closed && reconnectTask == nil : reconnectTask != nil)
                && doc.getMap(id: "meta").get(key: "offline")?.asValue()?.stringValue == "retained"
        } catch { return false }
    }

    private func exerciseIncompleteHistoryRepair(hasSnapshot: Bool) async -> Bool {
        var sent: [ProtocolMessage] = []
        regressionSend = { sent.append($0) }
        defer { regressionSend = nil; stop() }
        do {
            if hasSnapshot {
                try doc.getMap(id: "meta").insert(key: "offline", v: "retained")
                doc.commit()
            }
            let failure = ProtocolMessage.joinError(crdt: .loro, roomId: roomId,
                                                   code: .appError, message: "incomplete_history")
            await handleFrame(failure, gen: generation)
            if !hasSnapshot {
                return sent.isEmpty && !joinedLor && recovering && reconnectTask != nil
            }
            guard sent.count == 1,
                  case .docUpdate(_, _, let updates, let batchId) = sent[0],
                  !joinedLor, recovering else { return false }
            let repaired = LoroDoc()
            for update in updates { _ = try repaired.importWith(bytes: Data(update), origin: "regression") }
            guard repaired.getMap(id: "meta").get(key: "offline")?.asValue()?.stringValue == "retained"
            else { return false }
            await onAck(crdt: .loro, refId: .random(), status: .ok)
            guard sent.count == 1 else { return false }
            await onAck(crdt: .loro, refId: batchId, status: .ok)
            guard sent.count == 2,
                  case .joinRequest(.loro, _, _, _) = sent[1],
                  !joinedLor, recovering else { return false }
            // A repeated rejection after the acknowledged attempt redials;
            // it must not upload the same unusable snapshot indefinitely.
            await handleFrame(failure, gen: generation)
            return sent.count == 2 && reconnectTask != nil && !joinedLor
        } catch { return false }
    }

    private func exerciseJoinReadiness(_ connectionEvents: OSAllocatedUnfairLock<[Bool]>, userId: String) async -> Bool {
        regressionSend = { _ in }
        defer { regressionSend = nil }
        do {
            let source = LoroDoc()
            // A cold login and a warm rejoin both receive JoinResponseOk before
            // their backfill. Neither may announce a usable stale/empty replica.
            for turn in 1...2 {
                try source.getMap(id: "meta").insert(key: "title", v: "Joined \(turn)")
                for owner in ["reader-alpha", "reader-beta"] {
                    let id = "\(owner)-\(turn)"
                    let row = try source.getMap(id: "chats").getOrCreateContainer(key: id, child: LoroMap())
                    try row.insert(key: "id", v: id)
                    try row.insert(key: "deviceId", v: "synthetic-host")
                    let ref = try source.getMap(id: "sessionRefs").getOrCreateContainer(key: id, child: LoroMap())
                    try ref.insert(key: "userId", v: owner)
                    try ref.insert(key: "chatId", v: id)
                    try ref.insert(key: "addedAt", v: Int64(turn))
                    let imported = try source.getMap(id: "sessionRefs").getOrCreateContainer(
                        key: "\(id)-imported", child: LoroMap())
                    try imported.insert(key: "userId", v: owner)
                    try imported.insert(key: "chatId", v: "\(id)-imported")
                    try imported.insert(key: "addedAt", v: Int64(turn))
                }
                source.commit()
                connectionEvents.withLock { $0.removeAll() }
                await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
                guard connectionEvents.withLock({ $0 == [false] }) else { return false }
                // Lose the socket before receiving even one backfill frame.
                // A replacement room may advertise only our stale local VV;
                // that must not erase the previous room's advertised target.
                onSocketError(gen: generation)
                reconnectTask?.cancel()
                reconnectTask = nil
                serverVersion = nil
                await onJoinOk(crdt: .loro, version: localVersionBytes())
                guard recovering, connectionEvents.withLock({ $0 == [false, false] }) else { return false }
                await applyRemote(crdt: .loro, updates: [[UInt8](try source.export(mode: .snapshot))])
                guard connectionEvents.withLock({ $0 == [false, false, true] }),
                      doc.getMap(id: "meta").get(key: "title")?.asValue()?.stringValue
                        == "Joined \(turn)" else { return false }
                guard let projection = WorkspaceStore.decodeProjection(from: doc, userId: userId),
                      Set(projection.chats.map(\.id)) == Set((1...turn).map { "\(userId)-\($0)" }),
                      Set(projection.lists.sharedSessionRefs.map(\.chatId))
                        == Set((1...turn).map { "\(userId)-\($0)-imported" }) else { return false }
            }
            return true
        } catch { return false }
    }

    private func exercisePendingReadiness(_ connectionEvents: OSAllocatedUnfairLock<[Bool]>) async -> Bool {
        regressionSend = { _ in }
        defer { regressionSend = nil; stop() }
        do {
            let staleVersion = [UInt8](doc.oplogVv().encode())
            let source = LoroDoc()
            try source.getMap(id: "meta").insert(key: "pending", v: "dependency")
            source.commit()
            let dependencyVersion = source.oplogVv()
            try source.getMap(id: "meta").insert(key: "pending", v: "materialized")
            source.commit()
            connectionEvents.withLock { $0.removeAll() }
            await applyRemote(crdt: .loro, updates: [[UInt8](try source.export(mode: .updates(from: dependencyVersion)))])
            // Neither a stale authoritative reply nor an unrelated complete
            // import proves that the previously pending delta was applied.
            await onJoinOk(crdt: .loro, version: staleVersion)
            let unrelated = LoroDoc()
            try unrelated.getMap(id: "meta").insert(key: "other", v: "complete")
            unrelated.commit()
            await applyRemote(crdt: .loro, updates: [[UInt8](try unrelated.export(mode: .snapshot))])
            guard connectionEvents.withLock({ $0 == [false] }), recovering else { return false }
            await applyRemote(crdt: .loro, updates: [[UInt8](try source.export(mode: .snapshot))])
            return connectionEvents.withLock({ $0 == [false, true] }) && !recovering
                && doc.getMap(id: "meta").get(key: "pending")?.asValue()?.stringValue == "materialized"
        } catch { return false }
    }

    private func exerciseShallowResubmission() async -> Bool {
        var updates: [[UInt8]] = []
        regressionSend = { message in
            if case .docUpdate(let crdt, _, let batch, _) = message, crdt == .loro {
                updates.append(contentsOf: batch)
            }
        }
        defer { regressionSend = nil; stop() }
        do {
            let source = LoroDoc()
            try source.getMap(id: "meta").insert(key: "title", v: "old")
            source.commit()
            let server = source.fork()
            try source.getMap(id: "meta").insert(key: "title", v: "intermediate")
            source.commit()
            try source.getMap(id: "meta").insert(key: "title", v: "compacted")
            source.commit()
            doc = LoroDoc()
            _ = try doc.importWith(bytes: source.export(mode: .shallowSnapshot(frontiers: source.stateFrontiers())), origin: "regression")
            try doc.getMap(id: "meta").insert(key: "offline", v: "retained")
            doc.commit()
            await onJoinOk(crdt: .loro, version: [UInt8](server.oplogVv().encode()))
            let received = LoroDoc()
            for update in updates {
                _ = try received.importWith(bytes: Data(update), origin: "regression")
            }
            return received.stateVv() == doc.stateVv()
                && received.stateVv().includesVv(other: server.stateVv())
                && received.getMap(id: "meta").get(key: "title")?.asValue()?.stringValue == "compacted"
                && received.getMap(id: "meta").get(key: "offline")?.asValue()?.stringValue == "retained"
        } catch {
            await E2ERunner.log("FAIL Crew shallow probe error: \(error)")
            return false
        }
    }

    private func exerciseRepeatedRecovery() async -> Bool {
        var requests = 0
        regressionSend = { message in
            if case .joinRequest(let crdt, _, _, let version) = message,
               crdt == .loro, version.isEmpty { requests += 1 }
        }
        defer { regressionSend = nil; stop() }
        do {
            let source = LoroDoc()
            for turn in 1...5 {
                let before = requests
                // Omit a real dependency, then deliver only the following delta.
                // The server supplies the gap only after a full-snapshot request.
                try source.getMap(id: "meta").insert(key: "title", v: "Missing \(turn)")
                source.commit()
                let missingVersion = source.oplogVv()
                try source.getMap(id: "meta").insert(key: "title", v: "Recovered \(turn)")
                source.commit()
                let delta = try source.export(mode: .updates(from: missingVersion))
                await applyRemote(crdt: .loro, updates: [[UInt8](delta)])
                await applyRemote(crdt: .loro, updates: [[UInt8](delta)])
                guard requests == before + 1 else { return false }
                let snapshot = try source.export(mode: .snapshot)
                if turn.isMultiple(of: 2) {
                    await applyRemote(crdt: .loro, updates: [[UInt8](snapshot)])
                    await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
                } else {
                    await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
                    // The failed backfill coalesced before the reply must now
                    // have issued its deferred heal, without another event.
                    guard requests == before + 2 else { return false }
                    await applyRemote(crdt: .loro, updates: [[UInt8](snapshot)])
                    await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
                }
                guard doc.getMap(id: "meta").get(key: "title")?.asValue()?.stringValue
                    == "Recovered \(turn)", !recovering else { return false }
            }
            // An authoritative caught-up reply needs no additional backfill.
            await requestFullSnapshot()
            await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
            guard !recovering, fullResyncs == 0 else { return false }
            try source.getMap(id: "meta").insert(key: "title", v: "Still missing")
            source.commit()
            // Successful heals above must not consume a lifetime allowance.
            // Failed replies below must not reset backoff just by answering joins.
            let beforeFailures = requests
            for _ in 0..<RoomClient.maxFullResyncs {
                await applyRemote(crdt: .loro, updates: [[0]])
                await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
            }
            await applyRemote(crdt: .loro, updates: [[0]])
            guard requests == beforeFailures + RoomClient.maxFullResyncs,
                  reconnectTask != nil, recovering else { return false }
            reconnectTask?.cancel()
            let recoveryBackoff = backoffMs
            await onJoinOk(crdt: .loro, version: [UInt8](source.oplogVv().encode()))
            return backoffMs == recoveryBackoff && backoffMs > RoomClient.backoffBaseMs
        } catch { return false }
    }

    static func runForegroundBlockedRegression() async -> Bool {
        let blocked = OSAllocatedUnfairLock(initialState: [String]())
        let client = RoomClient(roomId: UUID().uuidString.lowercased(), doc: LoroDoc(),
            urlProvider: { nil }, events: { event in
                if case .recoveryBlocked(let message) = event { blocked.withLock { $0.append(message) } }
            }, adoptSnapshot: { _, _ in false })
        let blockedClient = RoomClient(roomId: UUID().uuidString.lowercased(), doc: LoroDoc(),
            urlProvider: { nil }, events: { event in
                if case .recoveryBlocked(let message) = event { blocked.withLock { $0.append(message) } }
            }, adoptSnapshot: { _, _ in false })
        guard await client.exerciseBroadcastProbe(),
              await blockedClient.exerciseBlockedSnapshot(blocked) else {
            await E2ERunner.log("FAIL Crew foreground and blocked recovery")
            return false
        }
        await E2ERunner.log("OK Crew foreground and blocked recovery: stale broadcast probe, ACK isolation, conflict visible, no automatic blocked retry")
        return true
    }

    private func exerciseBlockedSnapshot(_ blocked: OSAllocatedUnfairLock<[String]>) async -> Bool {
        regressionSend = { _ in }
        defer { regressionSend = nil; stop() }
        do {
            let source = LoroDoc()
            try source.getMap(id: "meta").insert(key: "current", v: "authoritative")
            source.commit()
            let previous = doc
            snapshotRecovery = DocDisk.replacementSnapshot(bytes: try source.export(mode: .snapshot))
            serverVersion = source.oplogVv()
            requiredRemoteVersion.merge(other: source.oplogVv())
            joinedLor = true; closed = false
            guard !(await adoptRecoveredSnapshotIfCaughtUp()), closed,
                  doc === previous, blocked.withLock({ $0.count == 1 }), reconnectTask == nil else { return false }
            await probe()
            return closed && reconnectTask == nil && blocked.withLock({ $0.count == 1 })
        } catch { return false }
    }
    #endif

    private func localVersionBytes() -> [UInt8] {
        let vv = doc.oplogVv()
        return vv.isEmpty() ? [] : [UInt8](vv.encode())
    }
}

private extension VersionVector {
    func isEmpty() -> Bool {
        // An empty VV encodes to a fixed small header with no entries; the
        // cheapest reliable emptiness probe the FFI exposes is comparing
        // against a fresh VV.
        self == VersionVector()
    }
}
