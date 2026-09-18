import Foundation
import Testing
@testable import OpenFlowMobileCore

/// The two screens that quote a cost used to have those numbers typed into them,
/// so the tests here are about where the numbers come from rather than what they
/// currently are.
@Suite struct EngineProfileTests {

    /// The download figure is the pin's own `expectedBytes`, not a second copy of
    /// it. Pinning a new artefact has to move both screens, and the only way to
    /// hold that is to compare against the pin itself.
    @Test func testDownloadSizeComesFromThePin() {
        for engine in EngineChoice.allCases {
            let profile = EngineProfile.profile(for: engine)
            #expect(profile.downloadBytes == ModelDownloader.pin(for: engine).expectedBytes)
            #expect(profile.displayName == engine.displayName)
        }
    }

    /// The resident figure is the weights plus the working set, so it moves with
    /// the pin as well, and it is never smaller than the download: a model that
    /// claimed to need less memory than it takes on disk would be describing
    /// something other than loading it.
    @Test func testResidentIsTheWeightsPlusTheWorkingSet() {
        for engine in EngineChoice.allCases {
            let profile = EngineProfile.profile(for: engine)
            #expect(profile.residentBytes == profile.downloadBytes + EngineProfile.activationHeadroom)
            #expect(profile.residentBytes > profile.downloadBytes)
        }
    }

    /// The bug the profile exists to stop: the lighter engine reading the
    /// heavier engine's numbers. Whisper's pin is the smaller one today, so
    /// selecting it has to lower both figures rather than leaving the copy
    /// quoting Qwen.
    @Test func testALighterEngineNeverQuotesTheHeavierOnesCost() {
        let qwen = EngineProfile.profile(for: .qwen06)
        let whisper = EngineProfile.profile(for: .whisper)
        #expect(whisper.downloadBytes < qwen.downloadBytes)
        #expect(whisper.residentBytes < qwen.residentBytes)
        #expect(whisper.downloadDescription != qwen.downloadDescription)
        #expect(whisper.residentDescription != qwen.residentDescription)
    }

    /// The strings the two screens show. Hedged, because the resident number is
    /// an estimate, and in the same decimal units the download screen's progress
    /// line already uses.
    @Test func testTheDescriptionsAreHedgedAndInDecimalUnits() {
        let qwen = EngineProfile.profile(for: .qwen06)
        #expect(qwen.downloadDescription.hasPrefix("about "))
        #expect(qwen.residentDescription.hasPrefix("about "))
        #expect(qwen.downloadDescription.contains("700"), "700 MB decimal, not 667 MB binary")
        #expect(qwen.residentDescription.contains("1 GB"))
    }
}
