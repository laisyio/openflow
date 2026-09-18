import Foundation

/// The one-time model download, and the only file in the app that is allowed to
/// touch the network.
///
/// PLAN.md section 0 makes a promise -- "the only network request it ever makes
/// is the one-time model download" -- and section 6 turns it into a check a
/// reviewer can run. `apps/ios/README.md` has the exact grep and what it is
/// allowed to return: this file, its test, and the engine package's manifest,
/// whose URL is where the dependency comes from rather than a host the app
/// calls. Anything else in our code that wants to reach the network is a bug,
/// not a feature request.
///
/// A Moonshine model is three files, not one (`M2-MOONSHINE.md`), so the unit of
/// download is `ModelPin`: a set of `Pin`s that install together or not at all.
public actor ModelDownloader {

    /// One pinned file. Nothing here is discovered at runtime: no manifest
    /// fetch, no redirect chasing, no remote config. Change these and ship a new
    /// build, which is also what makes the App Store privacy answer honest.
    public struct Pin: Sendable, Equatable {
        public let fileName: String
        public let remote: URL
        public let sha256: String
        public let expectedBytes: Int64

        public init(fileName: String, remote: URL, sha256: String, expectedBytes: Int64) {
            self.fileName = fileName
            self.remote = remote
            self.sha256 = sha256
            self.expectedBytes = expectedBytes
        }
    }

    /// One engine's weights: the files, and the directory under the model store
    /// they live in.
    ///
    /// The set is the unit because a model is only a model when all of it is
    /// there. Two of base-en's three files verified is not two thirds of a
    /// recogniser, it is a directory that would make `Transcriber.init` fail at
    /// the worst moment, so `download` installs the set atomically and removes a
    /// partial one.
    public struct ModelPin: Sendable, Equatable {
        /// Where the set lives, relative to the model store, e.g.
        /// `moonshine/base-en`. Per engine, so base-en and tiny-en can both be
        /// installed even though their file names are identical.
        public let subdirectory: String
        public let files: [Pin]

        public init(subdirectory: String, files: [Pin]) {
            self.subdirectory = subdirectory
            self.files = files
        }

        /// The whole transfer. `EngineProfile` quotes this, and the progress bar
        /// is drawn against it, so the bar rises once across three files rather
        /// than resetting twice.
        public var expectedBytes: Int64 {
            files.reduce(0) { $0 + $1.expectedBytes }
        }
    }

    /// The digest a pin carries until a real one is measured. A pin still
    /// wearing it refuses to download rather than installing something the app
    /// cannot check. Nothing uses it today; it stays for the next engine that is
    /// pinned before its artefact is published.
    public static let placeholderDigest = String(repeating: "0", count: 64)

    /// Moonshine's CDN, the only host the app ever contacts.
    private static let base = "https://download.moonshine.ai/model"

    /// base-en: 141 MB across three files, sizes and digests measured on
    /// 2026-09-10 from the files the desktop benchmark downloaded
    /// (`M2-MOONSHINE.md`, "Weights and pins").
    public static let moonshineBasePin = ModelPin(
        subdirectory: "moonshine/base-en",
        files: [
            Pin(
                fileName: "encoder_model.ort",
                remote: URL(string: "\(base)/base-en/quantized/base-en/encoder_model.ort")!,
                sha256: "7c66495948d0d08ec1af454cd4b5514862ae6511e94712a60e6d83eaec8dc8cf",
                expectedBytes: 31_326_816
            ),
            Pin(
                fileName: "decoder_model_merged.ort",
                remote: URL(string: "\(base)/base-en/quantized/base-en/decoder_model_merged.ort")!,
                sha256: "d9d7b333af34bc552580576ddcf248a1c6c839e0d3b43b09afb9376ed009899d",
                expectedBytes: 109_424_400
            ),
            Pin(
                fileName: "tokenizer.bin",
                remote: URL(string: "\(base)/base-en/quantized/base-en/tokenizer.bin")!,
                sha256: "6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d",
                expectedBytes: 249_974
            ),
        ]
    )

    /// tiny-en: 44 MB across three files. The tokenizer is byte for byte the
    /// same file as base-en's, and it is still downloaded and verified into
    /// tiny-en's own directory: sharing it would tie the two installs together
    /// and make removing one engine able to break the other.
    public static let moonshineTinyPin = ModelPin(
        subdirectory: "moonshine/tiny-en",
        files: [
            Pin(
                fileName: "encoder_model.ort",
                remote: URL(string: "\(base)/tiny-en/quantized/tiny-en/encoder_model.ort")!,
                sha256: "94e90a4654fc45cdfedb77c4c08e1739f48862998e58fada384b25118134f221",
                expectedBytes: 13_281_600
            ),
            Pin(
                fileName: "decoder_model_merged.ort",
                remote: URL(string: "\(base)/tiny-en/quantized/tiny-en/decoder_model_merged.ort")!,
                sha256: "cf524c4862d36e9e5ab032eddc73637efd822d70e868ac575cf1a46e1e4708a0",
                expectedBytes: 30_412_256
            ),
            Pin(
                fileName: "tokenizer.bin",
                remote: URL(string: "\(base)/tiny-en/quantized/tiny-en/tokenizer.bin")!,
                sha256: "6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d",
                expectedBytes: 249_974
            ),
        ]
    )

    public static func pin(for engine: EngineChoice) -> ModelPin {
        switch engine {
        case .moonshineBase: return moonshineBasePin
        case .moonshineTiny: return moonshineTinyPin
        }
    }

    public enum DownloadError: Error, Equatable, Sendable {
        case transport(String)
        case badStatus(Int)
        case checksumMismatch(file: String, expected: String, actual: String)
        case cancelled
        case placeholderPin
        case alreadyDownloading
    }

    public enum Progress: Sendable, Equatable {
        /// Bytes across the whole set, so the bar rises once.
        case downloading(received: Int64, expected: Int64)
        /// Hashing the file just downloaded, named so the screen can say which.
        case verifying(file: String)
        /// Every file is verified; the set is being moved into place.
        case installing
        /// The model directory, ready for the engine to open.
        case finished(URL)
    }

    private let store: ModelStore
    private let session: URLSession
    private var workTask: Task<Void, Never>?

    public init(store: ModelStore, session: URLSession = .shared) {
        self.store = store
        self.session = session
    }

    /// Download, verify, install, as one transaction over the whole set. Emits
    /// progress as an `AsyncThrowingStream` so the download screen can show the
    /// 141 MB honestly instead of a spinner.
    ///
    /// A download task rather than a byte stream: `URLSession.AsyncBytes`
    /// delivers one `UInt8` per iteration, which is fine for a JSON response and
    /// hopeless for 109 MB. The task streams to a file the system manages and
    /// reports progress through the delegate; the digest is then taken over that
    /// file in 1 MB pieces, so nothing large is ever held in memory.
    ///
    /// Verification is not optional and not a warning: a mismatch on any file
    /// throws without installing the set, because the alternative is running
    /// unknown weights on someone's voice. Previously verified files remain
    /// resumable; a file with a mismatched digest is never kept.
    ///
    /// Files land in a unique sibling `.partial.UUID` directory and move across only
    /// once all three have passed. That is what makes an interrupted download
    /// safe to walk away from: the model directory either does not exist or is
    /// complete, and `isInstalled` never has to decide what a half a model means.
    ///
    /// A failure moves verified files to a resume cache and removes only its own
    /// staging directory. An install
    /// that is already there is somebody's working recogniser, and a flaky
    /// network on a re-download is not a reason to take it away: they would be
    /// left unable to dictate by an operation they only started because they
    /// were told a newer model existed. The installed set is touched at exactly
    /// one point, after every file has been verified, when the replacement is
    /// two directory operations from done.
    public func download(pin: ModelPin) -> AsyncThrowingStream<Progress, Error> {
        guard workTask == nil else {
            return AsyncThrowingStream { $0.finish(throwing: DownloadError.alreadyDownloading) }
        }
        let store = self.store
        let session = self.session
        return AsyncThrowingStream(bufferingPolicy: .bufferingNewest(16)) { continuation in
            let work = Task {
                let target = store.subdirectory(pin.subdirectory)
                let staging = store.subdirectory(pin.subdirectory + ".partial." + UUID().uuidString)
                let resume = store.subdirectory(pin.subdirectory + ".resume")
                let key = target.directory.standardizedFileURL.path
                let owner = UUID()
                var acquired = false
                let result: Result<URL, Error>
                do {
                    try await Self.admission.acquire(key: key, owner: owner)
                    acquired = true
                    try Task.checkCancellation()
                    guard !pin.files.isEmpty,
                          !pin.files.contains(where: { $0.sha256 == Self.placeholderDigest }) else {
                        throw DownloadError.placeholderPin
                    }
                    try store.prepare()
                    try staging.prepare()
                    try resume.prepare()
                    let total = pin.expectedBytes
                    var finishedBytes: Int64 = 0

                    for file in pin.files {
                        try Task.checkCancellation()
                        if resume.exists(file.fileName),
                           (try? resume.verify(file.fileName, sha256Hex: file.sha256)) != nil {
                            try staging.install(from: resume.url(for: file.fileName), as: file.fileName)
                            finishedBytes += file.expectedBytes
                            continuation.yield(.downloading(received: finishedBytes, expected: total))
                            continue
                        }
                        let alreadyDone = finishedBytes
                        let observer = DownloadProgressObserver { received, _ in
                            continuation.yield(.downloading(received: alreadyDone + received, expected: total))
                        }
                        let resumeURL = resume.url(for: file.fileName + ".resume-data")
                        let temporary: URL
                        let response: URLResponse
                        do {
                            if let data = try? Data(contentsOf: resumeURL) {
                                (temporary, response) = try await session.download(resumeFrom: data, delegate: observer)
                            } else {
                                (temporary, response) = try await session.download(from: file.remote, delegate: observer)
                            }
                            try? FileManager.default.removeItem(at: resumeURL)
                        } catch {
                            if let data = (error as NSError).userInfo["NSURLSessionDownloadTaskResumeData"] as? Data {
                                try? data.write(to: resumeURL, options: .atomic)
                            } else {
                                // An expired/invalid system resume ticket should
                                // fall back to a fresh transfer on the next try.
                                try? FileManager.default.removeItem(at: resumeURL)
                            }
                            throw error
                        }
                        defer { try? FileManager.default.removeItem(at: temporary) }
                        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
                            throw DownloadError.badStatus(http.statusCode)
                        }
                        try Task.checkCancellation()
                        continuation.yield(.verifying(file: file.fileName))
                        let actual = try ModelStore.sha256Hex(ofFileAt: temporary)
                        guard actual.caseInsensitiveCompare(file.sha256) == .orderedSame else {
                            throw DownloadError.checksumMismatch(file: file.fileName,
                                                                 expected: file.sha256.lowercased(), actual: actual)
                        }
                        try Task.checkCancellation()
                        try staging.install(from: temporary, as: file.fileName)
                        finishedBytes += file.expectedBytes
                        continuation.yield(.downloading(received: finishedBytes, expected: total))
                    }
                    try Task.checkCancellation()
                    continuation.yield(.installing)
                    try target.replaceDirectory(from: staging.directory)
                    try? resume.removeAll()
                    result = .success(target.directory)
                } catch {
                    // Only verified complete files ever enter staging. Keep them
                    // for explicit Resume; each attempt otherwise owns its paths.
                    if acquired {
                        _ = try? resume.prepare()
                        for file in pin.files where staging.exists(file.fileName) {
                            try? resume.install(from: staging.url(for: file.fileName), as: file.fileName)
                        }
                    }
                    try? staging.removeAll()
                    if Task.isCancelled || error is CancellationError ||
                        (error as? URLError)?.code == .cancelled {
                        result = .failure(DownloadError.cancelled)
                    } else if error is DownloadError || error is ModelStore.StoreError {
                        result = .failure(error)
                    } else {
                        result = .failure(DownloadError.transport(error.localizedDescription))
                    }
                }
                if acquired { await Self.admission.release(key: key, owner: owner) }
                // Clear ownership before notifying the consumer, so a retry
                // after completion cannot replace a still-running task handle.
                self.workTask = nil
                switch result {
                case .success(let url):
                    continuation.yield(.finished(url))
                    continuation.finish()
                case .failure(let error):
                    continuation.finish(throwing: error)
                }
            }
            self.workTask = work
            continuation.onTermination = { _ in work.cancel() }
        }
    }

    private static let admission = ModelDownloadAdmission()

    public func cancelAndWait() async {
        let task = workTask
        task?.cancel()
        await task?.value
    }

    public func hasResumeData(pin: ModelPin) -> Bool {
        let resume = store.subdirectory(pin.subdirectory + ".resume")
        return pin.files.contains { resume.exists($0.fileName) || resume.exists($0.fileName + ".resume-data") }
    }

    /// Where the engine opens the model from. Valid whether or not it is
    /// installed; ask `isInstalled` for that.
    public nonisolated func directory(for pin: ModelPin) -> URL {
        store.subdirectory(pin.subdirectory).directory
    }

    /// Cheap: every file is there and is the size it was pinned at.
    ///
    /// What the download screen should ask on appear. It reads three directory
    /// entries rather than 141 MB, and it is enough to answer "is there anything
    /// to download", which is the question that screen is actually asking.
    ///
    /// It cannot catch a file that was corrupted in place without changing size,
    /// which is why it is not what gates loading. `isInstalled` is, and `load()`
    /// fails loudly behind it.
    public func isPresent(pin: ModelPin) -> Bool {
        let target = store.subdirectory(pin.subdirectory)
        for file in pin.files {
            guard target.exists(file.fileName) else { return false }
            guard target.sizeOnDisk(file.fileName) == file.expectedBytes else { return false }
        }
        return true
    }

    /// True when every file in the set is present and passes its checksum.
    ///
    /// All three are hashed, not just counted: a file truncated by a full disk
    /// exists and is the wrong file, and finding that out here costs a second
    /// once, where finding it out in `Transcriber.init` costs the user a take.
    ///
    /// **This reads and hashes all 141 MB, every call.** It is not something to
    /// put in a SwiftUI computed property, a `body`, or anything else that runs
    /// more than once: a view that asked it on every redraw would hash the model
    /// repeatedly while the user scrolled. The download screen should call
    /// `isPresent` for the cheap answer, or call this once and hold the verdict
    /// in state.
    public func isInstalled(pin: ModelPin) -> Bool {
        let target = store.subdirectory(pin.subdirectory)
        for file in pin.files {
            guard target.exists(file.fileName) else { return false }
            guard file.sha256 != Self.placeholderDigest else { continue }
            guard (try? target.verify(file.fileName, sha256Hex: file.sha256)) != nil else { return false }
        }
        return true
    }
}

/// Shared across downloader instances, so two sheets cannot mutate one model.
private actor ModelDownloadAdmission {
    private var owners: [String: UUID] = [:]

    func acquire(key: String, owner: UUID) throws {
        guard owners[key] == nil else { throw ModelDownloader.DownloadError.alreadyDownloading }
        owners[key] = owner
    }

    func release(key: String, owner: UUID) {
        if owners[key] == owner { owners[key] = nil }
    }
}

/// Turns `URLSessionDownloadDelegate`'s byte counts into a closure call.
///
/// `URLSession` calls its delegate on its own queue, so the callback is
/// `@Sendable` and the observer holds nothing mutable of its own.
private final class DownloadProgressObserver: NSObject, URLSessionTaskDelegate, URLSessionDownloadDelegate, Sendable {
    private let onProgress: @Sendable (Int64, Int64) -> Void

    init(onProgress: @escaping @Sendable (Int64, Int64) -> Void) {
        self.onProgress = onProgress
    }

    func urlSession(
        _ session: URLSession,
        downloadTask: URLSessionDownloadTask,
        didWriteData bytesWritten: Int64,
        totalBytesWritten: Int64,
        totalBytesExpectedToWrite: Int64
    ) {
        onProgress(totalBytesWritten, totalBytesExpectedToWrite)
    }

    /// Required by the protocol. The async `download(from:delegate:)` API hands
    /// the finished file back through its return value, so there is nothing to
    /// do here.
    func urlSession(
        _ session: URLSession,
        downloadTask: URLSessionDownloadTask,
        didFinishDownloadingTo location: URL
    ) {}
}
