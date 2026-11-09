import Foundation
import Network
import UIKit
import Combine
import WBProto

/// iPad side of the USB session (SPEC 4.4).
///
/// Listens on TCP port 7878. The Windows host reaches this port through the
/// usbmuxd tunnel. One connection at a time: a new connection replaces the
/// old one. Foreground only, no background modes.
///
/// Threading: every Network.framework callback, every socket operation, the
/// framer, the sequence counter and the stats timer run on the serial `net`
/// queue. `@Published` properties are only written on the main queue.
final class Session: ObservableObject {
    @Published private(set) var status: String = "Idle"
    @Published private(set) var streaming: Bool = false

    private static let waitingText = "Waiting for PC on USB\u{2026}"
    private static let keyframeMinIntervalNs: UInt64 = 100_000_000

    private struct DeviceInfo {
        var width: UInt16 = 2266
        var height: UInt16 = 1488
        var name: String = "iPad"
    }

    private let decoder: VideoDecoder
    private let renderer: DisplayRenderer
    private let net = DispatchQueue(label: "workbuddy.net", qos: .userInteractive)

    // The members below are only touched on `net`.
    private var running = false
    private var port: UInt16 = 7878
    private var deviceInfo = DeviceInfo()
    private var listener: NWListener?
    private var connection: NWConnection?
    private var connGen: UInt64 = 0
    private var connActive = false
    private var framer = WBFrameDecoder()
    private var sendSeq: UInt32 = 0
    private var lastKeyReqNs: UInt64?
    private var statsTimer: DispatchSourceTimer?

    init(decoder: VideoDecoder, renderer: DisplayRenderer) {
        self.decoder = decoder
        self.renderer = renderer
        decoder.onFrame = { [weak renderer] sb in
            renderer?.enqueue(sb)
        }
        decoder.onNeedKeyframe = { [weak self] in
            self?.requestKeyframe()
        }
    }

    // MARK: - Public API

    /// Call from the main thread. Reads UIKit values once here, then starts the
    /// listener on the `net` queue.
    func start(port: UInt16 = 7878) {
        let info = Session.readDeviceInfo()
        publish(status: "Starting\u{2026}")
        net.async { [weak self] in
            guard let self = self, !self.running else { return }
            self.running = true
            self.port = port
            self.deviceInfo = info
            self.openListener()
        }
    }

    func stop() {
        net.async { [weak self] in
            guard let self = self else { return }
            self.running = false
            self.listener?.stateUpdateHandler = nil
            self.listener?.cancel()
            self.listener = nil
            self.closeConnection(status: "Stopped")
        }
    }

    /// Asks the host for an IDR. Rate-limited to one request per 100 ms and a
    /// no-op without an active connection. Safe to call from any thread.
    func requestKeyframe() {
        net.async { [weak self] in
            guard let self = self, self.connActive else { return }
            let now = DispatchTime.now().uptimeNanoseconds
            if let last = self.lastKeyReqNs, now &- last < Session.keyframeMinIntervalNs {
                return
            }
            self.lastKeyReqNs = now
            self.send(.keyframeRequest)
        }
    }

    // MARK: - Main-thread helpers

    private static func readDeviceInfo() -> DeviceInfo {
        func read() -> DeviceInfo {
            var info = DeviceInfo()
            let bounds = UIScreen.main.nativeBounds
            info.width = UInt16(clamping: Int(max(bounds.width, bounds.height)))
            info.height = UInt16(clamping: Int(min(bounds.width, bounds.height)))
            info.name = String(UIDevice.current.name.prefix(60))
            return info
        }
        if Thread.isMainThread {
            return read()
        }
        return DispatchQueue.main.sync { read() }
    }

    private func publish(status: String? = nil, streaming: Bool? = nil) {
        DispatchQueue.main.async { [weak self] in
            guard let self = self else { return }
            if let status = status {
                self.status = status
            }
            if let streaming = streaming {
                self.streaming = streaming
            }
        }
    }

    // MARK: - Listener (net queue)

    private func openListener() {
        guard running, listener == nil else { return }

        let parameters = NWParameters.tcp
        parameters.allowLocalEndpointReuse = true
        if let tcp = parameters.defaultProtocolStack.transportProtocol as? NWProtocolTCP.Options {
            tcp.noDelay = true
        }

        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            publish(status: "Invalid port \(port)")
            return
        }

        do {
            let newListener = try NWListener(using: parameters, on: nwPort)
            newListener.stateUpdateHandler = { [weak self] state in
                self?.listenerStateChanged(state)
            }
            newListener.newConnectionHandler = { [weak self] connection in
                self?.adopt(connection)
            }
            listener = newListener
            newListener.start(queue: net)
        } catch {
            publish(status: "Listener error: \(error.localizedDescription)")
            scheduleListenerRestart()
        }
    }

    private func listenerStateChanged(_ state: NWListener.State) {
        switch state {
        case .ready:
            if connection == nil {
                publish(status: Session.waitingText)
            }
        case .failed(let error):
            if connection == nil {
                publish(status: "Listener failed: \(error.localizedDescription)")
            }
            listener?.stateUpdateHandler = nil
            listener?.cancel()
            listener = nil
            scheduleListenerRestart()
        default:
            break
        }
    }

    private func scheduleListenerRestart() {
        net.asyncAfter(deadline: .now() + 1) { [weak self] in
            guard let self = self, self.running, self.listener == nil else { return }
            self.openListener()
        }
    }

    // MARK: - Connection lifecycle (net queue)

    /// Adopts a new connection, replacing any existing one.
    private func adopt(_ newConnection: NWConnection) {
        guard running else {
            newConnection.cancel()
            return
        }
        if connection != nil {
            closeConnection(status: Session.waitingText)
        }
        connGen &+= 1
        let gen = connGen
        connection = newConnection
        connActive = false
        framer = WBFrameDecoder()
        sendSeq = 0
        lastKeyReqNs = nil
        newConnection.stateUpdateHandler = { [weak self] state in
            self?.connectionStateChanged(gen: gen, state: state)
        }
        newConnection.start(queue: net)
    }

    private func connectionStateChanged(gen: UInt64, state: NWConnection.State) {
        // Callbacks from a replaced or closed connection carry an old generation.
        guard gen == connGen else { return }
        switch state {
        case .ready:
            connActive = true
            publish(status: "Connected")
            send(.hello(makeHello()))
            armReceive(gen: gen)
            startStatsTimer()
        case .failed, .cancelled:
            closeConnection(status: Session.waitingText)
        default:
            break
        }
    }

    /// Tears down the active connection. Bumping `connGen` makes every
    /// pending callback from it a no-op.
    private func closeConnection(status: String) {
        stopStatsTimer()
        connGen &+= 1
        connection?.cancel()
        connection = nil
        connActive = false
        decoder.reset()
        renderer.flush()
        publish(status: status, streaming: false)
    }

    // MARK: - Receive (net queue)

    private func armReceive(gen: UInt64) {
        guard gen == connGen, let conn = connection else { return }
        conn.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { [weak self] data, _, isComplete, error in
            self?.onReceive(gen: gen, data: data, isComplete: isComplete, error: error)
        }
    }

    private func onReceive(gen: UInt64, data: Data?, isComplete: Bool, error: NWError?) {
        guard gen == connGen else { return }

        var alive = true
        do {
            if let data = data, !data.isEmpty {
                framer.push(data)
            }
            while alive, let item = try framer.next() {
                alive = handle(item.message)
            }
        } catch {
            // Protocol error (bad magic, version or size): close.
            alive = false
        }

        if !alive || isComplete || error != nil {
            closeConnection(status: Session.waitingText)
        } else {
            armReceive(gen: gen)
        }
    }

    /// Returns false when the connection should be closed.
    private func handle(_ message: WBMessage) -> Bool {
        switch message {
        case .config(let cfg):
            decoder.reset()
            renderer.flush()
            publish(status: "Streaming \(cfg.width)x\(cfg.height)@\(cfg.fps)", streaming: true)
        case .video(let video):
            decoder.decode(annexB: video.data, ptsUs: video.ptsUs, keyframe: video.isKeyframe)
        case .ping(let timestamp):
            send(.pong(timestamp))
        case .bye:
            return false
        default:
            break
        }
        return true
    }

    // MARK: - Send (net queue)

    private func makeHello() -> WBHello {
        return WBHello(protoVersion: 1,
                       codecs: 0x01,
                       orientation: 0,
                       nativeWidth: deviceInfo.width,
                       nativeHeight: deviceInfo.height,
                       maxFps: 60,
                       name: deviceInfo.name)
    }

    private func nextSeq() -> UInt32 {
        let seq = sendSeq
        sendSeq &+= 1
        return seq
    }

    /// Fire-and-forget send. Must be called on the `net` queue.
    private func send(_ message: WBMessage) {
        guard let conn = connection else { return }
        let data = wbEncode(message, seq: nextSeq())
        conn.send(content: data, completion: .contentProcessed { _ in })
    }

    // MARK: - Stats timer (net queue)

    private func startStatsTimer() {
        stopStatsTimer()
        let timer = DispatchSource.makeTimerSource(queue: net)
        timer.schedule(deadline: .now() + .seconds(1), repeating: .seconds(1))
        timer.setEventHandler { [weak self] in
            guard let self = self, self.connActive else { return }
            let stats = WBStats(framesDecoded: self.decoder.framesDecoded,
                                framesDropped: self.decoder.framesDropped,
                                lastDecodeUs: self.decoder.lastDecodeUs)
            self.send(.stats(stats))
        }
        statsTimer = timer
        timer.resume()
    }

    private func stopStatsTimer() {
        statsTimer?.cancel()
        statsTimer = nil
    }
}
