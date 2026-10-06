// Device-room relay RPC client — dials a device's room on the edge as a
// `client` peer and speaks ControlRpc to the HOST engine over a virtual
// socket (crates/rpc/src/device_room.rs + edge/src/device-room.ts).
//
// Frame codec (binary WS messages): uleb128(headerLen) ‖ headerJSON ‖ payload.
// Header key order MUST be {"s","k","to","from"} (byte parity with both
// implementations); clients never set `to`/`from` — the DO stamps `from`.
// RPC payloads are ndjson ControlRpc frames: {id, method, params} out,
// {id, ok|err|item|done} back. Relay control frames (kind " relay" — leading
// space is part of the constant) signal host_offline/host_closed.

import Foundation

enum RelayError: LocalizedError {
    case notConnected
    case hostOffline
    case rpc(String)
    case timeout

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected to the device"
        case .hostOffline: return "The device is offline"
        case .rpc(let message): return message
        case .timeout: return "The device didn't respond"
        }
    }
}

actor DeviceRelayClient {
    static let rpcKind = "rpc"
    static let relayKind = " relay"  // leading space is intentional

    private let deviceId: String
    private let config: AppConfig
    private let controlSessionId: String?
    private let controlDeploymentId: String?

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<Result<Data, RelayError>, Never>] = [:]
    private var connected = false
    private var generation: UInt64 = 0
    #if DEBUG
    private var regressionTokenGate: (expected: Int, ready: CheckedContinuation<Void, Never>,
                                      waiters: [CheckedContinuation<Void, Never>])?
    private var regressionNoNetwork = false
    private var regressionPing: ((URLSessionWebSocketTask) -> Void)?
    #endif

    init(deviceId: String, config: AppConfig, controlSessionId: String? = nil,
         controlDeploymentId: String? = nil) {
        self.deviceId = deviceId
        self.config = config
        self.controlSessionId = controlSessionId
        self.controlDeploymentId = controlDeploymentId
    }

    // MARK: Lifecycle

    private func connect() async throws -> URLSessionWebSocketTask {
        if connected, let socket { return socket }
        if let controlSessionId, AppConfig.canonicalSessionId(controlSessionId) != controlSessionId {
            throw RelayError.rpc("Crew control requires a canonical public session identity.")
        }
        if controlDeploymentId != nil, controlSessionId == nil {
            throw RelayError.rpc("Crew control requires an exact session scope for this deployment.")
        }
        let startingGeneration = generation
        #if DEBUG
        if regressionTokenGate != nil {
            await withCheckedContinuation { waiter in
                regressionTokenGate?.waiters.append(waiter)
                if let gate = regressionTokenGate, gate.waiters.count == gate.expected {
                    gate.ready.resume()
                }
            }
        }
        #endif
        guard let token = await config.currentToken() else { throw RelayError.notConnected }
        try Task.checkCancellation()
        if connected, let socket { return socket }
        guard generation == startingGeneration else { throw RelayError.notConnected }
        var components = URLComponents(url: config.edgeURL.appending(path: "device/\(deviceId)/ws"),
                                       resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        var queryItems = [
            URLQueryItem(name: "role", value: "client"),
            URLQueryItem(name: "syncProtocol", value: AppConfig.durableSyncProtocol),
            // A reconnect is a new relay peer. Reusing a connId can briefly
            // leave two tagged sockets in the hibernating DO and route the
            // host's response to the stale predecessor.
            URLQueryItem(name: "connId", value: UUID().uuidString.lowercased()),
        ]
        if let controlSessionId {
            queryItems += [
                URLQueryItem(name: "purpose", value: "control"),
                URLQueryItem(name: "controlSessionId", value: controlSessionId),
            ]
            if let controlDeploymentId {
                queryItems.append(URLQueryItem(name: "controlDeploymentId", value: controlDeploymentId))
            }
        }
        queryItems.append(URLQueryItem(name: "token", value: token))
        components.queryItems = queryItems
        let task = URLSession.shared.webSocketTask(with: components.url!)
        socket = task
        generation &+= 1
        let gen = generation
        #if DEBUG
        if regressionNoNetwork { connected = true; return task }
        #endif
        task.resume()
        connected = true

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                do {
                    let message = try await task.receive()
                    await self.handleInbound(message, generation: gen)
                } catch {
                    await self.teardown(error: .hostOffline, generation: gen)
                    return
                }
            }
        }
        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 30_000_000_000)
                guard !Task.isCancelled, let self else { return }
                await self.sendPing(socket: task, generation: gen)
            }
        }
        return task
    }

    func close() {
        teardown(error: .notConnected)
    }

    private func teardown(error: RelayError, generation expected: UInt64? = nil) {
        guard expected == nil || expected == generation else { return }
        generation &+= 1
        receiveTask?.cancel()
        pingTask?.cancel()
        receiveTask = nil
        pingTask = nil
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connected = false
        let waiting = pending
        pending.removeAll()
        for (_, continuation) in waiting {
            continuation.resume(returning: .failure(error))
        }
    }

    private func sendPing(socket: URLSessionWebSocketTask, generation gen: UInt64) async {
        guard gen == generation, self.socket === socket else { return }
        #if DEBUG
        if let regressionPing { regressionPing(socket); return }
        #endif
        try? await socket.send(.string("ping"))
    }

    // MARK: RPC

    /// One unary ControlRpc call. Ordinary device operations use a 10-second
    /// deadline; Scaffold control can opt into its longer bootstrap window.
    func call<Response: Decodable>(method: String, params: [String: Any],
                                   timeoutNanoseconds: UInt64? = 10_000_000_000,
                                   preserveSuccessfulResponseOnCancellation: Bool = false) async throws -> Response {
        let data = try await callData(method: method, params: params,
            timeoutNanoseconds: timeoutNanoseconds,
            preserveSuccessfulResponseOnCancellation: preserveSuccessfulResponseOnCancellation)
        return try JSONDecoder().decode(Response.self, from: data)
    }

    func callJSON(method: String, params: [String: Any]) async throws -> [String: Any] {
        let data = try await callData(method: method, params: params)
        guard let reply = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw RelayError.rpc("Crew command readback did not return an object.")
        }
        return reply
    }

    private func callData(method: String, params: [String: Any],
                          timeoutNanoseconds: UInt64? = 10_000_000_000,
                          preserveSuccessfulResponseOnCancellation: Bool = false) async throws -> Data {
        for attempt in 0..<3 {
            try Task.checkCancellation()
            do {
                return try await callOnce(method: method, params: params,
                                          timeoutNanoseconds: timeoutNanoseconds,
                                          preserveSuccessfulResponseOnCancellation: preserveSuccessfulResponseOnCancellation)
            } catch let error as RelayError {
                guard attempt < 2 else { throw error }
                switch error {
                case .hostOffline, .notConnected:
                    try await Task.sleep(nanoseconds: UInt64(attempt + 1) * 250_000_000)
                case .rpc, .timeout:
                    throw error
                }
            }
        }
        throw RelayError.notConnected
    }

    private func callOnce(
        method: String,
        params: [String: Any],
        timeoutNanoseconds: UInt64?,
        preserveSuccessfulResponseOnCancellation: Bool
    ) async throws -> Data {
        let socket = try await connect()
        guard connected, self.socket === socket else { throw RelayError.notConnected }
        let gen = generation
        defer {
            // One-shot command authority belongs only to this connection.
            if controlSessionId != nil { teardown(error: .notConnected, generation: gen) }
        }
        let id = nextId
        nextId += 1
        // Always send a params object — the engine's serde rejects a missing
        // field even when every param is optional (ListFolders home listing).
        let frame: [String: Any] = ["id": id, "method": method, "params": params]
        let payload = try JSONSerialization.data(withJSONObject: frame)
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)

        // Install the waiter before sending. URLSession's async send may yield
        // long enough for a fast host reply to reach handleInbound; registering
        // afterward loses that reply and turns a successful call into a timeout.
        let result: Result<Data, RelayError> = await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                pending[id] = continuation
                Task {
                    guard self.pending[id] != nil else { return }
                    await self.send(data, for: id, socket: socket, generation: gen)
                }
                if let timeoutNanoseconds {
                    Task {
                        try? await Task.sleep(nanoseconds: timeoutNanoseconds)
                        self.timeoutCall(id: id)
                    }
                }
                if Task.isCancelled { self.cancelCall(id: id) }
            }
        } onCancel: {
            Task { await self.cancelCall(id: id) }
        }
        if !preserveSuccessfulResponseOnCancellation {
            try Task.checkCancellation()
        }
        switch result {
        case .failure(let error):
            try Task.checkCancellation()
            throw error
        case .success(let ok):
            return ok
        }
    }

    private func send(_ data: Data, for id: UInt64, socket: URLSessionWebSocketTask, generation gen: UInt64) async {
        guard gen == generation, self.socket === socket else {
            failCall(id: id, error: .notConnected)
            return
        }
        do {
            try await socket.send(.data(data))
        } catch {
            failCall(id: id, error: .notConnected)
            teardown(error: .notConnected, generation: gen)
        }
    }

    private func failCall(id: UInt64, error: RelayError) {
        if let continuation = pending.removeValue(forKey: id) {
            continuation.resume(returning: .failure(error))
        }
    }

    private func timeoutCall(id: UInt64) {
        failCall(id: id, error: .timeout)
    }

    private func cancelCall(id: UInt64) {
        guard pending[id] != nil else { return }
        failCall(id: id, error: .timeout)
        let frame: [String: Any] = ["id": id, "cancel": true, "params": [:]]
        guard let payload = try? JSONSerialization.data(withJSONObject: frame) else { return }
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)
        let socket = self.socket
        Task { try? await socket?.send(.data(data)) }
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, generation gen: UInt64) {
        guard gen == generation else { return }
        switch message {
        case .string:
            return  // "pong"
        case .data(let data):
            guard let (header, payload) = Self.decodeFrame(data) else { return }
            switch header.k {
            case Self.rpcKind:
                handleRpcPayload(payload)
            case Self.relayKind:
                // {"error":"host_offline"|"host_closed"|...} — link down.
                teardown(error: .hostOffline, generation: gen)
            default:
                return
            }
        @unknown default:
            return
        }
    }

    private func handleRpcPayload(_ payload: Data) {
        // ndjson: each line is one ServerFrame.
        guard let text = String(data: payload, encoding: .utf8) else { return }
        for line in text.split(separator: "\n") {
            guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                  let id = (obj["id"] as? NSNumber)?.uint64Value,
                  let continuation = pending.removeValue(forKey: id) else { continue }
            if let err = obj["err"] as? String {
                continuation.resume(returning: .failure(.rpc(err)))
            } else if obj.keys.contains("ok"),
                      let okData = try? JSONSerialization.data(withJSONObject: obj["ok"] ?? NSNull(),
                                                               options: .fragmentsAllowed) {
                continuation.resume(returning: .success(okData))
            } else {
                continuation.resume(returning: .failure(.rpc("unexpected reply")))
            }
        }
    }

    // MARK: Frame codec

    struct FrameHeader: Decodable {
        var s: String?
        var k: String?
        var to: String?
        var from: String?
    }

    static func encodeFrame(header: String, payload: Data) -> Data {
        let headerBytes = Data(header.utf8)
        var out = Data()
        var len = UInt64(headerBytes.count)
        repeat {
            var byte = UInt8(len & 0x7f)
            len >>= 7
            if len != 0 { byte |= 0x80 }
            out.append(byte)
        } while len != 0
        out.append(headerBytes)
        out.append(payload)
        return out
    }

    static func decodeFrame(_ data: Data) -> (FrameHeader, Data)? {
        var offset = 0
        var length: UInt64 = 0
        var shift: UInt64 = 0
        let bytes = [UInt8](data)
        while offset < bytes.count {
            let byte = bytes[offset]
            offset += 1
            length |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { break }
            shift += 7
            if shift > 28 { return nil }
        }
        guard offset + Int(length) <= bytes.count else { return nil }
        let headerData = Data(bytes[offset..<offset + Int(length)])
        guard let header = try? JSONDecoder().decode(FrameHeader.self, from: headerData) else { return nil }
        let payload = Data(bytes[(offset + Int(length))...])
        return (header, payload)
    }
}

#if DEBUG
extension DeviceRelayClient {
    static func runConnectionGenerationRegression() async -> Bool {
        let config = AppConfig(edgeURL: URL(string: "http://127.0.0.1:1")!, mode: .dev,
                               userId: "relay-regression", projectScope: "relay-regression",
                               deviceId: "phone", deviceName: "Crew regression", devBearer: "fixture-only")
        let client = DeviceRelayClient(deviceId: "host", config: config)
        let passed = await client.exerciseConnectionGenerations()
        await E2ERunner.log(passed
            ? "OK Crew relay lifecycle: concurrent cold connect, stale receive/ping/error isolation, close during token wait"
            : "FAIL Crew relay connection generation isolation")
        return passed
    }

    private func releaseRegressionTokenGate() {
        guard let gate = regressionTokenGate else { return }
        regressionTokenGate = nil
        gate.waiters.forEach { $0.resume() }
    }

    private func exerciseConnectionGenerations() async -> Bool {
        regressionNoNetwork = true
        defer { releaseRegressionTokenGate(); regressionPing = nil; close() }
        var first: Task<URLSessionWebSocketTask, Error>?
        var second: Task<URLSessionWebSocketTask, Error>?
        await withCheckedContinuation { ready in
            regressionTokenGate = (2, ready, [])
            first = Task { try await self.connect() }
            second = Task { try await self.connect() }
        }
        releaseRegressionTokenGate()
        do {
            let original = try await first!.value
            let shared = try await second!.value
            guard original === shared, socket === original, generation == 1 else { return false }

            let oldGeneration = generation
            close()
            let replacement = try await connect()
            let newGeneration = generation
            var stalePreservedWaiter = false
            let result: Result<Data, RelayError> = await withCheckedContinuation { continuation in
                pending[42] = continuation
                teardown(error: .hostOffline, generation: oldGeneration)
                let relayError = Self.encodeFrame(header: #"{"s":"rpc","k":" relay"}"#, payload: Data())
                handleInbound(.data(relayError), generation: oldGeneration)
                let staleReply = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: Data(#"{"id":42,"err":"stale peer"}"#.utf8))
                handleInbound(.data(staleReply), generation: oldGeneration)
                stalePreservedWaiter = pending[42] != nil && socket === replacement && generation == newGeneration
                close()
            }
            guard stalePreservedWaiter, case .failure(.notConnected) = result else { return false }
            // A late ping from the old socket must not send on the replacement.
            let current = try await connect()
            let currentGeneration = generation
            var pings: [URLSessionWebSocketTask] = []
            regressionPing = { pings.append($0) }
            await sendPing(socket: original, generation: oldGeneration)
            guard pings.isEmpty, socket === current, generation == currentGeneration else { return false }
            await sendPing(socket: current, generation: currentGeneration)
            guard pings.count == 1, pings[0] === current else { return false }
            close()
            var blocked: Task<URLSessionWebSocketTask, Error>?
            await withCheckedContinuation { ready in
                regressionTokenGate = (1, ready, [])
                blocked = Task { try await self.connect() }
            }
            close()
            releaseRegressionTokenGate()
            do {
                _ = try await blocked!.value
                return false
            } catch RelayError.notConnected {
                return socket == nil && !connected
            }
        } catch { return false }
    }
}
#endif
