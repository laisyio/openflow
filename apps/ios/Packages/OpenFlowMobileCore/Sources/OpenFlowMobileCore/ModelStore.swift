import Foundation
import CryptoKit

/// Where the weights live on disk, and the proof they are the ones we pinned.
///
/// PLAN.md section 5: 141 MB for base-en and 44 MB for tiny-en, three files each,
/// never bundled, under Application Support with `isExcludedFromBackup = true`
/// -- iCloud should not carry a model a re-download reproduces exactly -- and
/// every file checked with SHA-256 against a value compiled into the app.
///
/// One store is one directory. A model that is a set of files lives in its own
/// subdirectory, reached with `subdirectory(_:)`, so base-en and tiny-en can both
/// be installed without their same-named files colliding.
public struct ModelStore: Sendable {
    public enum StoreError: Error, Equatable, Sendable {
        case directoryUnavailable(String)
        case fileMissing(String)
        case checksumMismatch(expected: String, actual: String)
    }

    public let directory: URL

    /// Injectable so the tests get a temporary directory.
    public init(directory: URL) {
        self.directory = directory
    }

    /// `Application Support/Models`. Application Support and not Caches: the
    /// system may purge Caches at any time, and re-downloading 141 MB because
    /// the phone wanted disk space is not a trade we want to make silently.
    public static func applicationSupport() throws -> ModelStore {
        guard let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
            throw StoreError.directoryUnavailable("No Application Support directory")
        }
        return ModelStore(directory: base.appendingPathComponent("OpenFlow/Models", isDirectory: true))
    }

    /// A store rooted at a subdirectory of this one. The path is relative and is
    /// created by `prepare()` along with everything above it, so callers do not
    /// have to make the intermediate directories themselves.
    ///
    /// Only the top of the tree carries the exclude-from-backup flag, which is
    /// how the flag works: it is inherited by everything underneath, and setting
    /// it again on each engine's directory would be noise.
    public func subdirectory(_ relativePath: String) -> ModelStore {
        ModelStore(directory: directory.appendingPathComponent(relativePath, isDirectory: true))
    }

    /// The same store, for a caller with nowhere to put an error.
    ///
    /// `DictationController.init` is not throwing and constructs the engine
    /// before anything is downloaded, so it needs a store rather than a
    /// decision. Application Support is missing only on a system that is already
    /// broken; falling back to the temporary directory keeps the app launchable
    /// and lets the failure surface where it can be reported, at `load()`, as
    /// weights that are not there.
    ///
    /// It exists so the `OpenFlow/Models` path stays written down once. A caller
    /// spelling the fallback itself is how the app and the downloader end up
    /// looking in two different places.
    public static func applicationSupportOrTemporary() -> ModelStore {
        (try? applicationSupport())
            ?? ModelStore(
                directory: FileManager.default.temporaryDirectory
                    .appendingPathComponent("OpenFlow/Models", isDirectory: true)
            )
    }

    public func url(for name: String) -> URL {
        directory.appendingPathComponent(name)
    }

    public func exists(_ name: String) -> Bool {
        FileManager.default.fileExists(atPath: url(for: name).path)
    }

    public func sizeOnDisk(_ name: String) -> Int64 {
        let attributes = try? FileManager.default.attributesOfItem(atPath: url(for: name).path)
        return (attributes?[.size] as? NSNumber)?.int64Value ?? 0
    }

    /// Create the directory and mark it out of backup. Both are idempotent.
    @discardableResult
    public func prepare() throws -> URL {
        var directory = self.directory
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try directory.setResourceValues(values)
        return directory
    }

    public func isExcludedFromBackup() -> Bool {
        (try? directory.resourceValues(forKeys: [.isExcludedFromBackupKey]))?.isExcludedFromBackup ?? false
    }

    /// Streaming SHA-256. Chunked because the largest of these files is 109 MB
    /// and reading it into memory to hash it would undo the whole point of the
    /// memory budget.
    public static func sha256Hex(ofFileAt url: URL, chunkBytes: Int = 1 << 20) throws -> String {
        guard let handle = try? FileHandle(forReadingFrom: url) else {
            throw StoreError.fileMissing(url.lastPathComponent)
        }
        defer { try? handle.close() }
        var hasher = SHA256()
        while true {
            let chunk = try handle.read(upToCount: chunkBytes) ?? Data()
            if chunk.isEmpty { break }
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// Throws `checksumMismatch` rather than returning false: a weights file that
    /// does not match the pin is not a condition to branch on quietly.
    public func verify(_ name: String, sha256Hex expected: String) throws {
        let actual = try Self.sha256Hex(ofFileAt: url(for: name))
        guard actual.caseInsensitiveCompare(expected) == .orderedSame else {
            throw StoreError.checksumMismatch(expected: expected.lowercased(), actual: actual)
        }
    }

    /// Delete a bad or unwanted download.
    public func remove(_ name: String) throws {
        let target = url(for: name)
        if FileManager.default.fileExists(atPath: target.path) {
            try FileManager.default.removeItem(at: target)
        }
    }

    /// Delete the whole directory this store is rooted at.
    ///
    /// What a half-installed model set is cleaned up with: a model is three
    /// files, and two of them verified is not a model, so the directory goes
    /// rather than being left for the next launch to mistake for an install.
    public func removeAll() throws {
        if FileManager.default.fileExists(atPath: directory.path) {
            try FileManager.default.removeItem(at: directory)
        }
    }

    /// Move a finished download into place, replacing anything already there.
    public func install(from temporary: URL, as name: String) throws {
        try prepare()
        let destination = url(for: name)
        if FileManager.default.fileExists(atPath: destination.path) {
            try FileManager.default.removeItem(at: destination)
        }
        try FileManager.default.moveItem(at: temporary, to: destination)
    }
}
