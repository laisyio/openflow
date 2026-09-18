import Foundation

/// What one recogniser costs, in the two numbers the app promises to be honest
/// about: what it downloads and what it holds in memory while it runs.
///
/// It exists because those numbers were typed into two screens by hand. The
/// download screen said "700 MB" and the Settings footer said "about 1 GB", and
/// neither knew which engine was selected, so picking the lighter one still read
/// the heavier claim, and pinning a new artefact would have left both sentences
/// quietly wrong.
///
/// The download figure is not a second opinion: it is the engine's
/// `ModelDownloader.ModelPin` summed over its files, which is the number the
/// downloader checks the transfer against. Change the pin and both screens
/// follow.
public struct EngineProfile: Sendable, Equatable {
    public let engine: EngineChoice

    /// The recogniser's name as the user sees it, so a profile is enough to
    /// label a row without also reaching for the enum.
    public var displayName: String { engine.displayName }

    /// The one-time transfer, from the engine's pin.
    public let downloadBytes: Int64

    /// Bytes resident while the model is loaded. An **estimate** until the M2
    /// gate on a phone replaces it with a measured number, and every screen that
    /// shows it says "about".
    ///
    /// The measured number already exists for one machine: `M2-MOONSHINE.md`
    /// records 562 MB for base-en and 246 MB for tiny-en as whole-process RSS on
    /// an M4, which is a Mac running a benchmark harness, not an iPhone running
    /// this app. `MoonshineSpeechEngine.residentBytes` reports what the phone
    /// actually pays, as a footprint delta; this is what the download screen can
    /// say before anything has been loaded.
    public let residentBytes: Int64

    /// What the whole thing costs in memory, as a multiple of the weights.
    ///
    /// Three, from the two ratios that were measured: 562 MB resident against
    /// 141 MB of weights is 4.0, and 246 MB against 44 MB is 5.6, both on a Mac
    /// whose process includes a Python harness the phone does not run. Three is
    /// under both and above the weights themselves, and it scales with the pin,
    /// which a flat allowance would not: the two engines here differ by a factor
    /// of three in weights, and quoting the same headroom for both would tell
    /// someone choosing tiny-en that they had saved almost nothing.
    ///
    /// It is a placeholder for a measurement, and the moment the phone gate runs
    /// it should be replaced by the numbers that gate produces.
    public static let residentMultiplier: Int64 = 3

    public init(engine: EngineChoice, downloadBytes: Int64, residentBytes: Int64, fileCount: Int) {
        self.engine = engine
        self.downloadBytes = downloadBytes
        self.residentBytes = residentBytes
        self.fileCount = fileCount
    }

    /// How many files the download screen is about to fetch, so the copy can say
    /// "three files" without counting them by hand in a view.
    public let fileCount: Int

    /// The profile for a recogniser, derived from its pin.
    public static func profile(for engine: EngineChoice) -> EngineProfile {
        let pin = ModelDownloader.pin(for: engine)
        let downloadBytes = pin.expectedBytes
        return EngineProfile(
            engine: engine,
            downloadBytes: downloadBytes,
            residentBytes: downloadBytes * residentMultiplier,
            fileCount: pin.files.count
        )
    }

    /// "about 141 MB", decimal, English.
    public var downloadDescription: String { Self.approximate(downloadBytes) }

    /// "about 423 MB", decimal, English.
    public var residentDescription: String { Self.approximate(residentBytes) }

    /// Decimal units, matching the download screen's progress line and the way a
    /// model card quotes a size. The hedge is part of the string because the
    /// resident figure is an estimate and the download figure is what the files
    /// measured when somebody pinned them.
    ///
    /// Formatted by hand rather than through `ByteCountFormatter`: the hedge is
    /// English, so the unit is too, and a locale-formatted "1 Go" after an
    /// English "about" would be the worse of the two. It also keeps the strings
    /// the same on every machine that runs the tests.
    private static func approximate(_ bytes: Int64) -> String {
        let megabytes = Double(bytes) / 1_000_000
        guard megabytes >= 1_000 else {
            return "about \(Int(megabytes.rounded())) MB"
        }
        let gigabytes = (megabytes / 100).rounded() / 10
        let whole = gigabytes.rounded()
        let text = gigabytes == whole ? String(Int(whole)) : String(gigabytes)
        return "about \(text) GB"
    }
}
