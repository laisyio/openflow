import Foundation

#if canImport(AVFoundation)
import AVFoundation
#endif

// MARK: - The maths (pure, and the part the tests exercise)

/// Sample-rate conversion, ported from `downsample` / `design_lowpass` in
/// `src-tauri/src/audio.rs`.
///
/// Interpolation alone does not prevent aliasing: at 48k -> 16k every component
/// above the new 8 kHz Nyquist folds back into the speech band regardless of how
/// the output points are interpolated -- a 15 kHz whine lands on 1 kHz, right on
/// top of the voice. Measured on the desktop, plain decimation and linear
/// interpolation both leave the alias at -0.0 dB; filtering first drops it to
/// -60 dB while the passband below 6 kHz stays within 0.1 dB.
public enum AudioResampler {
    /// Anti-alias filter length. 63 taps buys ~60 dB of stopband rejection at
    /// 48k -> 16k, far more than speech needs. (audio.rs `FIR_TAPS`)
    public static let firTaps = 63

    /// Windowed-sinc low-pass, Hamming window, normalised to unity DC gain.
    public static func designLowpass(cutoffHz: Float, sampleRate: Float, numTaps: Int) -> [Float] {
        let fc = cutoffHz / sampleRate
        let m = Float(numTaps - 1)
        var taps = [Float]()
        taps.reserveCapacity(numTaps)
        for i in 0..<numTaps {
            let n = Float(i) - m / 2
            let sinc: Float
            if abs(n) < 1e-6 {
                sinc = 2 * fc
            } else {
                sinc = sin(2 * .pi * fc * n) / (.pi * n)
            }
            let window = 0.54 - 0.46 * cos(2 * .pi * Float(i) / m)
            taps.append(sinc * window)
        }
        let sum = taps.reduce(0, +)
        if abs(sum) < 1e-9 { return taps }
        return taps.map { $0 / sum }
    }

    /// The filter used for a decimation to `toRate`.
    ///
    /// 0.45 * toRate leaves a transition band below the new Nyquist while keeping
    /// everything speech uses (< 7.2 kHz at a 16 kHz output).
    public static func decimationTaps(fromRate: Double, toRate: Double) -> [Float] {
        designLowpass(
            cutoffHz: 0.45 * Float(toRate),
            sampleRate: Float(fromRate),
            numTaps: firTaps
        )
    }

    /// Resample to `toRate`, low-passing first when decimating.
    public static func downsample(_ samples: [Float], from fromRate: Double, to toRate: Double) -> [Float] {
        if fromRate == toRate { return samples }
        if samples.isEmpty || fromRate <= 0 || toRate <= 0 { return [] }

        let ratio = fromRate / toRate
        let outputLength = Int(Double(samples.count) / ratio)
        if outputLength == 0 { return [] }

        // Upsampling needs interpolation, not decimation; no anti-alias filter
        // applies. Keep the linear interpolation for that direction.
        if fromRate < toRate {
            var output = [Float]()
            output.reserveCapacity(outputLength)
            for i in 0..<outputLength {
                let position = Double(i) * ratio
                let left = Int(position.rounded(.down))
                if left >= samples.count { break }
                let right = min(left + 1, samples.count - 1)
                let fraction = Float(position - Double(left))
                output.append(samples[left] + (samples[right] - samples[left]) * fraction)
            }
            return output
        }

        let taps = decimationTaps(fromRate: fromRate, toRate: toRate)
        let half = taps.count / 2
        var output = [Float]()
        output.reserveCapacity(outputLength)
        for i in 0..<outputLength {
            let center = Int(Double(i) * ratio)
            var acc: Float = 0
            for (k, tap) in taps.enumerated() {
                let index = center + k - half
                if index >= 0 && index < samples.count {
                    acc += samples[index] * tap
                }
            }
            output.append(acc)
        }
        return output
    }
}

/// The same decimation, fed a block at a time from the microphone tap.
///
/// The desktop can accumulate a whole take at the native rate and convert once,
/// because it has the memory to spare. The phone's ring is 16 kHz (PLAN.md
/// section 5), so the conversion happens in the tap. This keeps the filter's
/// input history across block boundaries so the result is identical to
/// converting the whole take at once -- `flush()` closes out the tail with the
/// same zero padding the batch version uses.
public struct StreamingDownsampler: Sendable {
    private let ratio: Double
    private let taps: [Float]
    private let half: Int
    private let passthrough: Bool

    /// Input samples still needed by the filter, oldest first.
    private var pending: [Float] = []
    /// Global index of `pending[0]` in the input stream.
    private var pendingBase = 0
    /// Total input samples seen.
    private var inputCount = 0
    /// Output samples emitted so far.
    private var outputCount = 0

    public init(from fromRate: Double, to toRate: Double) {
        self.passthrough = (fromRate == toRate) || fromRate <= 0 || toRate <= 0
        self.ratio = passthrough ? 1 : fromRate / toRate
        if passthrough || fromRate < toRate {
            self.taps = []
            self.half = 0
        } else {
            let designed = AudioResampler.decimationTaps(fromRate: fromRate, toRate: toRate)
            self.taps = designed
            self.half = designed.count / 2
        }
    }

    /// Convert one block. Returns only the output samples the filter can produce
    /// without seeing the future.
    public mutating func process(_ block: [Float]) -> [Float] {
        var output = [Float]()
        block.withUnsafeBufferPointer { process($0, into: &output) }
        return output
    }

    /// The same conversion, writing into a buffer the caller owns.
    ///
    /// The tap calls this once per microphone callback, so `output` is emptied
    /// with `keepingCapacity` rather than replaced: after the first few blocks
    /// it is already large enough and the conversion allocates nothing.
    public mutating func process(_ block: UnsafeBufferPointer<Float>, into output: inout [Float]) {
        output.removeAll(keepingCapacity: true)
        guard !block.isEmpty else { return }
        if passthrough || taps.isEmpty {
            inputCount += block.count
            outputCount += block.count
            output.append(contentsOf: block)
            return
        }
        pending.append(contentsOf: block)
        inputCount += block.count
        emit(upTo: inputCount, zeroPadTail: false, into: &output)
    }

    /// Close out the take: emit every remaining output sample, padding past the
    /// end of the input with zeros exactly as the batch converter does.
    public mutating func flush() -> [Float] {
        guard !passthrough, !taps.isEmpty else { return [] }
        var tail = [Float]()
        emit(upTo: inputCount, zeroPadTail: true, into: &tail)
        pending.removeAll(keepingCapacity: true)
        return tail
    }

    private mutating func emit(upTo availableInputs: Int, zeroPadTail: Bool, into output: inout [Float]) {
        let totalOutputs = Int(Double(availableInputs) / ratio)
        while outputCount < totalOutputs {
            let center = Int(Double(outputCount) * ratio)
            let lastNeeded = center + taps.count - 1 - half
            if !zeroPadTail && lastNeeded >= availableInputs { break }
            var acc: Float = 0
            for (k, tap) in taps.enumerated() {
                let index = center + k - half
                if index >= 0 && index < availableInputs {
                    let local = index - pendingBase
                    if local >= 0 && local < pending.count {
                        acc += pending[local] * tap
                    }
                }
            }
            output.append(acc)
            outputCount += 1
            // Drop history the next output can no longer reach.
            let nextCenter = Int(Double(outputCount) * ratio)
            let keepFrom = max(0, nextCenter - half)
            if keepFrom > pendingBase {
                pending.removeFirst(min(keepFrom - pendingBase, pending.count))
                pendingBase = keepFrom
            }
        }
    }
}

/// Preallocated 16 kHz Float32 ring. PLAN.md section 5: the capture pipeline
/// allocates once per take, and ten minutes is the ceiling.
///
/// Ten minutes at 16 kHz is 9.6 M floats, 38.4 MB. When a take runs past that the
/// ring keeps the most recent ten minutes and raises `didOverflow`, which is the
/// watchdog's cue to stop the capture -- the desktop's `MAX_CAPTURE_SAMPLES`
/// rule, translated to a bounded ring instead of a bounded vector.
public struct CaptureRingBuffer: Sendable {
    public static let sampleRate: Double = 16_000
    public static let maxSeconds: Double = 600

    /// What the user is told when a take ran past `maxSeconds`.
    ///
    /// It lives here, next to the constant that makes it true, for the reason
    /// audio.rs gives for `CAPTURE_CEILING_WARNING`: the sentence and the
    /// ceiling have to change together.
    ///
    /// It names no duration. The desktop's reason is that its ceiling is a frame
    /// count that means different clock times on different hardware; ours is
    /// that a figure in a sentence is one more thing to forget to update when
    /// `maxSeconds` moves, and a wrong figure is worse than none.
    ///
    /// It says the *beginning* was lost, which is the opposite of what the
    /// desktop says, because this is a ring and that is a bounded vector. The
    /// desktop drops frames as they arrive, so the opening survives; here the
    /// newest samples overwrite the oldest, so the closing survives. Which half
    /// is missing is the one thing the user cannot guess, so the two messages
    /// have to disagree.
    public static let ceilingNotice =
        "This take reached OpenFlow's length limit, so the beginning of it was not kept. "
        + "Everything said after that point is here."

    public let capacity: Int
    private var storage: [Float]
    private var writeIndex = 0
    public private(set) var totalWritten = 0

    public init(capacity: Int = Int(sampleRate * maxSeconds)) {
        self.capacity = max(1, capacity)
        self.storage = [Float](repeating: 0, count: max(1, capacity))
    }

    public var didOverflow: Bool { totalWritten > capacity }
    public var count: Int { min(totalWritten, capacity) }
    public var seconds: Double { Double(count) / Self.sampleRate }

    public mutating func append(_ samples: [Float]) {
        samples.withUnsafeBufferPointer { append($0) }
    }

    /// One block into the ring, in at most two copies.
    ///
    /// The sample-at-a-time version this replaces did a bounds check, a modulo
    /// and a retain-free-but-still-real store per sample, several thousand times
    /// per microphone callback on the audio thread. The ring wraps in at most
    /// one place, so a block is one copy to the end of the storage and, when it
    /// wraps, a second copy to the front.
    public mutating func append(_ samples: UnsafeBufferPointer<Float>) {
        guard let source = samples.baseAddress, !samples.isEmpty else { return }

        var offset = 0
        var index = writeIndex
        var remaining = samples.count
        // A block longer than the whole ring can only leave its own last
        // `capacity` samples behind, so start at those and copy each slot once.
        // The write index lands where the per-sample loop left it either way.
        if remaining > capacity {
            offset = remaining - capacity
            index = (writeIndex + offset) % capacity
            remaining = capacity
        }

        storage.withUnsafeMutableBufferPointer { destination in
            guard let base = destination.baseAddress else { return }
            let head = min(remaining, capacity - index)
            (base + index).update(from: source + offset, count: head)
            if remaining > head {
                base.update(from: source + offset + head, count: remaining - head)
            }
        }

        writeIndex = (index + remaining) % capacity
        totalWritten += samples.count
    }

    /// The take, oldest sample first.
    public func snapshot() -> [Float] {
        let available = count
        guard available > 0 else { return [] }
        if totalWritten <= capacity {
            return Array(storage[0..<available])
        }
        return Array(storage[writeIndex..<capacity]) + Array(storage[0..<writeIndex])
    }

    public mutating func reset() {
        writeIndex = 0
        totalWritten = 0
    }
}

/// What a finished take produced.
public struct CaptureResult: Sendable {
    /// 16 kHz mono Float32, auto-gained, ready for `SpeechEngine.transcribe`.
    public var samples16k: [Float]
    public var seconds: Double
    /// True when the whole-take gate says nothing reached the microphone.
    public var isSilent: Bool
    /// True when the ten-minute watchdog cut the take short.
    public var hitWatchdog: Bool
}

public enum AudioCaptureError: Error, Equatable, Sendable {
    /// The user has refused the microphone, or has not been asked and declined
    /// the prompt. Actionable: the sheet points at Settings.
    case permissionDenied
    case engineUnavailable(String)
    case notRecording
    case tooShort
}

/// Shared between the real-time tap and the actor. The tap runs on an audio
/// thread with no actor of its own, so the ring lives behind a lock rather than
/// inside the actor: `@unchecked Sendable` because the lock is the proof.
final class CaptureBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var ring: CaptureRingBuffer
    private var downsampler: StreamingDownsampler

    /// Whether the per-block level is worth computing at all. Only
    /// stop-on-silence reads it, and it is the one measurement in the tap that
    /// touches every sample twice, so a user who has the setting off should not
    /// be paying for it on the audio thread.
    private let measuresLevel: Bool
    private var lastLevel: Float = 0

    /// Buffers the tap reuses. Each one grows to a block's working size on the
    /// first callbacks and is then refilled in place, so the steady state of a
    /// take allocates nothing on the audio thread.
    private var mono: [Float] = []
    private var converted: [Float] = []
    private var levelScratch: [Float] = []

    init(inputRate: Double, measuresLevel: Bool = false) {
        self.ring = CaptureRingBuffer()
        self.downsampler = StreamingDownsampler(from: inputRate, to: CaptureRingBuffer.sampleRate)
        self.measuresLevel = measuresLevel
    }

    func write(mono block: [Float]) {
        block.withUnsafeBufferPointer { write($0) }
    }

    /// Average every channel into the reused mono buffer, then convert and store
    /// it. Same rule as the desktop's `mix_frame_to_mono`: average every
    /// channel, never pick channel 0 and hope.
    ///
    /// The mix writes into a buffer owned by this object rather than a fresh
    /// array per callback, because a callback is an audio thread and an
    /// allocation there is a malloc lock the microphone is waiting on.
    func writeMixedMono(
        channels: UnsafePointer<UnsafeMutablePointer<Float>>,
        channelCount: Int,
        frames: Int
    ) {
        guard frames > 0, channelCount > 0 else { return }
        if mono.count < frames {
            mono = [Float](repeating: 0, count: frames)
        }
        mono.withUnsafeMutableBufferPointer { destination in
            guard let base = destination.baseAddress else { return }
            if channelCount == 1 {
                base.update(from: channels[0], count: frames)
                return
            }
            let scale = 1 / Float(channelCount)
            base.update(from: channels[0], count: frames)
            for channel in 1..<channelCount {
                let source = channels[channel]
                for frame in 0..<frames { base[frame] += source[frame] }
            }
            for frame in 0..<frames { base[frame] *= scale }
        }
        mono.withUnsafeBufferPointer { filled in
            write(UnsafeBufferPointer(rebasing: filled[0..<frames]))
        }
    }

    private func write(_ block: UnsafeBufferPointer<Float>) {
        downsampler.process(block, into: &converted)
        guard !converted.isEmpty else { return }
        let level = measuresLevel
            ? SilenceGate.speechLevel(of: converted, scratch: &levelScratch)
            : 0
        lock.lock()
        ring.append(converted)
        if measuresLevel { lastLevel = level }
        lock.unlock()
    }

    /// The most recent block's 95th-percentile level, for stop-on-silence.
    ///
    /// `nil` when this take was opened without level measurement. Not zero: a
    /// zero is a perfectly good reading of a muted microphone, so anything that
    /// compares it against the silence line would conclude silence from a
    /// measurement nobody took.
    var level: Float? {
        guard measuresLevel else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return lastLevel
    }

    var didOverflow: Bool {
        lock.lock()
        defer { lock.unlock() }
        return ring.didOverflow
    }

    func finish() -> (samples: [Float], overflowed: Bool) {
        lock.lock()
        let tail = downsampler.flush()
        if !tail.isEmpty { ring.append(tail) }
        let samples = ring.snapshot()
        let overflowed = ring.didOverflow
        lock.unlock()
        return (samples, overflowed)
    }
}

#if canImport(AVFoundation)

/// The microphone tap. AVAudioEngine gives us the input node's native format; we
/// mix to mono and decimate to 16 kHz in the tap so the ring stays at the size
/// PLAN.md section 5 budgets for.
public actor AudioCapture {
    private var engine: AVAudioEngine?
    private var buffer: CaptureBuffer?
    private var startedAt: Date?

    public init() {}

    public var isRecording: Bool { engine != nil }

    /// The most recent block level, so the sheet can run stop-on-silence without
    /// reaching into the audio thread itself. `nil` when the take was not opened
    /// with `measuringLevel`, which is also when nothing is asking.
    public var currentLevel: Float? { buffer?.level }

    /// True once the ten-minute watchdog has tripped.
    public var watchdogTripped: Bool { buffer?.didOverflow ?? false }

    /// Ask for the microphone, then open it.
    ///
    /// The permission prompt comes first and on its own. Without it a denied or
    /// not-yet-asked microphone reaches `AVAudioEngine.start()` and comes back as
    /// an opaque failure -- on the Simulator, often no failure at all, just
    /// silence that the whole-take gate then rejects as a dead input. Neither
    /// tells the user the one thing they can act on, which is that the switch in
    /// Settings is off.
    ///
    /// `measuringLevel` is stop-on-silence asking for a running level. It is a
    /// parameter and not something the tap always does because the measurement
    /// walks the block twice and runs on the audio thread: with the setting off
    /// there is no reader for the number, and the desktop likewise only computes
    /// what it is about to use.
    public func start(measuringLevel: Bool = false) async throws {
        guard engine == nil else { return }
        #if os(iOS)
        // iOS 17 replaced AVAudioSession.requestRecordPermission with this. The
        // package targets iOS 18, so there is no older path to keep.
        guard await AVAudioApplication.requestRecordPermission() else {
            throw AudioCaptureError.permissionDenied
        }
        let session = AVAudioSession.sharedInstance()
        do {
            // `.record` and not `.playAndRecord`: OpenFlow never plays anything,
            // and the narrower category is one less thing to explain at review.
            try session.setCategory(.record, mode: .measurement)
            try session.setActive(true, options: [])
        } catch {
            throw AudioCaptureError.engineUnavailable(error.localizedDescription)
        }
        #endif

        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.inputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else {
            throw AudioCaptureError.engineUnavailable("The input node reported no usable format")
        }
        let capture = CaptureBuffer(inputRate: format.sampleRate, measuresLevel: measuringLevel)
        input.installTap(onBus: 0, bufferSize: 4_096, format: format) { pcm, _ in
            guard let channels = pcm.floatChannelData else { return }
            let frames = Int(pcm.frameLength)
            guard frames > 0 else { return }
            // Everything this callback needs already exists: the mix, the
            // conversion and the ring all write into buffers the capture owns.
            capture.writeMixedMono(
                channels: channels,
                channelCount: Int(pcm.format.channelCount),
                frames: frames
            )
        }
        do {
            engine.prepare()
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            throw AudioCaptureError.engineUnavailable(error.localizedDescription)
        }
        self.engine = engine
        self.buffer = capture
        self.startedAt = Date()
    }

    /// Stop and hand back the take, auto-gained and gate-checked.
    public func stop() throws -> CaptureResult {
        guard let engine, let buffer else { throw AudioCaptureError.notRecording }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        self.engine = nil
        self.buffer = nil
        self.startedAt = nil
        #if os(iOS)
        try? AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
        #endif

        let (samples, overflowed) = buffer.finish()
        // The desktop refuses anything under 800 samples (50 ms) as a mis-tap.
        guard samples.count >= 800 else { throw AudioCaptureError.tooShort }
        let silent = SilenceGate.isSilent(samples)
        return CaptureResult(
            samples16k: SilenceGate.autoGain(samples),
            seconds: Double(samples.count) / CaptureRingBuffer.sampleRate,
            isSilent: silent,
            hitWatchdog: overflowed
        )
    }

    /// Abandon a take without producing a transcript.
    public func cancel() {
        guard let engine else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        self.engine = nil
        self.buffer = nil
        self.startedAt = nil
        #if os(iOS)
        try? AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
        #endif
    }
}

#endif
