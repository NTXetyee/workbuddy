import Foundation
import XCTest
import WBProto

final class ProtocolTests: XCTestCase {

    // MARK: - Helpers

    /// Parses "57 42 01 ..." (whitespace ignored). Returns nil on malformed hex.
    private func hexToData(_ hex: String) -> Data? {
        let clean = Array(hex.filter { !$0.isWhitespace })
        if clean.count % 2 != 0 {
            return nil
        }
        var out = Data()
        var i = 0
        while i < clean.count {
            let pair = String(clean[i]) + String(clean[i + 1])
            guard let byte = UInt8(pair, radix: 16) else {
                return nil
            }
            out.append(byte)
            i += 2
        }
        return out
    }

    private func bytes(_ hex: String) -> Data {
        guard let d = hexToData(hex) else {
            XCTFail("invalid hex literal in test: \(hex)")
            return Data()
        }
        return d
    }

    private func typeName(_ m: WBMessage) -> String {
        switch m {
        case .hello: return "Hello"
        case .config: return "Config"
        case .video: return "Video"
        case .keyframeRequest: return "KeyframeRequest"
        case .ping: return "Ping"
        case .pong: return "Pong"
        case .orientation: return "Orientation"
        case .stats: return "Stats"
        case .bye: return "Bye"
        case .unknown: return "Unknown"
        }
    }

    private func decodeAll(_ data: Data) throws -> [(seq: UInt32, message: WBMessage)] {
        var dec = WBFrameDecoder()
        dec.push(data)
        var out: [(seq: UInt32, message: WBMessage)] = []
        while let r = try dec.next() {
            out.append(r)
        }
        return out
    }

    private func expectProtoError(_ data: Data, _ expected: WBProtoError, file: StaticString = #filePath, line: UInt = #line) {
        var dec = WBFrameDecoder()
        dec.push(data)
        do {
            _ = try dec.next()
            XCTFail("expected \(expected) but no error was thrown", file: file, line: line)
        } catch let e as WBProtoError {
            XCTAssertEqual(e, expected, file: file, line: line)
        } catch {
            XCTFail("unexpected error type: \(error)", file: file, line: line)
        }
    }

    private func sampleHello() -> WBHello {
        return WBHello(protoVersion: 1, codecs: 0x01, orientation: 0, nativeWidth: 2266, nativeHeight: 1488, maxFps: 60, name: "iPad mini")
    }

    private func sampleVideo() -> WBVideo {
        return WBVideo(ptsUs: 0x1122334455667788, flags: 0x03, data: Data([0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE]))
    }

    // MARK: - Golden vectors (SPEC 2.3)

    func testGoldenPingEncodeAndDecode() throws {
        let expected = bytes("57 42 01 05 01 00 00 00 08 00 00 00 08 07 06 05 04 03 02 01")
        XCTAssertEqual(wbEncode(.ping(0x0102030405060708), seq: 1), expected)

        let frames = try decodeAll(expected)
        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(frames.first?.seq, 1)
        XCTAssertEqual(frames.first?.message, WBMessage.ping(0x0102030405060708))
    }

    func testGoldenKeyframeRequestEncodeAndDecode() throws {
        let expected = bytes("57 42 01 04 02 00 00 00 00 00 00 00")
        XCTAssertEqual(wbEncode(.keyframeRequest, seq: 2), expected)

        let frames = try decodeAll(expected)
        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(frames.first?.seq, 2)
        XCTAssertEqual(frames.first?.message, WBMessage.keyframeRequest)
    }

    func testGoldenConfigEncodeAndDecode() throws {
        let expected = bytes("57 42 01 02 03 00 00 00 0C 00 00 00 01 00 DA 08 D0 05 3C 00 20 4E 00 00")
        let config = WBConfig(codec: 1, width: 2266, height: 1488, fps: 60, bitrateKbps: 20000)
        XCTAssertEqual(wbEncode(.config(config), seq: 3), expected)

        let frames = try decodeAll(expected)
        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(frames.first?.seq, 3)
        XCTAssertEqual(frames.first?.message, WBMessage.config(config))
    }

    // MARK: - Roundtrip of every message type

    func testRoundtripAllMessageTypes() throws {
        let messages: [WBMessage] = [
            .hello(sampleHello()),
            .config(WBConfig(codec: 1, width: 1920, height: 1080, fps: 30, bitrateKbps: 8000)),
            .video(sampleVideo()),
            .video(WBVideo(ptsUs: 0, flags: 0, data: Data())),
            .keyframeRequest,
            .ping(42),
            .pong(UInt64.max),
            .orientation(1),
            .stats(WBStats(framesDecoded: 600, framesDropped: 3, lastDecodeUs: 2500)),
            .bye,
            .unknown(type: 0x10, payload: Data([0x01, 0x02, 0x03])),
        ]
        for (i, m) in messages.enumerated() {
            let seq = UInt32(i * 7)
            let encoded = wbEncode(m, seq: seq)
            let frames = try decodeAll(encoded)
            XCTAssertEqual(frames.count, 1, "message \(i)")
            XCTAssertEqual(frames.first?.seq, seq, "message \(i)")
            XCTAssertEqual(frames.first?.message, m, "message \(i)")
        }
    }

    func testVideoFlagsAndHeaderLayout() {
        let v = sampleVideo()
        XCTAssertTrue(v.isKeyframe)
        XCTAssertTrue(v.hasParams)
        let notKey = WBVideo(ptsUs: 1, flags: 0x02, data: Data())
        XCTAssertFalse(notKey.isKeyframe)
        XCTAssertTrue(notKey.hasParams)

        let encoded = wbEncode(.video(v), seq: 9)
        // header: magic, version, type 0x03, seq 9, len = 12 + data.count
        XCTAssertEqual(encoded[0], 0x57)
        XCTAssertEqual(encoded[1], 0x42)
        XCTAssertEqual(encoded[2], 0x01)
        XCTAssertEqual(encoded[3], 0x03)
        XCTAssertEqual(Array(encoded[4..<8]), [9, 0, 0, 0])
        XCTAssertEqual(Array(encoded[8..<12]), [UInt8(12 + v.data.count), 0, 0, 0])
        // payload: pts LE, flags, reserved zeros
        XCTAssertEqual(Array(encoded[12..<20]), [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11])
        XCTAssertEqual(encoded[20], 0x03)
        XCTAssertEqual(Array(encoded[21..<24]), [0, 0, 0])
    }

    func testHelloNameTruncatedTo255BytesAndRoundtrips() throws {
        let longName = String(repeating: "a", count: 300)
        let h = WBHello(protoVersion: 1, codecs: 1, orientation: 0, nativeWidth: 2266, nativeHeight: 1488, maxFps: 60, name: longName)
        let encoded = wbEncode(.hello(h), seq: 0)
        // payload name_len is at offset 12 + 10
        XCTAssertEqual(encoded[22], 255)
        XCTAssertEqual(encoded.count, 12 + 11 + 255)
        let frames = try decodeAll(encoded)
        guard case .hello(let back)? = frames.first?.message else {
            return XCTFail("expected hello")
        }
        XCTAssertEqual(back.name, String(repeating: "a", count: 255))
    }

    func testHelloFixedLayout() {
        let encoded = wbEncode(.hello(sampleHello()), seq: 0)
        // payload begins at 12: proto, codecs, orientation, reserved, w LE, h LE, fps LE, name_len, name
        let p = Array(encoded[12...])
        XCTAssertEqual(Array(p[0..<4]), [1, 1, 0, 0])
        XCTAssertEqual(Array(p[4..<6]), [0xDA, 0x08]) // 2266
        XCTAssertEqual(Array(p[6..<8]), [0xD0, 0x05]) // 1488
        XCTAssertEqual(Array(p[8..<10]), [60, 0])
        XCTAssertEqual(p[10], 9)
        XCTAssertEqual(String(bytes: p[11...], encoding: .utf8), "iPad mini")
    }

    // MARK: - Chunking

    func testByteAtATimeDecodeOfMultipleMessages() throws {
        var stream = Data()
        stream.append(wbEncode(.ping(1), seq: 0))
        stream.append(wbEncode(.video(sampleVideo()), seq: 1))
        stream.append(wbEncode(.keyframeRequest, seq: 2))
        stream.append(wbEncode(.stats(WBStats(framesDecoded: 1, framesDropped: 2, lastDecodeUs: 3)), seq: 3))

        var dec = WBFrameDecoder()
        var out: [(seq: UInt32, message: WBMessage)] = []
        for i in 0..<stream.count {
            dec.push(stream.subdata(in: i..<(i + 1)))
            while let r = try dec.next() {
                out.append(r)
            }
        }
        XCTAssertEqual(out.count, 4)
        XCTAssertEqual(out.map { $0.seq }, [0, 1, 2, 3])
        XCTAssertEqual(out[0].message, WBMessage.ping(1))
        XCTAssertEqual(out[1].message, WBMessage.video(sampleVideo()))
        XCTAssertEqual(out[2].message, WBMessage.keyframeRequest)
        XCTAssertEqual(out[3].message, WBMessage.stats(WBStats(framesDecoded: 1, framesDropped: 2, lastDecodeUs: 3)))
    }

    func testSplitAcrossHeaderAndPayloadBoundary() throws {
        let encoded = wbEncode(.video(sampleVideo()), seq: 5)
        var dec = WBFrameDecoder()
        dec.push(encoded.subdata(in: 0..<5))
        XCTAssertNil(try dec.next())
        dec.push(encoded.subdata(in: 5..<15))
        XCTAssertNil(try dec.next())
        dec.push(encoded.subdata(in: 15..<encoded.count))
        let r = try dec.next()
        XCTAssertEqual(r?.seq, 5)
        XCTAssertEqual(r?.message, WBMessage.video(sampleVideo()))
        XCTAssertNil(try dec.next())
    }

    func testUnknownTypeIsSkippedAndSurfaced() throws {
        let unknown = bytes("57 42 01 7F 00 00 00 00 02 00 00 00 AA BB")
        let ping = wbEncode(.ping(7), seq: 1)
        var dec = WBFrameDecoder()
        dec.push(unknown)
        dec.push(ping)
        let first = try dec.next()
        XCTAssertEqual(first?.seq, 0)
        XCTAssertEqual(first?.message, WBMessage.unknown(type: 0x7F, payload: Data([0xAA, 0xBB])))
        let second = try dec.next()
        XCTAssertEqual(second?.seq, 1)
        XCTAssertEqual(second?.message, WBMessage.ping(7))
    }

    // MARK: - Errors

    func testBadMagic() {
        expectProtoError(bytes("58 42 01 05 01 00 00 00 08 00 00 00 08 07 06 05 04 03 02 01"), .badMagic)
    }

    func testBadVersion() {
        expectProtoError(bytes("57 42 02 04 02 00 00 00 00 00 00 00"), .badVersion(2))
    }

    func testTooLarge() {
        // len = 4194305 (0x00400001), header only: must fail without waiting for payload
        expectProtoError(bytes("57 42 01 03 00 00 00 00 01 00 40 00"), .tooLarge(4_194_305))
    }

    func testMaxPayloadIsAllowedAsLength() throws {
        // len == 4 MiB is legal as a length; the frame just is not complete yet.
        var dec = WBFrameDecoder()
        dec.push(bytes("57 42 01 09 00 00 00 00 00 00 40 00"))
        XCTAssertNil(try dec.next())
    }

    func testBadPayloadConfigWrongLength() {
        var data = bytes("57 42 01 02 00 00 00 00 0B 00 00 00")
        data.append(Data(repeating: 0, count: 11))
        expectProtoError(data, .badPayload(type: 0x02))
    }

    func testBadPayloadKeyframeRequestWithBody() {
        expectProtoError(bytes("57 42 01 04 00 00 00 00 01 00 00 00 00"), .badPayload(type: 0x04))
    }

    func testBadPayloadHelloNameLengthMismatch() {
        // fixed part 11 bytes, name_len says 5 but only 2 name bytes follow
        expectProtoError(bytes("57 42 01 01 00 00 00 00 0D 00 00 00 01 01 00 00 DA 08 D0 05 3C 00 05 61 62"), .badPayload(type: 0x01))
    }

    func testBadPayloadShortPing() {
        expectProtoError(bytes("57 42 01 05 00 00 00 00 04 00 00 00 01 02 03 04"), .badPayload(type: 0x05))
    }

    func testBadPayloadAfterErrorKeepsStreamInSync() throws {
        // A bad payload consumes its frame, so the following frame still decodes.
        var stream = bytes("57 42 01 04 00 00 00 00 01 00 00 00 00")
        stream.append(wbEncode(.ping(9), seq: 1))
        var dec = WBFrameDecoder()
        dec.push(stream)
        XCTAssertThrowsError(try dec.next())
        let next = try dec.next()
        XCTAssertEqual(next?.seq, 1)
        XCTAssertEqual(next?.message, WBMessage.ping(9))
    }

    // MARK: - Shared vectors (repo proto/vectors.json, generated by the Rust crate)

    func testSharedVectors() throws {
        // #filePath = <repo>/ios/WBProto/Tests/WBProtoTests/ProtocolTests.swift
        // Dropping 5 components yields <repo>.
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 {
            url.deleteLastPathComponent()
        }
        url.appendPathComponent("proto")
        url.appendPathComponent("vectors.json")

        guard FileManager.default.fileExists(atPath: url.path) else {
            throw XCTSkip("proto/vectors.json not found at \(url.path)")
        }

        let raw = try Data(contentsOf: url)
        let json = try JSONSerialization.jsonObject(with: raw)
        let entries: [[String: Any]]
        if let arr = json as? [[String: Any]] {
            entries = arr
        } else if let dict = json as? [String: Any], let arr = dict["vectors"] as? [[String: Any]] {
            entries = arr
        } else {
            XCTFail("vectors.json has an unexpected shape")
            return
        }
        XCTAssertFalse(entries.isEmpty, "vectors.json has no entries")

        for entry in entries {
            let name = entry["name"] as? String ?? "<unnamed>"
            guard let hex = entry["hex"] as? String, let expected = hexToData(hex) else {
                XCTFail("[\(name)] missing or invalid hex")
                continue
            }
            guard let seqNumber = entry["seq"] as? Int else {
                XCTFail("[\(name)] missing seq")
                continue
            }
            guard let msgObj = entry["message"] as? [String: Any], let expectedType = msgObj["type"] as? String else {
                XCTFail("[\(name)] missing message.type")
                continue
            }

            var dec = WBFrameDecoder()
            dec.push(expected)
            let decoded: (seq: UInt32, message: WBMessage)?
            do {
                decoded = try dec.next()
            } catch {
                XCTFail("[\(name)] decode failed: \(error)")
                continue
            }
            guard let frame = decoded else {
                XCTFail("[\(name)] decoder produced no frame")
                continue
            }
            XCTAssertEqual(frame.seq, UInt32(seqNumber), "[\(name)] seq")
            XCTAssertEqual(typeName(frame.message), expectedType, "[\(name)] message type")
            XCTAssertEqual(wbEncode(frame.message, seq: frame.seq), expected, "[\(name)] re-encode")

            let rest = try dec.next()
            XCTAssertNil(rest, "[\(name)] trailing data after frame")
        }
    }
}
