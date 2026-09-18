import Foundation

/// The desktop's whole-take silence gate and auto gain, ported from
/// `src-tauri/src/audio.rs` with the same constants and the same test vectors so
/// the two implementations can be cross-checked line for line.
///
/// Nothing here is a re-derivation. `TARGET_PEAK`, `MAX_GAIN`, `SILENCE_LEVEL`,
/// the 95th-percentile index rule and the `1e-4` gain floor are copied. Changing
/// one of them here without changing it there is a bug in this file.
public enum SilenceGate {
    /// Amplitude the 95th-percentile sample should reach after boosting. An
    /// amplitude target, not an RMS one: p95 of speech runs about 1.4x its RMS,
    /// and 0.21 lands the voiced RMS in the 0.13-0.20 band with headroom for
    /// transients. (audio.rs `TARGET_PEAK`)
    public static let targetPeak: Float = 0.21

    /// (audio.rs `MAX_GAIN`)
    public static let maxGain: Float = 20.0

    /// -60 dBFS. A take whose loud part sits under this carried no voice: a
    /// muted, virtual, or permission-blocked input. (audio.rs `SILENCE_LEVEL`)
    public static let silenceLevel: Float = 1e-3

    /// Below this the take has no measurable level at all and boosting it would
    /// only amplify a noise floor. (audio.rs `auto_gain`)
    public static let gainFloor: Float = 1e-4

    /// 95th percentile of |sample|: the level of the loud part of a take, which
    /// leading silence and a single transient both leave alone.
    ///
    /// Keyed on a percentile, not the absolute peak, because the peak is
    /// whatever single loudest thing happened -- a cough, a desk bump, one hard
    /// key press -- so a `peak > 0.5 => give up` rule throws away the boost for
    /// the entire quiet take.
    public static func speechLevel(_ samples: [Float]) -> Float {
        guard !samples.isEmpty else { return 0 }
        var magnitudes = samples.map { abs($0) }
        return percentile(ofMagnitudes: &magnitudes)
    }

    /// The same measurement, over a buffer the caller owns and reuses.
    ///
    /// The capture tap asks for this once per block while stop-on-silence is
    /// armed, so the magnitudes buffer is the caller's to keep: emptying it
    /// with `keepingCapacity` and refilling it allocates nothing after the
    /// first block, where `speechLevel(_:)` allocates a fresh copy every time.
    /// `scratch` is left holding those magnitudes, in no useful order.
    public static func speechLevel(of samples: [Float], scratch: inout [Float]) -> Float {
        scratch.removeAll(keepingCapacity: true)
        guard !samples.isEmpty else { return 0 }
        scratch.reserveCapacity(samples.count)
        for sample in samples { scratch.append(abs(sample)) }
        return percentile(ofMagnitudes: &scratch)
    }

    /// The 95th-percentile index rule, applied to a buffer this is free to
    /// reorder. Rust: `((len as f32 * 0.95) as usize).min(len - 1)` -- truncating,
    /// not rounding. Float32 arithmetic is used deliberately so the index matches.
    private static func percentile(ofMagnitudes magnitudes: inout [Float]) -> Float {
        let scaled = Float(magnitudes.count) * 0.95
        let index = min(Int(scaled), magnitudes.count - 1)
        return selectNth(&magnitudes, index)
    }

    /// The element that would sit at `index` if `values` were sorted, found
    /// without sorting it.
    ///
    /// A selection, not a sort, for the reason audio.rs gives for
    /// `select_nth_unstable_by`: O(n) instead of O(n log n) over a copy that is
    /// tens of megabytes for the longest take, on a path the user is waiting on.
    /// On the phone it also runs per block on the audio thread, where the sort
    /// was the largest thing happening between two microphone callbacks.
    ///
    /// The partition is three-way, which is what makes the adversarial inputs
    /// cheap rather than quadratic: an all-equal block, a block that is already
    /// sorted, and a block of two distinct values all finish in one pass each,
    /// and every one of them is a real capture (a muted input, a fade, a square
    /// wave). `values` is left partitioned around the answer, not sorted.
    static func selectNth(_ values: inout [Float], _ index: Int) -> Float {
        guard !values.isEmpty else { return 0 }
        let target = min(max(index, 0), values.count - 1)
        return values.withUnsafeMutableBufferPointer { buffer -> Float in
            var low = 0
            var high = buffer.count - 1
            while low < high {
                // Median of the two ends and the middle: cheap, and it is what
                // keeps an already-sorted block off the quadratic path.
                let middle = low + (high - low) / 2
                let pivot = medianOfThree(buffer[low], buffer[middle], buffer[high])

                var less = low
                var scan = low
                var greater = high
                while scan <= greater {
                    let value = buffer[scan]
                    if value < pivot {
                        buffer.swapAt(less, scan)
                        less += 1
                        scan += 1
                    } else if value > pivot {
                        buffer.swapAt(scan, greater)
                        greater -= 1
                    } else {
                        // Equal to the pivot, and so is anything NaN: Rust's
                        // comparator falls back to `Ordering::Equal` for those
                        // too, so neither implementation lets one loop forever.
                        scan += 1
                    }
                }

                if target < less {
                    high = less - 1
                } else if target > greater {
                    low = greater + 1
                } else {
                    return pivot
                }
            }
            return buffer[low]
        }
    }

    private static func medianOfThree(_ a: Float, _ b: Float, _ c: Float) -> Float {
        if a < b {
            if b < c { return b }
            return a < c ? c : a
        }
        if a < c { return a }
        return b < c ? c : b
    }

    /// A take whose loud part sits under -60 dBFS carried no voice.
    ///
    /// This is a whole-take gate, not per-sample silence stripping: that was
    /// removed twice from the desktop (3a9ebee, 0865284) for cutting speech from
    /// low-gain mics, and a quiet real take still measures 10x to 50x above this
    /// line.
    public static func isSilent(_ samples: [Float]) -> Bool {
        speechLevel(samples) < silenceLevel
    }

    /// Boost quiet recordings so the speech-to-text model gets a usable level.
    /// Never clips: the result is clamped to [-1, 1].
    public static func autoGain(_ samples: [Float]) -> [Float] {
        guard !samples.isEmpty else { return [] }
        let level = speechLevel(samples)
        if level < gainFloor { return samples }
        let gain = min(max(targetPeak / level, 1.0), maxGain)
        return samples.map { min(max($0 * gain, -1.0), 1.0) }
    }

    /// The desktop refuses to upload a dead take rather than let Whisper
    /// hallucinate over it. On the phone there is no upload, but the same take
    /// would make Qwen invent a sentence, so the sheet says so instead.
    public static func rejectionMessage(deviceName: String) -> String {
        "No sound reached OpenFlow from \"\(deviceName)\". Check the microphone permission in Settings."
    }
}
