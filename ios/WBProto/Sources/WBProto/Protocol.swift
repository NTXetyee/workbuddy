import Foundation

// WorkBuddy wire protocol v1 (SPEC section 2). All integers are little-endian.
// Frame = 12-byte header ('W','B',version,type,seq u32,len u32) + payload(len).

// MARK: - Public messages

public struct WBHello: Equatable, Sendable {
    public var protoVersion: UInt8
    public var codecs: UInt8
    public var orientation: UInt8
    public var nativeWidth: UInt16
    public var nativeHeight: UInt16
    public var maxFps: UInt16
    public var name: String

    public init(protoVersion: UInt8, codecs: UInt8, orientation: UInt8, nativeWidth: UInt16, nativeHeight: UInt16, maxFps: UInt16, name: String) {
        self.protoVersion = protoVersion
        self.codecs = codecs
        self.orientation = orientation
        self.nativeWidth = nativeWidth
        self.nativeHeight = nativeHeight
        self.maxFps = maxFps
        self.name = name
    }
}

public struct WBConfig: Equatable, Sendable {
    public var codec: UInt8
    public var width: UInt16
    public var height: UInt16
    public var fps: UInt16
    public var bitrateKbps: UInt32

    public init(codec: UInt8, width: UInt16, height: UInt16, fps: UInt16, bitrateKbps: UInt32) {
        self.codec = codec
        self.width = width
        self.height = height
        self.fps = fps
        self.bitrateKbps = bitrateKbps
    }
}

public struct WBVideo: Equatable, Sendable {
    public var ptsUs: UInt64
    public var flags: UInt8
    public var data: Data

    /// flags bit0 = keyframe (IDR).
    public var isKeyframe: Bool { return (flags & 0x01) != 0 }
    /// flags bit1 = contains SPS+PPS.
    public var hasParams: Bool { return (flags & 0x02) != 0 }

    public init(ptsUs: UInt64, flags: UInt8, data: Data) {
        self.ptsUs = ptsUs
        self.flags = flags
        self.data = data
    }
}

public struct WBStats: Equatable, Sendable {
    public var framesDecoded: UInt32
    public var framesDropped: UInt32
    public var lastDecodeUs: UInt32

    public init(framesDecoded: UInt32, framesDropped: UInt32, lastDecodeUs: UInt32) {
        self.framesDecoded = framesDecoded
        self.framesDropped = framesDropped
        self.lastDecodeUs = lastDecodeUs
    }
}

public enum WBMessage: Equatable, Sendable {
    case hello(WBHello)
    case config(WBConfig)
    case video(WBVideo)
    case keyframeRequest
    case ping(UInt64)
    case pong(UInt64)
    case orientation(UInt8)
    case stats(WBStats)
    case bye
    case unknown(type: UInt8, payload: Data)
}

public enum WBProtoError: Error, Equatable, Sendable {
    case badMagic
    case badVersion(UInt8)
    case tooLarge(UInt32)
    case badPayload(type: UInt8)
}

// MARK: - Constants

private let wbMagic0: UInt8 = 0x57 // 'W'
private let wbMagic1: UInt8 = 0x42 // 'B'
private let wbVersion: UInt8 = 0x01
private let wbHeaderLen: Int = 12
private let wbMaxPayload: UInt32 = 4 * 1024 * 1024
private let wbMaxNameBytes: Int = 255

private enum WBTypeCode {
    static let hello: UInt8 = 0x01
    static let config: UInt8 = 0x02
    static let video: UInt8 = 0x03
    static let keyframeRequest: UInt8 = 0x04
    static let ping: UInt8 = 0x05
    static let pong: UInt8 = 0x06
    static let orientation: UInt8 = 0x07
    static let stats: UInt8 = 0x08
    static let bye: UInt8 = 0x09
}

// MARK: - Byte helpers (explicit byte-wise, no unaligned loads)

private func appendLE(_ d: inout Data, _ value: UInt64, bytes: Int) {
    for i in 0..<bytes {
        d.append(UInt8(truncatingIfNeeded: value >> UInt64(8 * i)))
    }
}

private func readLE(_ b: [UInt8], _ off: Int, _ n: Int) -> UInt64 {
    var v: UInt64 = 0
    for i in 0..<n {
        v |= UInt64(b[off + i]) << UInt64(8 * i)
    }
    return v
}

private func readU16(_ b: [UInt8], _ off: Int) -> UInt16 {
    return UInt16(truncatingIfNeeded: readLE(b, off, 2))
}

private func readU32(_ b: [UInt8], _ off: Int) -> UInt32 {
    return UInt32(truncatingIfNeeded: readLE(b, off, 4))
}

private func readU64(_ b: [UInt8], _ off: Int) -> UInt64 {
    return readLE(b, off, 8)
}

/// UTF-8 encoding of `s`, cut at a character boundary so it never exceeds `maxBytes`.
private func utf8Prefix(_ s: String, maxBytes: Int) -> Data {
    var out = Data()
    for ch in s {
        let bytes = Array(String(ch).utf8)
        if out.count + bytes.count > maxBytes {
            break
        }
        out.append(contentsOf: bytes)
    }
    return out
}

// MARK: - Encoding

private func wbPayload(_ m: WBMessage) -> (UInt8, Data) {
    switch m {
    case .hello(let h):
        var p = Data()
        p.append(h.protoVersion)
        p.append(h.codecs)
        p.append(h.orientation)
        p.append(UInt8(0)) // reserved
        appendLE(&p, UInt64(h.nativeWidth), bytes: 2)
        appendLE(&p, UInt64(h.nativeHeight), bytes: 2)
        appendLE(&p, UInt64(h.maxFps), bytes: 2)
        let name = utf8Prefix(h.name, maxBytes: wbMaxNameBytes)
        p.append(UInt8(name.count))
        p.append(name)
        return (WBTypeCode.hello, p)

    case .config(let c):
        var p = Data()
        p.append(c.codec)
        p.append(UInt8(0)) // reserved
        appendLE(&p, UInt64(c.width), bytes: 2)
        appendLE(&p, UInt64(c.height), bytes: 2)
        appendLE(&p, UInt64(c.fps), bytes: 2)
        appendLE(&p, UInt64(c.bitrateKbps), bytes: 4)
        return (WBTypeCode.config, p)

    case .video(let v):
        var p = Data(capacity: 12 + v.data.count)
        appendLE(&p, v.ptsUs, bytes: 8)
        p.append(v.flags)
        p.append(UInt8(0)) // reserved
        p.append(UInt8(0))
        p.append(UInt8(0))
        p.append(v.data)
        return (WBTypeCode.video, p)

    case .keyframeRequest:
        return (WBTypeCode.keyframeRequest, Data())

    case .ping(let ts):
        var p = Data()
        appendLE(&p, ts, bytes: 8)
        return (WBTypeCode.ping, p)

    case .pong(let ts):
        var p = Data()
        appendLE(&p, ts, bytes: 8)
        return (WBTypeCode.pong, p)

    case .orientation(let o):
        return (WBTypeCode.orientation, Data([o]))

    case .stats(let s):
        var p = Data()
        appendLE(&p, UInt64(s.framesDecoded), bytes: 4)
        appendLE(&p, UInt64(s.framesDropped), bytes: 4)
        appendLE(&p, UInt64(s.lastDecodeUs), bytes: 4)
        return (WBTypeCode.stats, p)

    case .bye:
        return (WBTypeCode.bye, Data())

    case .unknown(let type, let payload):
        return (type, payload)
    }
}

/// Encodes one complete frame (header + payload).
public func wbEncode(_ m: WBMessage, seq: UInt32) -> Data {
    let (type, payload) = wbPayload(m)
    var out = Data()
    out.reserveCapacity(wbHeaderLen + payload.count)
    out.append(wbMagic0)
    out.append(wbMagic1)
    out.append(wbVersion)
    out.append(type)
    appendLE(&out, UInt64(seq), bytes: 4)
    appendLE(&out, UInt64(payload.count), bytes: 4)
    out.append(payload)
    return out
}

// MARK: - Payload decoding

private func wbDecodePayload(type: UInt8, _ p: [UInt8]) throws -> WBMessage {
    let bad = WBProtoError.badPayload(type: type)
    switch type {
    case WBTypeCode.hello:
        guard p.count >= 11 else { throw bad }
        let nameLen = Int(p[10])
        guard p.count == 11 + nameLen else { throw bad }
        guard let name = String(bytes: p[11..<(11 + nameLen)], encoding: .utf8) else { throw bad }
        let h = WBHello(
            protoVersion: p[0],
            codecs: p[1],
            orientation: p[2],
            nativeWidth: readU16(p, 4),
            nativeHeight: readU16(p, 6),
            maxFps: readU16(p, 8),
            name: name
        )
        return .hello(h)

    case WBTypeCode.config:
        guard p.count == 12 else { throw bad }
        let c = WBConfig(
            codec: p[0],
            width: readU16(p, 2),
            height: readU16(p, 4),
            fps: readU16(p, 6),
            bitrateKbps: readU32(p, 8)
        )
        return .config(c)

    case WBTypeCode.video:
        guard p.count >= 12 else { throw bad }
        let v = WBVideo(ptsUs: readU64(p, 0), flags: p[8], data: Data(p[12...]))
        return .video(v)

    case WBTypeCode.keyframeRequest:
        guard p.isEmpty else { throw bad }
        return .keyframeRequest

    case WBTypeCode.ping:
        guard p.count == 8 else { throw bad }
        return .ping(readU64(p, 0))

    case WBTypeCode.pong:
        guard p.count == 8 else { throw bad }
        return .pong(readU64(p, 0))

    case WBTypeCode.orientation:
        guard p.count == 1 else { throw bad }
        return .orientation(p[0])

    case WBTypeCode.stats:
        guard p.count == 12 else { throw bad }
        let s = WBStats(
            framesDecoded: readU32(p, 0),
            framesDropped: readU32(p, 4),
            lastDecodeUs: readU32(p, 8)
        )
        return .stats(s)

    case WBTypeCode.bye:
        guard p.isEmpty else { throw bad }
        return .bye

    default:
        // Unknown types are skipped by the framing layer; surfaced as .unknown (SPEC 2.1).
        return .unknown(type: type, payload: Data(p))
    }
}

// MARK: - Framing decoder

/// Incremental frame decoder. Feed bytes with `push`, then call `next` until it returns nil.
/// Bad magic / bad version / too large are protocol errors: the caller must close the connection.
/// `badPayload` is raised after the whole frame has been consumed, so decoding stays in sync.
public struct WBFrameDecoder {
    private var buffer: Data

    public init() {
        self.buffer = Data()
    }

    public mutating func push(_ d: Data) {
        buffer.append(d)
    }

    public mutating func next() throws -> (seq: UInt32, message: WBMessage)? {
        if buffer.count < wbHeaderLen {
            return nil
        }
        let base = buffer.startIndex
        let hdr = Array(buffer.prefix(wbHeaderLen))
        guard hdr[0] == wbMagic0, hdr[1] == wbMagic1 else {
            throw WBProtoError.badMagic
        }
        guard hdr[2] == wbVersion else {
            throw WBProtoError.badVersion(hdr[2])
        }
        let type = hdr[3]
        let seq = readU32(hdr, 4)
        let len = readU32(hdr, 8)
        guard len <= wbMaxPayload else {
            throw WBProtoError.tooLarge(len)
        }
        let total = wbHeaderLen + Int(len)
        if buffer.count < total {
            return nil
        }
        let payload = Array(buffer[(base + wbHeaderLen)..<(base + total)])
        // Re-create the buffer so indices start at 0 again.
        buffer = Data(buffer.dropFirst(total))
        let msg = try wbDecodePayload(type: type, payload)
        return (seq: seq, message: msg)
    }
}
