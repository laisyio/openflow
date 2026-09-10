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
/// The download figure is not a second opinion: it is `ModelDownloader.Pin`'s
/// own `expectedBytes`, which is the number the downloader checks the transfer
/// against. Change the pin and both screens follow.
public struct EngineProfile: Sendable, Equatable {
    public let engine: EngineChoice

    /// The recogniser's name as the user sees it, so a profile is enough to
    /// label a row without also reaching for the enum.
    public var displayName: String { engine.displayName }

    /// The one-time transfer, from the engine's pin.
    public let downloadBytes: Int64

    /// Bytes resident while the model is loaded. An estimate, and every screen
    /// that shows it says "about".
    public let residentBytes: Int64

    /// What the runtime adds on top of the weights: the working buffers, the
    /// decoder state and the scratch a forward pass needs.
    ///
    /// A flat figure rather than a per-engine one because the weights are what
    /// actually differ between the two candidates, and a made-up second number
    /// per engine would look more precise than it is. It is deliberately not a
    /// multiplier: doubling a heavier pin would overstate the difference, and
    /// the working set is roughly the same size whichever of these two runs.
    public static let activationHeadroom: Int64 = 300 * 1_000 * 1_000

    public init(engine: EngineChoice, downloadBytes: Int64, residentBytes: Int64) {
        self.engine = engine
        self.downloadBytes = downloadBytes
        self.residentBytes = residentBytes
    }

    /// The profile for a recogniser, derived from its pin.
    public static func profile(for engine: EngineChoice) -> EngineProfile {
        let downloadBytes = ModelDownloader.pin(for: engine).expectedBytes
        return EngineProfile(
            engine: engine,
            downloadBytes: downloadBytes,
            residentBytes: downloadBytes + activationHeadroom
        )
    }

    /// "about 700 MB", in the user's units.
    public var downloadDescription: String { Self.approximate(downloadBytes) }

    /// "about 1 GB", in the user's units.
    public var residentDescription: String { Self.approximate(residentBytes) }

    /// Decimal units, matching the download screen's progress line and the way a
    /// model card quotes a size. The hedge is part of the string because the
    /// resident figure is an estimate and the download figure is what the server
    /// said last time somebody pinned it.
    private static func approximate(_ bytes: Int64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        formatter.allowedUnits = [.useMB, .useGB]
        return "about " + formatter.string(fromByteCount: bytes)
    }
}
