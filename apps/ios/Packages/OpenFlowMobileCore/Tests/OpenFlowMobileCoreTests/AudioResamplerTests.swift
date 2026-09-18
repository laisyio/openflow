import Foundation
import Testing
@testable import OpenFlowMobileCore

@Suite struct AudioResamplerTests {

    /// audio.rs `downsample_interpolates_and_preserves_duration`
    @Test func testDownsampleInterpolatesAndPreservesDuration() {
        let input: [Float] = (0..<48_000).map { Float($0) / 48_000 }
        let output = AudioResampler.downsample(input, from: 48_000, to: 16_000)
        #expect(output.count == 16_000)
        #expect(abs((output[8_000]) - (0.5)) < 0.001)
    }

    /// audio.rs `downsample_rejects_aliasing`. 15 kHz folds onto 1 kHz at a
    /// 16 kHz output rate; linear interpolation leaves it at full strength.
    @Test func testDownsampleRejectsAliasing() {
        let out = AudioResampler.downsample(tone(15_000, 48_000, 0.5), from: 48_000, to: 16_000)
        let ghost = energyAt(out, 16_000, 1_000)
        #expect(ghost < 0.05, "15 kHz aliased into the speech band: \(ghost)")
    }

    /// The anti-alias test above only means something if the same measurement
    /// screams when the filter is removed. Plain decimation of the same tone
    /// must leave the ghost at full strength -- otherwise the assertion above
    /// would pass with no filter at all.
    @Test func testTheAliasTestWouldFailWithoutTheFilter() {
        let input = tone(15_000, 48_000, 0.5)
        let decimated = stride(from: 0, to: input.count, by: 3).map { input[$0] }
        let ghost = energyAt(decimated, 16_000, 1_000)
        #expect(ghost > 0.5, "plain decimation must alias, or the filter test proves nothing: \(ghost)")
    }

    /// audio.rs `downsample_preserves_speech_band`
    @Test func testDownsamplePreservesSpeechBand() {
        let out = AudioResampler.downsample(tone(1_000, 48_000, 0.5), from: 48_000, to: 16_000)
        #expect(energyAt(out, 16_000, 1_000) > 0.8, "1 kHz speech tone must survive decimation")
    }

    @Test func testDownsampleEdgeCases() {
        #expect(AudioResampler.downsample([], from: 48_000, to: 16_000) == [])
        let passthrough: [Float] = [0.1, 0.2, 0.3]
        #expect(AudioResampler.downsample(passthrough, from: 16_000, to: 16_000) == passthrough)
        // Upsampling takes the interpolation path, not the filter path.
        let up = AudioResampler.downsample([0, 1], from: 8_000, to: 16_000)
        #expect(up.count == 4)
    }

    /// The microphone tap converts a block at a time. If that is not identical to
    /// converting the whole take at once, the ring holds something the desktop
    /// would never have produced.
    @Test func testStreamingConversionMatchesTheBatchConversion() {
        let input = tone(440, 48_000, 0.35)
        let batch = AudioResampler.downsample(input, from: 48_000, to: 16_000)

        var streaming = StreamingDownsampler(from: 48_000, to: 16_000)
        var produced: [Float] = []
        for block in stride(from: 0, to: input.count, by: 1_024) {
            let end = min(block + 1_024, input.count)
            produced.append(contentsOf: streaming.process(Array(input[block..<end])))
        }
        produced.append(contentsOf: streaming.flush())

        #expect(produced.count == batch.count)
        for index in 0..<min(produced.count, batch.count) {
            #expect(abs((produced[index]) - (batch[index])) < 1e-6, "sample \(index)")
        }
    }

    @Test func testRingBufferKeepsTheMostRecentAudioAndFlagsTheWatchdog() {
        var ring = CaptureRingBuffer(capacity: 5)
        ring.append([1, 2, 3])
        #expect(ring.snapshot() == [1, 2, 3])
        #expect(!ring.didOverflow)

        ring.append([4, 5, 6, 7])
        #expect(ring.didOverflow, "past capacity the watchdog must say so")
        #expect(ring.snapshot() == [3, 4, 5, 6, 7], "the ring keeps the newest audio")

        ring.reset()
        #expect(ring.snapshot() == [])
        #expect(!ring.didOverflow)
    }

    /// PLAN.md section 5 budgets ten minutes at 16 kHz, allocated once.
    @Test func testDefaultRingIsTenMinutesAtSixteenKilohertz() {
        let ring = CaptureRingBuffer()
        #expect(ring.capacity == 9_600_000)
        #expect(CaptureRingBuffer.maxSeconds == 600)
        #expect(CaptureRingBuffer.sampleRate == 16_000)
    }

    /// The bulk copy has to land every sample exactly where the sample-at-a-time
    /// loop did, which is a claim about the boundaries and not about the middle.
    ///
    /// Three of them: a block that stops one short of the end of the storage, a
    /// block that fills it exactly and leaves the write index at zero, and a
    /// block that wraps, which is the case that becomes two copies. A block
    /// longer than the whole ring is the fourth, because it is the one the
    /// per-sample loop handled by writing every slot several times over.
    @Test func testRingAppendsInBulkAcrossEveryBoundary() {
        var ring = CaptureRingBuffer(capacity: 4)
        ring.append([1, 2, 3])
        #expect(ring.snapshot() == [1, 2, 3], "short of the end")
        ring.append([4])
        #expect(ring.snapshot() == [1, 2, 3, 4], "exactly full")
        #expect(!ring.didOverflow, "exactly full is not past the ceiling")
        #expect(ring.count == 4)

        ring.append([5, 6])
        #expect(ring.snapshot() == [3, 4, 5, 6], "the wrap is still one sequence")
        #expect(ring.didOverflow)

        var exact = CaptureRingBuffer(capacity: 4)
        exact.append([1, 2, 3, 4])
        #expect(exact.snapshot() == [1, 2, 3, 4])
        #expect(!exact.didOverflow)
        exact.append([5, 6, 7, 8])
        #expect(exact.snapshot() == [5, 6, 7, 8], "a full ring, replaced wholesale")

        var flooded = CaptureRingBuffer(capacity: 3)
        flooded.append([1])
        flooded.append([2, 3, 4, 5, 6, 7, 8])
        #expect(flooded.snapshot() == [6, 7, 8], "only the tail of an over-long block survives")
        #expect(flooded.didOverflow)
        // The write index has to be where the per-sample loop left it, or the
        // next block lands in the wrong slot rather than merely being wrong now.
        flooded.append([9])
        #expect(flooded.snapshot() == [7, 8, 9])
    }

    /// The same result from the same samples, block by block, against a ring
    /// that was fed one sample at a time. This is the property the rewrite has
    /// to keep and the one a bounds slip breaks silently.
    @Test func testBulkAppendMatchesSampleBySampleAppend() {
        var generator = SeededGenerator(seed: 0xB105_0000)
        let blocks: [[Float]] = (0..<40).map { _ in
            (0..<Int(generator.next() % 23 + 1)).map { _ in generator.nextUnit() }
        }

        var bulk = CaptureRingBuffer(capacity: 17)
        var single = CaptureRingBuffer(capacity: 17)
        for block in blocks {
            bulk.append(block)
            for sample in block { single.append([sample]) }
            #expect(bulk.snapshot() == single.snapshot())
            #expect(bulk.didOverflow == single.didOverflow)
            #expect(bulk.count == single.count)
        }
    }

    /// The reused-buffer conversion the tap uses has to agree, sample for
    /// sample, with the allocating one the tests already trust.
    @Test func testStreamingConversionIntoAReusedBufferMatches() {
        let input = tone(440, 48_000, 0.2)
        var allocating = StreamingDownsampler(from: 48_000, to: 16_000)
        var reusing = StreamingDownsampler(from: 48_000, to: 16_000)
        var scratch: [Float] = []
        var fromScratch: [Float] = []
        var fromArrays: [Float] = []

        for start in stride(from: 0, to: input.count, by: 1_024) {
            let end = min(start + 1_024, input.count)
            let block = Array(input[start..<end])
            fromArrays.append(contentsOf: allocating.process(block))
            block.withUnsafeBufferPointer { reusing.process($0, into: &scratch) }
            fromScratch.append(contentsOf: scratch)
        }
        fromArrays.append(contentsOf: allocating.flush())
        fromScratch.append(contentsOf: reusing.flush())

        #expect(fromScratch == fromArrays)
        #expect(!fromScratch.isEmpty)
    }

    /// Stop-on-silence is the only reader of the per-block level, so a take
    /// opened without it must not be paying for the measurement. `nil` rather
    /// than zero: zero is what a muted microphone reads, and the poll would take
    /// it for silence.
    @Test func testTheBlockLevelIsOnlyMeasuredWhenSomebodyIsReadingIt() {
        let loud = [Float](repeating: 0.5, count: 1_024)

        let quiet = CaptureBuffer(inputRate: 16_000, measuresLevel: false)
        quiet.write(mono: loud)
        #expect(quiet.level == nil)

        let measuring = CaptureBuffer(inputRate: 16_000, measuresLevel: true)
        measuring.write(mono: loud)
        #expect(measuring.level == SilenceGate.speechLevel(loud))

        // Whether the level was taken or not, the audio itself is identical.
        #expect(quiet.finish().samples == measuring.finish().samples)
    }

    /// The mix is the desktop's: every channel averaged, never channel 0 alone.
    /// One channel takes the copy path, so both have to be checked.
    @Test func testTheMonoMixAveragesEveryChannel() {
        let left: [Float] = [1, 1, 1, 1]
        let right: [Float] = [0, 0.5, -1, 0]

        let stereo = CaptureBuffer(inputRate: 16_000, measuresLevel: false)
        mix(into: stereo, channels: [left, right])
        #expect(stereo.finish().samples == [0.5, 0.75, 0, 0.5])

        let mono = CaptureBuffer(inputRate: 16_000, measuresLevel: false)
        mix(into: mono, channels: [right])
        #expect(mono.finish().samples == right)
    }

    /// Feeds `writeMixedMono` the way AVAudioEngine does: one pointer per
    /// channel, all of the same length.
    private func mix(into buffer: CaptureBuffer, channels: [[Float]]) {
        let frames = channels[0].count
        let storage = channels.map { _ in UnsafeMutablePointer<Float>.allocate(capacity: frames) }
        defer { for pointer in storage { pointer.deallocate() } }
        for (index, channel) in channels.enumerated() {
            channel.withUnsafeBufferPointer { storage[index].update(from: $0.baseAddress!, count: frames) }
        }
        storage.withUnsafeBufferPointer { pointers in
            buffer.writeMixedMono(
                channels: pointers.baseAddress!,
                channelCount: channels.count,
                frames: frames
            )
        }
    }

    @Test func testTheCeilingNoticeMatchesTheCaptureLimit() {
        let buffer = CaptureBuffer(inputRate: 16_000, capacity: 4)
        buffer.write(mono: [1, 2, 3, 4, 5, 6])
        #expect(buffer.finish().samples == [1, 2, 3, 4])
        #expect(CaptureRingBuffer.ceilingNotice.contains("Everything recorded up to that point was kept"))
    }
    @Test func testTheCeilingNoticeNamesNoDuration() {
        #expect(
            CaptureRingBuffer.ceilingNotice.first(where: { $0.isNumber }) == nil,
            "a duration here goes stale the moment maxSeconds moves: \(CaptureRingBuffer.ceilingNotice)"
        )
    }
}
