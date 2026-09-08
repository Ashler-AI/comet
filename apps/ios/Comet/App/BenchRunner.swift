// Transcript load benchmark — launch with `-bench`. Builds a synthetic session
// doc the size of a long agent transcript and times import, projection and
// row preparation. The production cache builds on a worker actor; its heartbeat
// probe measures main-actor responsiveness while that work is in flight.
// Results append to Documents/bench.log for simctl to read.

import Foundation
import Loro

@MainActor
enum BenchRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("bench.log")
    }

    static func log(_ line: String) {
        print("BENCH: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    static func run() async {
        try? FileManager.default.removeItem(at: logURL)
        for turns in [50, 200, 500] {
            await measure(turns: turns)
        }
        await measureWorkspace()
        await measureHydration()
        log("done")
    }

    // MARK: Measurement

    private static func measure(turns: Int) async {
        let doc = buildDoc(turns: turns)
        let snapshot = (try? doc.export(mode: .snapshot)) ?? Data()
        let bytes = snapshot.count

        // Raw import cost; production hydration now performs this off-main.
        let importMs = best(3) {
            _ = try? LoroDoc().importWith(bytes: snapshot, origin: "disk")
        }

        // Raw projection cost; production runs the decoder off-main.
        var entries: [MessageEntry] = []
        let decode = time { entries = SessionStore.decodeEntries(from: doc) ?? [] }

        // Stage 2 — cold row build (empty caches). The OLD per-rebuild cost.
        var rowCount = 0
        let cold = best(3) {
            var parsers: [String: IncrementalMarkdownParser] = [:]
            var memo: [String: CompletedParse] = [:]
            let rows = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                                 parsers: &parsers, completed: &memo)
            rowCount = rows.count
        }

        // Stage 3 — warm rebuild: memo primed, as after any doc update.
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var memo: [String: CompletedParse] = [:]
        _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                      parsers: &parsers, completed: &memo)
        let warm = best(5) {
            _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                          parsers: &parsers, completed: &memo)
        }

        // Stage 4 — cold preparation on the worker, with a main-actor heartbeat.
        let cache = TranscriptBuilderCache()
        // Exercise this worker and the task runtime before collecting timing.
        // Clearing its rows at a new revision also clears parser memos, so the
        // measured history remains a cold parse rather than a warmed-cache hit.
        await cache.update(revision: 0, entries: Array(entries.prefix(2)), pendingSends: [])
        await cache.update(revision: 1, entries: [], pendingSends: [])
        try? await Task.sleep(nanoseconds: 10_000_000)
        let heartbeat = Task { @MainActor in
            var ticks = 0
            var largestGap = 0.0
            var previous = CFAbsoluteTimeGetCurrent()
            while !Task.isCancelled {
                do { try await Task.sleep(nanoseconds: 1_000_000) } catch { break }
                let now = CFAbsoluteTimeGetCurrent()
                largestGap = max(largestGap, (now - previous) * 1000)
                previous = now
                ticks += 1
            }
            return (ticks, largestGap)
        }
        await Task.yield()
        let started = CFAbsoluteTimeGetCurrent()
        await cache.update(revision: 2, entries: entries, pendingSends: [])
        let prepared = (CFAbsoluteTimeGetCurrent() - started) * 1000
        heartbeat.cancel()
        let (ticks, largestGap) = await heartbeat.value
        let cached = best(5) { _ = cache.rows.count }
        let reopened = CFAbsoluteTimeGetCurrent()
        await cache.update(revision: 2, entries: entries, pendingSends: [])
        let reopenMs = (CFAbsoluteTimeGetCurrent() - reopened) * 1000

        log("--- \(turns) turns · \(entries.count) entries · \(rowCount) rows · \(bytes / 1024) KB snapshot")
        log(String(format: "disk import raw         %8.2f ms", importMs))
        log(String(format: "decode raw              %8.2f ms", decode))
        log(String(format: "row build cold sync     %8.2f ms", cold))
        log(String(format: "row build warm sync     %8.2f ms", warm))
        log(String(format: "row prepare warmed worker %8.2f ms", prepared))
        log(String(format: "warmed heartbeat max gap %8.2f ms · %d ticks", largestGap, ticks))
        log(String(format: "retained reopen         %8.4f ms", reopenMs))
        log(String(format: "scroll row access       %8.4f ms", cached))
    }

    /// Offline surface fixture: real list rows/navigation, no live account data.
    static func populateList(demo: DemoDataset) {
        guard let template = demo.chats.first else { return }
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        demo.chats = (0..<600).map { index in
            var chat = template
            chat.id = String(format: "perf-%03d", index)
            chat.title = String(format: "Crew performance session %03d", index)
            chat.archived = false
            chat.createdAt = now - Int64(index * 1_000)
            chat.lastMessageAt = chat.createdAt
            chat.lastSeenAt = chat.createdAt
            return chat
        }
        // Stress one cold transcript on navigation; the rest remain cheap rows.
        if let first = demo.chats.first {
            demo.sessionStore(for: first.id).setEntries(syntheticEntries(turns: 500))
        }
    }

    private static func measureWorkspace() async {
        let doc = LoroDoc()
        let devices = doc.getMap(id: "devices")
        let spaces = doc.getMap(id: "spaces")
        let chats = doc.getMap(id: "chats")
        let refs = doc.getMap(id: "sessionRefs")
        for index in 0..<20 {
            let device = try! devices.insertContainer(key: "device-\(index)", child: LoroMap())
            try! device.insert(key: "id", v: "device-\(index)")
            try! device.insert(key: "name", v: "Device \(index)")
            try! device.insert(key: "platform", v: "macos")
            let space = try! spaces.insertContainer(key: "space-\(index)", child: LoroMap())
            try! space.insert(key: "id", v: "space-\(index)")
            try! space.insert(key: "deviceId", v: "device-\(index)")
            try! space.insert(key: "path", v: "/bench/space-\(index)")
            try! space.insert(key: "createdAt", v: Int64(index))
        }
        for index in 0..<600 {
            let id = "chat-\(index)"
            let chat = try! chats.insertContainer(key: id, child: LoroMap())
            try! chat.insert(key: "id", v: id)
            try! chat.insert(key: "deviceId", v: "device-\(index % 20)")
            try! chat.insert(key: "spaceId", v: "space-\(index % 20)")
            try! chat.insert(key: "title", v: "Crew performance session \(index)")
            try! chat.insert(key: "archived", v: index % 5 == 0)
            try! chat.insert(key: "createdAt", v: Int64(index))
            try! chat.insert(key: "lastMessageAt", v: Int64(index * 2))
            let ref = try! refs.insertContainer(key: id, child: LoroMap())
            try! ref.insert(key: "chatId", v: id)
            try! ref.insert(key: "userId", v: "bench")
            try! ref.insert(key: "addedAt", v: Int64(index))
        }
        doc.commit()
        let begin = CFAbsoluteTimeGetCurrent()
        guard let projection = await Task.detached(priority: .userInitiated, operation: {
            WorkspaceStore.decodeProjection(from: doc, userId: "bench")
        }).value else { log("FAIL workspace projection"); return }
        let decodeMs = (CFAbsoluteTimeGetCurrent() - begin) * 1000
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:9")!, mode: .dev,
                               userId: "bench", projectScope: "bench", deviceId: "bench", deviceName: "Bench")
        let workspace = WorkspaceStore(config: config)
        workspace.applyProjection(projection)
        var legacyCount = 0
        let legacyMs = best(20) {
            let active = sessionListChats(projection.chats, archived: false)
            let archived = sessionListChats(projection.chats, archived: true)
            let occupied = Set(active.compactMap(\.spaceId))
            var count = active.count + archived.count
            for space in projection.spaces where occupied.contains(space.id) {
                count += sortActive(projection.chats.filter { !$0.archived && $0.spaceId == space.id }).count
            }
            for chat in active {
                if projection.spaces.first(where: { $0.id == chat.spaceId }) != nil { count += 1 }
                if projection.devices.first(where: { $0.id == chat.deviceId }) != nil { count += 1 }
                if projection.sessionRefs.first(where: { $0.chatId == chat.id }) != nil { count += 1 }
            }
            legacyCount = count
        }
        var cachedCount = 0
        let indexedMs = best(20) {
            var count = workspace.overviewChats.count + workspace.settledChats.count
            for space in workspace.occupiedSpaces { count += workspace.chats(in: space.id).count }
            for chat in workspace.overviewChats {
                if workspace.space(id: chat.spaceId ?? "") != nil { count += 1 }
                if workspace.device(id: chat.deviceId) != nil { count += 1 }
                if workspace.sessionRef(id: chat.id) != nil { count += 1 }
            }
            cachedCount = count
        }
        log(String(format: "workspace 600 rows decode off-main %.2f ms · list pass legacy %.2f ms indexed %.2f ms", decodeMs, legacyMs, indexedMs))
        log(legacyCount == cachedCount && workspace.chats.count == 600
            ? "PASS indexed list retains all 600 sessions and matching row context"
            : "FAIL indexed list context mismatch")
    }

    private static func measureHydration() async {
        let id = "bench-hydration-\(UUID().uuidString)"
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:9")!, mode: .dev,
                               userId: "bench", projectScope: "bench", deviceId: "bench", deviceName: "Bench")
        let cacheId = config.documentCacheId(roomId: id)
        let doc = buildDoc(turns: 500)
        DocDisk.save(doc: doc, id: cacheId)
        defer { try? FileManager.default.removeItem(at: DocDisk.url(for: cacheId)) }
        let store = SessionStore(chatId: id, config: config, metadataOnly: true)
        let startMs = time { store.start() }
        store.activateTranscript()
        let began = CFAbsoluteTimeGetCurrent()
        for _ in 0..<5_000 {
            if !store.isHydrating && !store.isProjecting && store.entries.count == 1_000 { break }
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
        let elapsedMs = (CFAbsoluteTimeGetCurrent() - began) * 1000
        let revision = store.revision
        let projections = store.projectionCount
        for _ in 0..<100 { store.project() }
        while store.isProjecting { try? await Task.sleep(nanoseconds: 1_000_000) }
        log(String(format: "cached session start main %.3f ms · ready %.2f ms · %d entries", startMs, elapsedMs, store.entries.count))
        log(store.entries.count == 1_000 && store.revision == revision && store.projectionCount == projections
            ? "PASS hydration activation and unchanged projections preserve transcript/revision"
            : "FAIL hydration activation or redundant projection")
        store.stop()
        let restartMs = time { store.start() }
        log(String(format: "warm session restart main %.3f ms", restartMs))
        store.stop()

        let cancelled = SessionStore(chatId: id, config: config)
        cancelled.start()
        cancelled.stop()
        try? await Task.sleep(nanoseconds: 100_000_000)
        log(cancelled.entries.isEmpty && !cancelled.hydrationComplete
            ? "PASS stopped hydration cannot adopt cached history" : "FAIL stopped hydration published")
        cancelled.start()
        cancelled.updateDeploymentId("different-deployment")
        for _ in 0..<1_000 {
            if !cancelled.isHydrating && !cancelled.isProjecting { break }
            try? await Task.sleep(nanoseconds: 1_000_000)
        }
        log(cancelled.entries.isEmpty ? "PASS deployment switch rejects old cached history" : "FAIL cross-deployment history")
        cancelled.stop()
    }

    private static func time(_ body: () -> Void) -> Double {
        let t0 = CFAbsoluteTimeGetCurrent()
        body()
        return (CFAbsoluteTimeGetCurrent() - t0) * 1000
    }

    /// Best-of-n: the floor is the honest number for cache behaviour (noise
    /// only ever adds).
    private static func best(_ n: Int, _ body: () -> Void) -> Double {
        var lowest = Double.greatestFiniteMagnitude
        for _ in 0..<n { lowest = min(lowest, time(body)) }
        return lowest
    }

    /// A big synthetic transcript for the `-big` demo route — stresses the
    /// scroll-settle path with far more lazy rows than the demo dataset has.
    static func syntheticEntries(turns: Int) -> [MessageEntry] {
        SessionStore.decodeEntries(from: buildDoc(turns: turns)) ?? []
    }

    // MARK: Synthetic doc (schema.rs shape — see SessionStore.entryFrom)

    private static func buildDoc(turns: Int) -> LoroDoc {
        let doc = LoroDoc()
        let messages = doc.getList(id: "messages")
        for i in 0..<turns {
            let user = try! messages.pushContainer(child: LoroMap())
            try! user.insert(key: "id", v: "u\(i)")
            try! user.insert(key: "role", v: "user")
            try! user.insert(key: "createdAt", v: Int64(i * 1000))
            try! user.insert(key: "deviceId", v: "bench")
            try! user.insert(key: "status", v: "complete")
            let uparts = try! user.insertContainer(key: "parts", child: LoroList())
            try! addText(to: uparts, id: "t0", text: "Turn \(i): the ref dropdown still hangs on open — dig into it.")

            let bot = try! messages.pushContainer(child: LoroMap())
            try! bot.insert(key: "id", v: "a\(i)")
            try! bot.insert(key: "role", v: "assistant")
            try! bot.insert(key: "createdAt", v: Int64(i * 1000 + 1))
            try! bot.insert(key: "deviceId", v: "dev-mac")
            try! bot.insert(key: "status", v: "complete")
            let aparts = try! bot.insertContainer(key: "parts", child: LoroList())
            try! addText(to: aparts, id: "t0", text: prose(i))
            for t in 0..<4 {
                try! addTool(to: aparts, id: "k\(i).\(t)", index: i * 4 + t)
            }
            try! addText(to: aparts, id: "t1", text: closing(i))
        }
        return doc
    }

    private static func addText(to parts: LoroList, id: String, text: String) throws {
        let p = try parts.pushContainer(child: LoroMap())
        try p.insert(key: "id", v: id)
        try p.insert(key: "kind", v: "text")
        try p.insert(key: "text", v: text)
    }

    private static func addTool(to parts: LoroList, id: String, index: Int) throws {
        let p = try parts.pushContainer(child: LoroMap())
        try p.insert(key: "id", v: id)
        try p.insert(key: "kind", v: "tool")
        try p.insert(key: "isError", v: index % 17 == 0)
        let call = try p.insertContainer(key: "call", child: LoroMap())
        switch index % 4 {
        case 0:
            try call.insert(key: "kind", v: "exec")
            try call.insert(key: "command", v: "rg -n 'refDropdown' src/components --glob '!*.test.ts'")
        case 1:
            try call.insert(key: "kind", v: "readFile")
            try call.insert(key: "path", v: "src/components/refs/RefDropdown.tsx")
        case 2:
            try call.insert(key: "kind", v: "editFile")
            try call.insert(key: "path", v: "src/components/refs/useRefIndex.ts")
        default:
            try call.insert(key: "kind", v: "search")
            try call.insert(key: "pattern", v: "loadRefs\\(")
        }
    }

    /// A realistic assistant block: headings, prose, a list, a table, code.
    private static func prose(_ i: Int) -> String {
        """
        ## Pass \(i): where the dropdown stalls

        The dropdown's open handler awaits `loadRefs()` **before** it paints, so
        the menu can't render until the full ref index resolves. On a repo with
        many refs that's a visible hang, and it is paid again on every open
        because the result is never memoized between mounts.

        Three things stack up here:

        1. `loadRefs()` walks every ref and builds a fresh array each call
        2. The handler `await`s it inline instead of rendering an empty menu
        3. `useRefIndex` has no cache, so remount re-does the whole walk

        | Stage | Cost | Cached |
        | --- | --- | --- |
        | `loadRefs` | O(refs) | no |
        | `useRefIndex` | O(refs) | no |
        | paint | O(visible) | n/a |

        > The fix is to paint first and fill in — the index can arrive late.

        ```ts
        const refs = useRefIndex()          // memoized, suspense-free
        useEffect(() => { void warmRefIndex() }, [])
        return <Menu items={refs ?? []} loading={refs == null} />
        ```
        """
    }

    private static func closing(_ i: Int) -> String {
        """
        Landed the pass-\(i) change behind `refIndexCache`. Open latency drops to
        a paint, and the index warms in the background on first hover.
        """
    }
}
