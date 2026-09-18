import Foundation
import Testing
@testable import OpenFlowMobileCore

/// The two screens that quote a cost used to have those numbers typed into them,
/// so the tests here are about where the numbers come from rather than what they
/// currently are. The exceptions are the strings themselves, which are what the
/// user reads and are therefore worth pinning.
@Suite struct EngineProfileTests {

    /// The download figure is the pin's own byte count summed over its files, not
    /// a second copy of it. Pinning a new artefact has to move both screens, and
    /// the only way to hold that is to compare against the pin itself.
    @Test func testDownloadSizeComesFromThePin() {
        for engine in EngineChoice.allCases {
            let pin = ModelDownloader.pin(for: engine)
            let profile = EngineProfile.profile(for: engine)
            #expect(profile.downloadBytes == pin.expectedBytes)
            #expect(profile.downloadBytes == pin.files.reduce(0) { $0 + $1.expectedBytes })
            #expect(profile.fileCount == pin.files.count)
            #expect(profile.displayName == engine.displayName)
        }
    }

    /// A model is three files, and the download screen says so. Counting them in
    /// the view is how the copy drifts from the pin.
    @Test func testTheFileCountIsThreeAndComesFromThePin() {
        for engine in EngineChoice.allCases {
            #expect(EngineProfile.profile(for: engine).fileCount == 3)
        }
    }

    /// The resident figure moves with the pin as well, and it is never smaller
    /// than the download: a model that claimed to need less memory than it takes
    /// on disk would be describing something other than loading it.
    @Test func testResidentScalesWithTheWeights() {
        for engine in EngineChoice.allCases {
            let profile = EngineProfile.profile(for: engine)
            #expect(profile.residentBytes == profile.downloadBytes * EngineProfile.residentMultiplier)
            #expect(profile.residentBytes > profile.downloadBytes)
        }
    }

    /// The bug the profile exists to stop: the lighter engine reading the
    /// heavier engine's numbers. Selecting tiny-en has to lower both figures
    /// rather than leaving the copy quoting base-en.
    @Test func testALighterEngineNeverQuotesTheHeavierOnesCost() {
        let base = EngineProfile.profile(for: .moonshineBase)
        let tiny = EngineProfile.profile(for: .moonshineTiny)
        #expect(tiny.downloadBytes < base.downloadBytes)
        #expect(tiny.residentBytes < base.residentBytes)
        #expect(tiny.downloadDescription != base.downloadDescription)
        #expect(tiny.residentDescription != base.residentDescription)

        // A multiplier rather than a flat allowance, so the saving survives into
        // the memory figure. A flat 300 MB headroom would have told someone
        // choosing tiny-en that they had saved 3 percent of the memory when they
        // had saved two thirds of it.
        #expect(Double(tiny.residentBytes) < Double(base.residentBytes) * 0.5)
    }

    /// The strings the two screens show. Hedged, because the resident number is
    /// an estimate, and in the same decimal units the download screen's progress
    /// line already uses. `M2-MOONSHINE.md` quotes 141 MB and 44 MB.
    @Test func testTheDescriptionsAreHedgedAndInDecimalUnits() {
        let base = EngineProfile.profile(for: .moonshineBase)
        #expect(base.downloadDescription == "about 141 MB")
        #expect(base.residentDescription == "about 423 MB")

        let tiny = EngineProfile.profile(for: .moonshineTiny)
        #expect(tiny.downloadDescription == "about 44 MB")
        #expect(tiny.residentDescription == "about 132 MB")

        for profile in [base, tiny] {
            #expect(profile.downloadDescription.hasPrefix("about "))
            #expect(profile.residentDescription.hasPrefix("about "))
            #expect(!profile.residentDescription.contains("GB"), "neither engine is a gigabyte any more")
        }
    }

    /// The formatter still reaches gigabytes, for whatever gets pinned next.
    /// Removing the two heavy engines removed the only callers that exercised
    /// that branch, and an untested branch in a string a user reads is how "1 Go"
    /// ships.
    @Test func testTheFormatterStillHandlesGigabyteSizedPins() {
        let heavy = EngineProfile(
            engine: .moonshineBase,
            downloadBytes: 700_000_000,
            residentBytes: 2_500_000_000,
            fileCount: 1
        )
        #expect(heavy.downloadDescription == "about 700 MB")
        #expect(heavy.residentDescription == "about 2.5 GB")

        let round = EngineProfile(
            engine: .moonshineBase,
            downloadBytes: 1,
            residentBytes: 1_000_000_000,
            fileCount: 1
        )
        #expect(round.residentDescription == "about 1 GB", "a whole number loses its decimal point")
    }
}
