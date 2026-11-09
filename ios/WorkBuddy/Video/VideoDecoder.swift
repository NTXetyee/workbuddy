import Foundation
import Dispatch
import CoreMedia
import CoreVideo
import VideoToolbox

/// Hardware H.264 decoder for the WorkBuddy stream (SPEC 4.2).
///
/// Threading model:
/// - All decoder state that is only touched by decoding (parameter sets, format
///   description, VT session, keyframe bookkeeping) lives on `queue`.
/// - State shared with the VT output callback thread and with readers of the
///   public counters is guarded by `lock`. `lock` is never held across a VT call.
/// - `reset()` runs synchronously on `queue`; do not call it from inside `onFrame`.
final class VideoDecoder {

    // MARK: - Public API

    /// Called with each decoded frame (DisplayImmediately set). Set by Session.
    var onFrame: ((CMSampleBuffer) -> Void)? {
        get { locked { _onFrame } }
        set { locked { _onFrame = newValue } }
    }

    /// Called when the decoder needs an IDR from the host. Set by Session.
    var onNeedKeyframe: (() -> Void)? {
        get { locked { _onNeedKeyframe } }
        set { locked { _onNeedKeyframe = newValue } }
    }

    var framesDecoded: UInt32 { locked { _framesDecoded } }
    var framesDropped: UInt32 { locked { _framesDropped } }
    var lastDecodeUs: UInt32 { locked { _lastDecodeUs } }

    init() {}

    deinit {
        // No other references exist here, so touching queue-confined state directly is safe.
        // Invalidation blocks until pending VT callbacks have returned.
        if let s = session {
            VTDecompressionSessionInvalidate(s)
        }
    }

    /// Drops the session and parameter sets and waits for the next keyframe.
    func reset() {
        queue.sync {
            self.resetOnQueue()
        }
    }

    /// Submits one access unit (Annex-B). Returns immediately; work runs on the decoder queue.
    func decode(annexB: Data, ptsUs: UInt64, keyframe: Bool) {
        queue.async { [weak self] in
            self?.process(annexB: annexB, ptsUs: ptsUs, keyframe: keyframe)
        }
    }

    // MARK: - Private state

    private struct PendingFrame {
        let submitNanos: UInt64
        let keyframe: Bool
    }

    private let queue = DispatchQueue(label: "wb.video.decoder", qos: .userInteractive)
    private let lock = NSLock()

    // Guarded by `lock`.
    private var _onFrame: ((CMSampleBuffer) -> Void)?
    private var _onNeedKeyframe: (() -> Void)?
    private var _framesDecoded: UInt32 = 0
    private var _framesDropped: UInt32 = 0
    private var _lastDecodeUs: UInt32 = 0
    private var inFlight: Int = 0
    private var pending: [UInt64: PendingFrame] = [:]

    // Queue-confined.
    private var sps: Data?
    private var pps: Data?
    private var formatDesc: CMVideoFormatDescription?
    private var session: VTDecompressionSession?
    private var awaitingKeyframe = true   // silently ignore non-key frames
    private var dropping = false          // overload drop mode, counted, until next keyframe
    private var lastKeyframeRequestNanos: UInt64? = nil   // rate limit for IDR requests (queue-confined)

    // Overload / recovery tuning.
    private let overloadInFlightLimit = 4                    // hardware decoders legitimately hold 3-4
    private let overloadWaitNanos: UInt64 = 60_000_000       // oldest in-flight frame must wait this long
    private let keyframeRequestIntervalNanos: UInt64 = 200_000_000

    // MARK: - Lock helper

    private func locked<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    // MARK: - Decode path (queue)

    private func process(annexB: Data, ptsUs: UInt64, keyframe: Bool) {
        var newSPS: Data?
        var newPPS: Data?
        var slices: [Data] = []
        for nal in VideoDecoder.splitNALs(annexB) {
            guard let header = nal.first else { continue }
            switch header & 0x1F {
            case 7:
                newSPS = nal
            case 8:
                newPPS = nal
            case 1, 5:
                slices.append(nal)
            default:
                // AUD (9), SEI (6), filler (12) and anything else stay out of the sample.
                break
            }
        }

        if let s = newSPS, let p = newPPS, (s != sps || p != pps || self.session == nil) {
            sps = s
            pps = p
            if !rebuildSession(sps: s, pps: p) {
                invalidateSession()
                formatDesc = nil
            }
        }

        guard !slices.isEmpty else { return }

        guard let activeSession = self.session, let desc = self.formatDesc else {
            // No usable session. A keyframe here lacked parameter sets, so ask for one.
            if keyframe {
                emitNeedKeyframe()
            }
            return
        }

        if keyframe {
            awaitingKeyframe = false
            dropping = false
        } else {
            if awaitingKeyframe {
                // Keep asking for an IDR (rate-limited) until one arrives.
                requestKeyframeRateLimited()
                return
            }
            if dropping {
                countDrop()
                requestKeyframeRateLimited()
                return
            }
            if isOverloaded(nowNanos: DispatchTime.now().uptimeNanoseconds) {
                dropping = true
                countDrop()
                requestKeyframeRateLimited()
                return
            }
        }

        guard let sample = makeSample(slices: slices, desc: desc, ptsUs: ptsUs) else {
            emitNeedKeyframe()
            return
        }

        let submitNanos = DispatchTime.now().uptimeNanoseconds
        lock.lock()
        inFlight += 1
        pending[ptsUs] = PendingFrame(submitNanos: submitNanos, keyframe: keyframe)
        lock.unlock()

        // No _1xRealTimePlayback: it lets VT drop frames silently. Drops are handled in handleOutput.
        let flags: VTDecodeFrameFlags = [._EnableAsynchronousDecompression]
        let status = VTDecompressionSessionDecodeFrame(
            activeSession,
            sampleBuffer: sample,
            flags: flags,
            frameRefcon: nil,
            infoFlagsOut: nil)

        if status != noErr {
            lock.lock()
            if inFlight > 0 { inFlight -= 1 }
            pending.removeValue(forKey: ptsUs)
            lock.unlock()

            if status == kVTInvalidSessionErr {
                invalidateSession()
                formatDesc = nil
                sps = nil
                pps = nil
            }
            awaitingKeyframe = true
            dropping = false
            emitNeedKeyframe()
        }
    }

    /// Builds a CMVideoFormatDescription and a fresh decompression session from parameter sets.
    /// Returns false if either step fails. On success `formatDesc` and `session` are set.
    private func rebuildSession(sps: Data, pps: Data) -> Bool {
        invalidateSession()
        formatDesc = nil

        let spsBytes = [UInt8](sps)
        let ppsBytes = [UInt8](pps)
        var desc: CMVideoFormatDescription?

        let descStatus: OSStatus = spsBytes.withUnsafeBufferPointer { spsBuf -> OSStatus in
            return ppsBytes.withUnsafeBufferPointer { ppsBuf -> OSStatus in
                guard let spsBase = spsBuf.baseAddress, let ppsBase = ppsBuf.baseAddress else {
                    return -1
                }
                let pointers: [UnsafePointer<UInt8>] = [spsBase, ppsBase]
                let sizes: [Int] = [spsBytes.count, ppsBytes.count]
                return pointers.withUnsafeBufferPointer { pBuf -> OSStatus in
                    return sizes.withUnsafeBufferPointer { sBuf -> OSStatus in
                        guard let pBase = pBuf.baseAddress, let sBase = sBuf.baseAddress else {
                            return -1
                        }
                        return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                            allocator: kCFAllocatorDefault,
                            parameterSetCount: 2,
                            parameterSetPointers: pBase,
                            parameterSetSizes: sBase,
                            nalUnitHeaderLength: 4,
                            formatDescriptionOut: &desc)
                    }
                }
            }
        }

        guard descStatus == noErr, let newDesc = desc else {
            return false
        }

        var record = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: VideoDecoder.outputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())

        // Literal keys: same strings as kVTVideoDecoderSpecification_{Require,Enable}HardwareAcceleratedVideoDecoder.
        let decoderSpec: [String: Any] = [
            "RequireHardwareAcceleratedVideoDecoder": true,
            "EnableHardwareAcceleratedVideoDecoder": true
        ]

        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any](),
            kCVPixelBufferMetalCompatibilityKey as String: true
        ]

        var newSession: VTDecompressionSession?
        let sessionStatus = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: newDesc,
            decoderSpecification: decoderSpec as CFDictionary,
            imageBufferAttributes: attributes as CFDictionary,
            outputCallback: &record,
            decompressionSessionOut: &newSession)

        guard sessionStatus == noErr, let created = newSession else {
            return false
        }

        _ = VTSessionSetProperty(created, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)

        formatDesc = newDesc
        session = created
        return true
    }

    private func invalidateSession() {
        if let s = session {
            VTDecompressionSessionInvalidate(s)
            session = nil
        }
    }

    private func resetOnQueue() {
        invalidateSession()
        sps = nil
        pps = nil
        formatDesc = nil
        awaitingKeyframe = true
        dropping = false
        lastKeyframeRequestNanos = nil

        lock.lock()
        inFlight = 0
        pending.removeAll()
        lock.unlock()
    }

    /// Wraps the slices of one access unit as AVCC (4-byte big-endian lengths) in a
    /// CMBlockBuffer that owns a copy of the bytes, then as a ready CMSampleBuffer.
    private func makeSample(slices: [Data], desc: CMVideoFormatDescription, ptsUs: UInt64) -> CMSampleBuffer? {
        var avcc = Data()
        for nal in slices {
            let length = UInt32(nal.count).bigEndian
            avcc.append(contentsOf: withUnsafeBytes(of: length) { Array($0) })
            avcc.append(nal)
        }
        let total = avcc.count
        guard total > 0 else { return nil }

        var blockOut: CMBlockBuffer?
        let blockStatus = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: total,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: total,
            flags: 0,
            blockBufferOut: &blockOut)
        guard blockStatus == noErr, let block = blockOut else { return nil }

        let copyStatus: OSStatus = avcc.withUnsafeBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(
                with: base,
                blockBuffer: block,
                offsetIntoDestination: 0,
                dataLength: total)
        }
        guard copyStatus == noErr else { return nil }

        var timing = CMSampleTimingInfo(
            duration: CMTime.invalid,
            presentationTimeStamp: CMTime(value: Int64(bitPattern: ptsUs), timescale: 1_000_000),
            decodeTimeStamp: CMTime.invalid)
        var sizes: [Int] = [total]
        var sample: CMSampleBuffer?
        let sampleStatus = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: block,
            formatDescription: desc,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sizes,
            sampleBufferOut: &sample)
        guard sampleStatus == noErr else { return nil }
        return sample
    }

    /// Splits an Annex-B byte stream into NAL unit payloads (start codes removed).
    /// Trailing zero bytes before the next start code are stripped.
    private static func splitNALs(_ data: Data) -> [Data] {
        let bytes = [UInt8](data)
        let n = bytes.count
        var codeStarts: [Int] = []
        var payloadStarts: [Int] = []

        var i = 0
        while i + 2 < n {
            if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
                codeStarts.append(i)
                payloadStarts.append(i + 3)
                i += 3
            } else {
                i += 1
            }
        }

        var result: [Data] = []
        for k in 0..<payloadStarts.count {
            let p = payloadStarts[k]
            var e = (k + 1 < codeStarts.count) ? codeStarts[k + 1] : n
            while e > p && bytes[e - 1] == 0 {
                e -= 1
            }
            if e > p {
                result.append(Data(bytes[p..<e]))
            }
        }
        return result
    }

    // MARK: - Keyframe signalling

    private func countDrop() {
        lock.lock()
        _framesDropped &+= 1
        lock.unlock()
    }

    /// True when more than `overloadInFlightLimit` frames are in flight AND the oldest one has
    /// been waiting longer than `overloadWaitNanos`. Queue depth alone is not overload.
    private func isOverloaded(nowNanos: UInt64) -> Bool {
        return locked { () -> Bool in
            guard inFlight > overloadInFlightLimit else { return false }
            guard let oldest = pending.values.map({ $0.submitNanos }).min() else { return false }
            return nowNanos > oldest && nowNanos - oldest > overloadWaitNanos
        }
    }

    /// Requests an IDR, at most once per `keyframeRequestIntervalNanos`. Queue-confined.
    /// Re-evaluated on every incoming non-key frame while recovering, so a lost request is retried.
    private func requestKeyframeRateLimited() {
        let now = DispatchTime.now().uptimeNanoseconds
        if let last = lastKeyframeRequestNanos, now < last + keyframeRequestIntervalNanos {
            return
        }
        lastKeyframeRequestNanos = now
        emitNeedKeyframe()
    }

    /// Error path: always reports (Session rate-limits the actual KEYFRAME_REQUEST).
    private func emitNeedKeyframe() {
        lock.lock()
        let cb = _onNeedKeyframe
        lock.unlock()
        cb?()
    }

    private func handleDecodeFailure() {
        awaitingKeyframe = true
        dropping = false
        emitNeedKeyframe()
    }

    // MARK: - VT output callback

    /// Plain C callback: captures nothing; the decoder comes back through the refCon.
    private static let outputCallback: VTDecompressionOutputCallback = { refCon, _, status, infoFlags, imageBuffer, pts, _ in
        guard let refCon = refCon else { return }
        let decoder = Unmanaged<VideoDecoder>.fromOpaque(refCon).takeUnretainedValue()
        decoder.handleOutput(status: status, infoFlags: infoFlags, imageBuffer: imageBuffer, pts: pts)
    }

    /// Runs on a VT thread.
    fileprivate func handleOutput(status: OSStatus, infoFlags: VTDecodeInfoFlags, imageBuffer: CVImageBuffer?, pts: CMTime) {
        let key = UInt64(bitPattern: pts.value)
        let nowNanos = DispatchTime.now().uptimeNanoseconds

        lock.lock()
        if inFlight > 0 { inFlight -= 1 }
        let info = pending.removeValue(forKey: key)
        lock.unlock()

        // Hard failure: VT returned an error status. Resync on the next keyframe.
        guard status == noErr else {
            queue.async { [weak self] in
                self?.handleDecodeFailure()
            }
            return
        }

        // VT skipped this frame (no image, or frameDropped). Not a hard failure: count it,
        // request a keyframe through the rate-limited path, and keep decoding later frames.
        guard let image = imageBuffer, !infoFlags.contains(.frameDropped) else {
            countDrop()
            queue.async { [weak self] in
                self?.requestKeyframeRateLimited()
            }
            return
        }

        var decodeUs: UInt32 = 0
        if let info = info, nowNanos > info.submitNanos {
            let elapsedUs = (nowNanos - info.submitNanos) / 1000
            decodeUs = UInt32(min(elapsedUs, UInt64(UInt32.max)))
        }

        lock.lock()
        _framesDecoded &+= 1
        _lastDecodeUs = decodeUs
        let callback = _onFrame
        lock.unlock()

        guard let cb = callback, let sample = makeDisplaySample(image: image, pts: pts) else {
            return
        }
        cb(sample)
    }

    /// Wraps a decoded image buffer in a CMSampleBuffer marked DisplayImmediately.
    private func makeDisplaySample(image: CVImageBuffer, pts: CMTime) -> CMSampleBuffer? {
        var imageDesc: CMVideoFormatDescription?
        let fdStatus = CMVideoFormatDescriptionCreateForImageBuffer(
            allocator: kCFAllocatorDefault,
            imageBuffer: image,
            formatDescriptionOut: &imageDesc)
        guard fdStatus == noErr, let fd = imageDesc else { return nil }

        var timing = CMSampleTimingInfo(
            duration: CMTime.invalid,
            presentationTimeStamp: pts,
            decodeTimeStamp: CMTime.invalid)
        var out: CMSampleBuffer?
        let sbStatus = CMSampleBufferCreateReadyWithImageBuffer(
            allocator: kCFAllocatorDefault,
            imageBuffer: image,
            formatDescription: fd,
            sampleTiming: &timing,
            sampleBufferOut: &out)
        guard sbStatus == noErr, let sb = out else { return nil }

        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dict,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        }
        return sb
    }
}
