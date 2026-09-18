import Foundation
import Testing
@testable import OpenFlowMobileCore

/// These are the desktop's tests, ported sample for sample from
/// `src-tauri/src/audio.rs`. If one of them fails here and passes there, the two
/// implementations have drifted and the phone is no longer doing what the Mac
/// does. That is the whole point of copying the vectors rather than inventing
/// new ones.
@Suite struct SilenceGateTests {

    /// The constants are the contract with audio.rs. Pinned literally so a
    /// well-meaning tweak has to argue with a test.
    @Test func testConstantsMatchTheDesktop() {
        #expect(SilenceGate.targetPeak == 0.21)
        #expect(SilenceGate.maxGain == 20.0)
        #expect(SilenceGate.silenceLevel == 1e-3)
        #expect(SilenceGate.gainFloor == 1e-4)
        #expect(AudioResampler.firTaps == 63)
    }

    /// audio.rs `silence_gate_rejects_dead_input_but_keeps_quiet_speech`
    @Test func testSilenceGateRejectsDeadInputButKeepsQuietSpeech() {
        #expect(SilenceGate.isSilent([Float](repeating: 0, count: 16_000)))

        let hiss: [Float] = (0..<16_000).map { $0 % 2 == 0 ? 2e-4 : -2e-4 }
        #expect(SilenceGate.isSilent(hiss), "a virtual device's noise floor is not speech")

        let quiet = tone(300, 16_000, 1.0).map { $0 * 0.01 }
        #expect(!SilenceGate.isSilent(quiet), "a quiet real take must pass the gate")

        let mostlySilent = [Float](repeating: 0, count: 32_000) + quiet
        #expect(
            !SilenceGate.isSilent(mostlySilent),
            "two seconds of leading silence must not fail a real take"
        )
    }

    /// audio.rs `auto_gain_survives_one_loud_transient`
    @Test func testAutoGainSurvivesOneLoudTransient() {
        let quiet = tone(300, 16_000, 1.0).map { $0 * 0.03 }
        let cleanLevel = rms(SilenceGate.autoGain(quiet))

        var bumped = quiet
        for index in 0..<200 { bumped[index] = 0.95 }
        let bumpedLevel = rms(SilenceGate.autoGain(bumped))

        #expect(cleanLevel > rms(quiet) * 2, "quiet take must be boosted")
        #expect(bumpedLevel > cleanLevel * 0.5, "one transient must not cancel the boost: clean=\(cleanLevel) bumped=\(bumpedLevel)")
    }

    /// audio.rs `auto_gain_leaves_silence_alone_and_never_clips`
    @Test func testAutoGainLeavesSilenceAloneAndNeverClips() {
        let silence = [Float](repeating: 0, count: 1_000)
        #expect(SilenceGate.autoGain(silence) == silence)
        #expect(SilenceGate.autoGain([]).isEmpty)
        #expect(SilenceGate.autoGain(tone(300, 16_000, 0.2)).allSatisfy { abs($0) <= 1.0 })
    }

    /// The percentile itself, not just its consequences: a run of 100 samples
    /// where only the top 5 are loud must report the quiet level, and the
    /// truncating index rule must match Rust's `as usize` cast.
    @Test func testSpeechLevelIsTheNinetyFifthPercentileNotThePeak() {
        var samples = [Float](repeating: 0.1, count: 95)
        samples.append(contentsOf: [Float](repeating: 0.9, count: 5))
        #expect(abs((SilenceGate.speechLevel(samples)) - (0.9)) < 1e-6)

        var quieter = [Float](repeating: 0.1, count: 96)
        quieter.append(contentsOf: [Float](repeating: 0.9, count: 4))
        #expect(abs((SilenceGate.speechLevel(quieter)) - (0.1)) < 1e-6)

        #expect(SilenceGate.speechLevel([]) == 0)
        #expect(SilenceGate.speechLevel([0.5]) == 0.5)
    }

    /// The gate has to be capable of failing, or it is decoration. A take one
    /// notch either side of the line must land on opposite sides of it.
    @Test func testTheGateActuallyFiresAtItsThreshold() {
        let justUnder = [Float](repeating: SilenceGate.silenceLevel * 0.9, count: 1_000)
        let justOver = [Float](repeating: SilenceGate.silenceLevel * 1.1, count: 1_000)
        #expect(SilenceGate.isSilent(justUnder))
        #expect(!SilenceGate.isSilent(justOver))
    }

    /// The selection has to agree with the sort it replaced, on every index of
    /// every shape, or the percentile has quietly moved.
    ///
    /// The sorted array is the oracle: `selectNth(&values, i)` must equal
    /// `values.sorted()[i]`, which is the definition `select_nth_unstable_by`
    /// satisfies in audio.rs. The adversarial shapes are here because they are
    /// what a naive quickselect is quadratic on and they are all real captures:
    /// all-equal is a muted input, already-sorted is a fade, two values is a
    /// square wave, and one sample is the shortest block a tap can hand over.
    @Test func testSelectionAgreesWithTheSortItReplaced() {
        var generator = SeededGenerator(seed: 0x0F10_2026)
        var cases: [[Float]] = [
            [0.5],
            [0.5, 0.5],
            [1, 0],
            [Float](repeating: 0.25, count: 999),
            (0..<1_000).map { Float($0) / 1_000 },
            (0..<1_000).map { Float(999 - $0) / 1_000 },
            (0..<1_000).map { $0 % 2 == 0 ? Float(0.1) : Float(0.9) },
            (0..<777).map { Float($0 % 7) },
        ]
        for length in [2, 3, 17, 256, 1_001] {
            cases.append((0..<length).map { _ in Float(generator.nextUnit()) })
        }

        for values in cases {
            let sorted = values.sorted()
            for index in 0..<values.count {
                var scratch = values
                let selected = SilenceGate.selectNth(&scratch, index)
                #expect(
                    selected == sorted[index],
                    "length \(values.count), index \(index): selected \(selected), sorted \(sorted[index])"
                )
            }
        }
    }

    /// An index off either end lands on that end, and an empty block is zero
    /// rather than a trap: the caller computes the index in Float32 to match
    /// Rust, and a rounding surprise there should not take a recording with it.
    ///
    /// Faked this before trusting it. The first version claimed to be testing
    /// the `min(max(index, 0), count - 1)` clamp, and removing that clamp left
    /// it green: the narrowing walks a low index down to the minimum and a high
    /// one up to the maximum on its own, so the clamp is belt and braces and
    /// there is no input that can prove it is there. What the assertions below
    /// do hold is the guard on the empty array, which is the branch that turns
    /// into a crash when it goes.
    @Test func testSelectionOfAnIndexOffEitherEndLandsOnThatEnd() {
        var values: [Float] = [3, 1, 2]
        #expect(SilenceGate.selectNth(&values, -5) == 1)
        var again: [Float] = [3, 1, 2]
        #expect(SilenceGate.selectNth(&again, 99) == 3)
        var empty: [Float] = []
        #expect(SilenceGate.selectNth(&empty, 0) == 0)
        #expect(SilenceGate.speechLevel([]) == 0)
    }

    /// The tap's allocation-free entry point has to report the same number as
    /// the one that copies, including on a second call that reuses the buffer:
    /// a scratch still holding the previous block's magnitudes is exactly the
    /// bug this shape invites.
    @Test func testScratchLevelMatchesTheCopyingOne() {
        var generator = SeededGenerator(seed: 0xA11D_0C10)
        let loud = (0..<4_096).map { _ in Float(generator.nextUnit()) }
        let quiet = loud.map { $0 * 0.001 }
        let short: [Float] = [0.2, -0.4, 0.1]

        var scratch: [Float] = []
        #expect(SilenceGate.speechLevel(of: loud, scratch: &scratch) == SilenceGate.speechLevel(loud))
        #expect(SilenceGate.speechLevel(of: quiet, scratch: &scratch) == SilenceGate.speechLevel(quiet))
        #expect(SilenceGate.speechLevel(of: short, scratch: &scratch) == SilenceGate.speechLevel(short))
        #expect(SilenceGate.speechLevel(of: [], scratch: &scratch) == 0)
        #expect(scratch.isEmpty, "an empty block must leave nothing behind for the next one")
        #expect(SilenceGate.speechLevel(of: loud, scratch: &scratch) == SilenceGate.speechLevel(loud))
    }
}

/// A generator the tests can repeat exactly. `SystemRandomNumberGenerator` would
/// make a failure a story about which run it was.
struct SeededGenerator: RandomNumberGenerator {
    private var state: UInt64

    init(seed: UInt64) { self.state = seed == 0 ? 0x9E37_79B9_7F4A_7C15 : seed }

    mutating func next() -> UInt64 {
        state ^= state << 13
        state ^= state >> 7
        state ^= state << 17
        return state
    }

    /// A float in [-1, 1), the range a capture block lives in.
    mutating func nextUnit() -> Float {
        Float(Double(next() >> 11) / Double(1 << 53)) * 2 - 1
    }
}
