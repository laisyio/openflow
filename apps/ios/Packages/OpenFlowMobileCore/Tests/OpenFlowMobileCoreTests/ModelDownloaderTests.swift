import Foundation
import Testing
@testable import OpenFlowMobileCore

/// A Moonshine model is three files that are only a model together
/// (`M2-MOONSHINE.md`, "Weights and pins"), so what is tested here is the set,
/// not the file: that all of it verifies before any of it is installed, and that
/// a set which fails part way leaves nothing behind for the next launch to
/// mistake for an install.
///
/// No test in this suite touches the network. The downloader's seam is the
/// `URLSession` it is given and the URLs its pins carry, and `URLSession`
/// downloads a `file://` URL exactly as it downloads an `https://` one, right
/// down to handing back a temporary file. So the fixtures are written to disk
/// and pinned by their real digests, and the code under test is the same code
/// that runs against Moonshine's CDN.
@Suite struct ModelDownloaderTests {

    // MARK: - Fixtures

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("openflow-downloader-tests")
            .appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    /// Files on disk and a pin that describes them truthfully.
    ///
    /// An array of pairs rather than a dictionary, because the order the set is
    /// fetched in is part of what these tests are about: the interesting failure
    /// is the second file of three, when the first has already verified.
    private func makeServedSet(
        subdirectory: String,
        contents: [(name: String, body: String)]
    ) throws -> (pin: ModelDownloader.ModelPin, served: URL) {
        let served = temporaryDirectory()
        var files: [ModelDownloader.Pin] = []
        for (name, text) in contents {
            let body = Data(text.utf8)
            let url = served.appendingPathComponent(name)
            try body.write(to: url)
            files.append(
                ModelDownloader.Pin(
                    fileName: name,
                    remote: url,
                    sha256: try ModelStore.sha256Hex(ofFileAt: url),
                    expectedBytes: Int64(body.count)
                )
            )
        }
        return (ModelDownloader.ModelPin(subdirectory: subdirectory, files: files), served)
    }

    private func run(
        _ downloader: ModelDownloader,
        _ pin: ModelDownloader.ModelPin
    ) async -> (progress: [ModelDownloader.Progress], error: Error?) {
        var seen: [ModelDownloader.Progress] = []
        do {
            for try await step in await downloader.download(pin: pin) { seen.append(step) }
            return (seen, nil)
        } catch {
            return (seen, error)
        }
    }

    // MARK: - The pins themselves

    /// The six digests and the six sizes, written out again rather than compared
    /// against the constants that produced them.
    ///
    /// A test that read `moonshineBasePin.files[0].sha256` and checked it equals
    /// itself would pass through any edit at all. These literals were taken with
    /// `shasum -a 256` against the files the desktop benchmark downloaded and
    /// they are the whole reason the app can claim it runs the weights it meant
    /// to run, so changing one has to be a deliberate act that shows up in this
    /// file as well as in the source.
    @Test func testTheShippingPinsAreTheMeasuredOnes() {
        let base = ModelDownloader.moonshineBasePin
        #expect(base.subdirectory == "moonshine/base-en")
        #expect(base.files.map(\.fileName) == ["encoder_model.ort", "decoder_model_merged.ort", "tokenizer.bin"])
        #expect(base.files.map(\.expectedBytes) == [31_326_816, 109_424_400, 249_974])
        #expect(base.files.map(\.sha256) == [
            "7c66495948d0d08ec1af454cd4b5514862ae6511e94712a60e6d83eaec8dc8cf",
            "d9d7b333af34bc552580576ddcf248a1c6c839e0d3b43b09afb9376ed009899d",
            "6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d",
        ])
        #expect(base.expectedBytes == 141_001_190)

        let tiny = ModelDownloader.moonshineTinyPin
        #expect(tiny.subdirectory == "moonshine/tiny-en")
        #expect(tiny.files.map(\.fileName) == ["encoder_model.ort", "decoder_model_merged.ort", "tokenizer.bin"])
        #expect(tiny.files.map(\.expectedBytes) == [13_281_600, 30_412_256, 249_974])
        #expect(tiny.files.map(\.sha256) == [
            "94e90a4654fc45cdfedb77c4c08e1739f48862998e58fada384b25118134f221",
            "cf524c4862d36e9e5ab032eddc73637efd822d70e868ac575cf1a46e1e4708a0",
            "6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d",
        ])
        #expect(tiny.expectedBytes == 43_943_830)

        // The tokenizer really is the same file in both models. It is still
        // pinned and downloaded twice, into two directories, so removing one
        // engine cannot break the other.
        #expect(base.files[2].sha256 == tiny.files[2].sha256)
        #expect(base.files[2].remote != tiny.files[2].remote)
    }

    /// One host, over TLS, on the path the spec names. The app's offline claim is
    /// only as good as the list of hosts it can reach.
    @Test func testEveryPinnedURLIsTheMoonshineCDNOverTLS() {
        for pin in [ModelDownloader.moonshineBasePin, ModelDownloader.moonshineTinyPin] {
            for file in pin.files {
                #expect(file.remote.scheme == "https")
                #expect(file.remote.host == "download.moonshine.ai")
                #expect(file.remote.lastPathComponent == file.fileName)
            }
        }
        #expect(ModelDownloader.moonshineBasePin.files[0].remote.absoluteString
            == "https://download.moonshine.ai/model/base-en/quantized/base-en/encoder_model.ort")
        #expect(ModelDownloader.moonshineTinyPin.files[0].remote.absoluteString
            == "https://download.moonshine.ai/model/tiny-en/quantized/tiny-en/encoder_model.ort")
    }

    @Test func testEveryEngineHasAPinAndNoneOfThemIsAPlaceholder() {
        for engine in EngineChoice.allCases {
            let pin = ModelDownloader.pin(for: engine)
            #expect(pin.files.count == 3)
            for file in pin.files {
                #expect(file.sha256 != ModelDownloader.placeholderDigest)
                #expect(file.sha256.count == 64)
                #expect(file.expectedBytes > 0)
            }
        }
    }

    // MARK: - Installing a set

    @Test func testAVerifiedSetInstallsEveryFile() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "encoder weights"),
                (name: "decoder_model_merged.ort", body: "decoder weights"),
                (name: "tokenizer.bin", body: "tokenizer"),
            ]
        )
        let downloader = ModelDownloader(store: store)

        let (progress, error) = await run(downloader, pin)
        #expect(error == nil)

        let installed = store.subdirectory(pin.subdirectory)
        for file in pin.files {
            #expect(installed.exists(file.fileName), "\(file.fileName) must be installed")
            try installed.verify(file.fileName, sha256Hex: file.sha256)
        }
        #expect(await downloader.isInstalled(pin: pin))
        #expect(progress.last == .finished(installed.directory))
        #expect(downloader.directory(for: pin) == installed.directory)

        // Nothing is left in the staging directory the install moved across.
        let partial = store.subdirectory(pin.subdirectory + ".partial")
        #expect(!FileManager.default.fileExists(atPath: partial.directory.path))
    }

    /// The bar rises once across the set, not once per file. Progress is reported
    /// against the sum, so `received` never goes backwards and it ends at the
    /// whole transfer.
    @Test func testProgressCountsTheWholeSetAndNeverGoesBackwards() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: String(repeating: "e", count: 4_000)),
                (name: "decoder_model_merged.ort", body: String(repeating: "d", count: 9_000)),
                (name: "tokenizer.bin", body: String(repeating: "t", count: 500)),
            ]
        )
        let (progress, error) = await run(ModelDownloader(store: store), pin)
        #expect(error == nil)

        var high: Int64 = -1
        var last: Int64 = 0
        for step in progress {
            guard case let .downloading(received, expected) = step else { continue }
            #expect(expected == pin.expectedBytes, "the bar is drawn against the set, not one file")
            #expect(received >= high, "progress must not restart between files")
            high = received
            last = received
        }
        #expect(last == pin.expectedBytes)
        #expect(pin.expectedBytes == 13_500)
    }

    /// The mutation this exists for: change one digit of one digest and the
    /// install has to refuse, and refuse the whole set, not just that file.
    @Test func testOneBadDigestRefusesTheWholeSetAndLeavesNothingBehind() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (honest, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "encoder weights"),
                (name: "decoder_model_merged.ort", body: "decoder weights"),
                (name: "tokenizer.bin", body: "tokenizer"),
            ]
        )
        // The decoder is the second of three, so the first file has already
        // downloaded and verified when this one fails. That is the case worth
        // testing: a set that fails part way through.
        var files = honest.files
        let bad = files[1]
        files[1] = ModelDownloader.Pin(
            fileName: bad.fileName,
            remote: bad.remote,
            sha256: String(repeating: "a", count: 64),
            expectedBytes: bad.expectedBytes
        )
        let pin = ModelDownloader.ModelPin(subdirectory: honest.subdirectory, files: files)

        let downloader = ModelDownloader(store: store)
        let (_, error) = await run(downloader, pin)

        guard case let .checksumMismatch(file, expected, actual)? = error as? ModelDownloader.DownloadError else {
            Issue.record("expected a checksum mismatch, got \(String(describing: error))")
            return
        }
        #expect(file == "decoder_model_merged.ort")
        #expect(expected == String(repeating: "a", count: 64))
        #expect(actual != expected)

        // The encoder verified. It must still be gone: two of three files is not
        // a model, and leaving it would let a later `isInstalled` walk into a
        // directory that cannot be opened.
        let target = store.subdirectory(pin.subdirectory)
        #expect(!FileManager.default.fileExists(atPath: target.directory.path), "a partial set must be removed")
        #expect(!target.exists("encoder_model.ort"))
        #expect(await downloader.isInstalled(pin: pin) == false)
        let partial = store.subdirectory(pin.subdirectory + ".partial")
        #expect(!FileManager.default.fileExists(atPath: partial.directory.path))
    }

    /// A failed re-download must not cost somebody their working recogniser.
    ///
    /// The case: base-en is installed and verified, a newer pin is offered, and
    /// the second of its three files fails. Every failure path used to remove
    /// the installed directory along with the staging one, so a flaky network on
    /// an upgrade left the user unable to dictate at all, by an operation they
    /// only started because they were told there was something newer. They are
    /// worse off than if they had ignored it.
    @Test func testAFailedReDownloadLeavesTheWorkingInstallAlone() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let downloader = ModelDownloader(store: store)

        let (installed, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v1 encoder"),
                (name: "decoder_model_merged.ort", body: "v1 decoder"),
                (name: "tokenizer.bin", body: "v1 tokenizer"),
            ]
        )
        #expect(await run(downloader, installed).error == nil)
        #expect(await downloader.isInstalled(pin: installed))

        // The upgrade. Its second file will not verify.
        let (honest, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v2 encoder"),
                (name: "decoder_model_merged.ort", body: "v2 decoder"),
                (name: "tokenizer.bin", body: "v2 tokenizer"),
            ]
        )
        var files = honest.files
        files[1] = ModelDownloader.Pin(
            fileName: files[1].fileName,
            remote: files[1].remote,
            sha256: String(repeating: "b", count: 64),
            expectedBytes: files[1].expectedBytes
        )
        let upgrade = ModelDownloader.ModelPin(subdirectory: honest.subdirectory, files: files)

        let (_, error) = await run(downloader, upgrade)
        #expect(error != nil, "the upgrade must fail")

        // The recogniser the user had is still the recogniser the user has.
        let target = store.subdirectory(installed.subdirectory)
        #expect(await downloader.isInstalled(pin: installed), "the working install must survive")
        #expect(await downloader.isPresent(pin: installed))
        #expect(try Data(contentsOf: target.url(for: "encoder_model.ort")) == Data("v1 encoder".utf8))
        #expect(try Data(contentsOf: target.url(for: "decoder_model_merged.ort")) == Data("v1 decoder".utf8))
        #expect(try Data(contentsOf: target.url(for: "tokenizer.bin")) == Data("v1 tokenizer".utf8))

        // And no half-written upgrade is left lying about.
        let partial = store.subdirectory(installed.subdirectory + ".partial")
        #expect(!FileManager.default.fileExists(atPath: partial.directory.path))
    }

    /// A successful re-download still replaces the install, so the rule above is
    /// "do not destroy on failure", not "never replace".
    @Test func testASucceedingReDownloadStillReplacesTheInstall() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let downloader = ModelDownloader(store: store)
        let (first, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v1 encoder"),
                (name: "decoder_model_merged.ort", body: "v1 decoder"),
                (name: "tokenizer.bin", body: "v1 tokenizer"),
            ]
        )
        #expect(await run(downloader, first).error == nil)

        let (second, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v2 encoder"),
                (name: "decoder_model_merged.ort", body: "v2 decoder"),
                (name: "tokenizer.bin", body: "v2 tokenizer"),
            ]
        )
        #expect(await run(downloader, second).error == nil)
        let target = store.subdirectory(second.subdirectory)
        #expect(try Data(contentsOf: target.url(for: "encoder_model.ort")) == Data("v2 encoder".utf8))
        #expect(await downloader.isInstalled(pin: second))
        #expect(await downloader.isInstalled(pin: first) == false, "the old digests no longer match")
    }

    /// `isPresent` is the cheap question the download screen should ask: three
    /// directory entries rather than 141 MB of hashing.
    @Test func testIsPresentChecksNamesAndSizesWithoutHashing() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let downloader = ModelDownloader(store: store)
        let (pin, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "encoder weights"),
                (name: "decoder_model_merged.ort", body: "decoder weights"),
                (name: "tokenizer.bin", body: "tokenizer"),
            ]
        )
        #expect(await downloader.isPresent(pin: pin) == false, "nothing is installed yet")
        #expect(await run(downloader, pin).error == nil)
        #expect(await downloader.isPresent(pin: pin))

        let installed = store.subdirectory(pin.subdirectory)

        // A truncated file changes size, so the cheap check catches it too.
        try Data("short".utf8).write(to: installed.url(for: "tokenizer.bin"))
        #expect(await downloader.isPresent(pin: pin) == false)
        #expect(await downloader.isInstalled(pin: pin) == false)

        // A file corrupted in place at the same length is exactly what it cannot
        // catch, which is why it is not what gates loading. Stated here so the
        // difference between the two is a test rather than a claim in a comment.
        let original = try Data(contentsOf: installed.url(for: "encoder_model.ort"))
        try Data(repeating: 0x41, count: original.count).write(to: installed.url(for: "encoder_model.ort"))
        try Data("tokenizer".utf8).write(to: installed.url(for: "tokenizer.bin"))
        #expect(await downloader.isPresent(pin: pin), "same names, same sizes")
        #expect(await downloader.isInstalled(pin: pin) == false, "different bytes")

        // A missing file fails the cheap check.
        try installed.remove("encoder_model.ort")
        #expect(await downloader.isPresent(pin: pin) == false)
    }

    /// A file that is not there at all fails the same way a bad digest does, and
    /// takes the set with it.
    @Test func testAMissingFileTakesTheSetWithIt() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (honest, served) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "encoder weights"),
                (name: "decoder_model_merged.ort", body: "decoder weights"),
                (name: "tokenizer.bin", body: "tokenizer"),
            ]
        )
        try FileManager.default.removeItem(at: served.appendingPathComponent("tokenizer.bin"))

        let downloader = ModelDownloader(store: store)
        let (_, error) = await run(downloader, honest)
        #expect(error != nil)
        #expect(!FileManager.default.fileExists(atPath: store.subdirectory(honest.subdirectory).directory.path))
        #expect(await downloader.isInstalled(pin: honest) == false)
    }

    /// `isInstalled` hashes what is on disk. A file truncated after install --
    /// a full disk, a crash mid-write -- exists and is the wrong file, and the
    /// download screen has to see that before the engine does.
    @Test func testATamperedFileMakesTheSetNotInstalled() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "encoder weights"),
                (name: "decoder_model_merged.ort", body: "decoder weights"),
                (name: "tokenizer.bin", body: "tokenizer"),
            ]
        )
        let downloader = ModelDownloader(store: store)
        let (_, error) = await run(downloader, pin)
        #expect(error == nil)
        #expect(await downloader.isInstalled(pin: pin))

        let installed = store.subdirectory(pin.subdirectory)
        try Data("truncated".utf8).write(to: installed.url(for: "tokenizer.bin"))
        #expect(await downloader.isInstalled(pin: pin) == false)
    }

    /// base-en and tiny-en name their three files identically. Without a
    /// per-engine subdirectory, installing one would overwrite the other with
    /// files that pass no checksum.
    @Test func testTheTwoEnginesInstallSideBySide() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (basePin, _) = try makeServedSet(
            subdirectory: ModelDownloader.moonshineBasePin.subdirectory,
            contents: [
                (name: "encoder_model.ort", body: "base encoder"),
                (name: "decoder_model_merged.ort", body: "base decoder"),
                (name: "tokenizer.bin", body: "shared tokenizer"),
            ]
        )
        let (tinyPin, _) = try makeServedSet(
            subdirectory: ModelDownloader.moonshineTinyPin.subdirectory,
            contents: [
                (name: "encoder_model.ort", body: "tiny encoder"),
                (name: "decoder_model_merged.ort", body: "tiny decoder"),
                (name: "tokenizer.bin", body: "shared tokenizer"),
            ]
        )
        let downloader = ModelDownloader(store: store)
        #expect(await run(downloader, basePin).error == nil)
        #expect(await run(downloader, tinyPin).error == nil)

        #expect(await downloader.isInstalled(pin: basePin))
        #expect(await downloader.isInstalled(pin: tinyPin))
        let baseEncoder = store.subdirectory(basePin.subdirectory).url(for: "encoder_model.ort")
        let tinyEncoder = store.subdirectory(tinyPin.subdirectory).url(for: "encoder_model.ort")
        #expect(try Data(contentsOf: baseEncoder) == Data("base encoder".utf8))
        #expect(try Data(contentsOf: tinyEncoder) == Data("tiny encoder".utf8))
    }

    /// Re-downloading over an install replaces it rather than merging into it.
    @Test func testASecondDownloadReplacesTheInstalledSet() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (first, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v1 encoder"),
                (name: "decoder_model_merged.ort", body: "v1 decoder"),
                (name: "tokenizer.bin", body: "v1 tokenizer"),
            ]
        )
        let downloader = ModelDownloader(store: store)
        #expect(await run(downloader, first).error == nil)

        // A leftover from an older release, sitting in the model directory.
        let installed = store.subdirectory(first.subdirectory)
        try Data("stale".utf8).write(to: installed.url(for: "leftover.ort"))

        let (second, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [
                (name: "encoder_model.ort", body: "v2 encoder"),
                (name: "decoder_model_merged.ort", body: "v2 decoder"),
                (name: "tokenizer.bin", body: "v2 tokenizer"),
            ]
        )
        #expect(await run(downloader, second).error == nil)
        #expect(try Data(contentsOf: installed.url(for: "encoder_model.ort")) == Data("v2 encoder".utf8))
        #expect(!installed.exists("leftover.ort"), "the directory is replaced, not merged into")
    }

    /// The placeholder machinery stays for the next engine that is pinned before
    /// its artefact is published. A pin still wearing it refuses to start.
    @Test func testAPlaceholderDigestRefusesToDownload() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (honest, _) = try makeServedSet(
            subdirectory: "moonshine/fixture-en",
            contents: [(name: "encoder_model.ort", body: "encoder"), (name: "tokenizer.bin", body: "tokenizer")]
        )
        var files = honest.files
        files[0] = ModelDownloader.Pin(
            fileName: files[0].fileName,
            remote: files[0].remote,
            sha256: ModelDownloader.placeholderDigest,
            expectedBytes: files[0].expectedBytes
        )
        let pin = ModelDownloader.ModelPin(subdirectory: honest.subdirectory, files: files)

        let (progress, error) = await run(ModelDownloader(store: store), pin)
        #expect(progress.isEmpty, "a placeholder pin must not start a download")
        #expect(error as? ModelDownloader.DownloadError == .placeholderPin)
        #expect(!FileManager.default.fileExists(atPath: store.subdirectory(pin.subdirectory).directory.path))
    }

    @Test func resumeReusesVerifiedFilesWithoutRequestingThemAgain() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, served) = try makeServedSet(subdirectory: "moonshine/resume", contents: [
            (name: "encoder", body: "verified encoder"), (name: "decoder", body: "decoder")
        ])
        try FileManager.default.removeItem(at: served.appendingPathComponent("decoder"))
        let downloader = ModelDownloader(store: store)
        #expect(await run(downloader, pin).error != nil)
        #expect(await downloader.hasResumeData(pin: pin))
        try FileManager.default.removeItem(at: served.appendingPathComponent("encoder"))
        try Data("decoder".utf8).write(to: served.appendingPathComponent("decoder"))
        #expect(await run(downloader, pin).error == nil, "the encoder is now available only in the resume cache")
        #expect(await downloader.isInstalled(pin: pin))
        #expect(!(await downloader.hasResumeData(pin: pin)))
    }

    @Test func concurrentDownloadIsRejectedAndCancellationReleasesAdmission() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, _) = try makeServedSet(subdirectory: "moonshine/concurrent", contents: [(name: "weights", body: "weights")])
        let url = URL(string: "https://download-fixture.invalid/\(UUID().uuidString)")!
        let suspended = ModelDownloader.ModelPin(subdirectory: pin.subdirectory, files: [
            ModelDownloader.Pin(fileName: "weights", remote: url, sha256: pin.files[0].sha256, expectedBytes: 7)
        ])
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [SuspendedDownloadProtocol.self]
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }
        let first = ModelDownloader(store: store, session: session)
        let running = Task { await run(first, suspended) }
        await waitUntil("download requested") { SuspendedDownloadProtocol.count(url, stopped: false) > 0 }
        #expect(await run(first, pin).error as? ModelDownloader.DownloadError == .alreadyDownloading,
                "a duplicate attempt must not replace the first attempt's cancellation handle")
        let second = ModelDownloader(store: store)
        #expect(await run(second, pin).error as? ModelDownloader.DownloadError == .alreadyDownloading)
        await first.cancelAndWait()
        _ = await running.value
        await waitUntil("transport cancelled") { SuspendedDownloadProtocol.count(url, stopped: true) > 0 }
        #expect(await run(second, pin).error == nil)
        #expect(await second.isInstalled(pin: pin))
        let contents = try FileManager.default.contentsOfDirectory(atPath: store.subdirectory("moonshine").directory.path)
        #expect(!contents.contains { $0.contains(".partial.") })
    }

    @MainActor
    @Test func downloadControllerCancelsTransportAndReopenedScreenDetectsInstall() async throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        let (pin, _) = try makeServedSet(subdirectory: "moonshine/screen", contents: [(name: "weights", body: "weights")])
        let url = URL(string: "https://download-fixture.invalid/\(UUID().uuidString)")!
        let suspended = ModelDownloader.ModelPin(subdirectory: pin.subdirectory, files: [
            ModelDownloader.Pin(fileName: "weights", remote: url, sha256: pin.files[0].sha256, expectedBytes: 7)
        ])
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [SuspendedDownloadProtocol.self]
        let session = URLSession(configuration: configuration)
        defer { session.invalidateAndCancel() }
        let controller = ModelDownloadController(downloader: ModelDownloader(store: store, session: session), pin: suspended)
        controller.start()
        controller.start()
        await waitUntil("screen requested transfer") { SuspendedDownloadProtocol.count(url, stopped: false) > 0 }
        await controller.cancel()
        #expect(controller.phase == .idle)
        #expect(SuspendedDownloadProtocol.count(url, stopped: false) == 1)
        let installer = ModelDownloader(store: store)
        #expect(await run(installer, pin).error == nil)
        let reopened = ModelDownloadController(downloader: installer, pin: pin)
        await reopened.refresh()
        #expect(reopened.phase == .installed)
    }
}

private final class SuspendedDownloadProtocol: URLProtocol, @unchecked Sendable {
    private final class Counts: @unchecked Sendable {
        let lock = NSLock()
        var values: [String: Int] = [:]
    }
    private static let counts = Counts()
    static func count(_ url: URL, stopped: Bool) -> Int {
        counts.lock.withLock { counts.values[url.absoluteString + (stopped ? "stop" : "start"), default: 0] }
    }
    override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "download-fixture.invalid" }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        guard let url = request.url else { return }
        Self.counts.lock.withLock { Self.counts.values[url.absoluteString + "start", default: 0] += 1 }
    }
    override func stopLoading() {
        guard let url = request.url else { return }
        Self.counts.lock.withLock { Self.counts.values[url.absoluteString + "stop", default: 0] += 1 }
    }
}
