import Foundation

/// One saved take.
public struct TranscriptRecord: Codable, Sendable, Equatable, Identifiable {
    public var id: UUID
    public var text: String
    public var createdAt: Date
    /// Seconds of audio, for the history row's subtitle.
    public var durationSeconds: Double
    /// Which engine produced it, so a mixed history is still readable.
    public var engine: String

    public init(
        id: UUID = UUID(),
        text: String,
        createdAt: Date = Date(),
        durationSeconds: Double = 0,
        engine: String = ""
    ) {
        self.id = id
        self.text = text
        // Snapped to the millisecond so a record equals itself after a round
        // trip through JSON. Sub-millisecond precision has no meaning for a
        // dictation timestamp and buys only a flaky equality.
        self.createdAt = Date(
            timeIntervalSince1970: ((createdAt.timeIntervalSince1970 * 1_000).rounded() / 1_000)
        )
        self.durationSeconds = durationSeconds
        self.engine = engine
    }
}

/// Last transcript plus history, as JSON files in the App Group container.
///
/// Two files, no database. The keyboard extension reads `last.json` and nothing
/// else, which is what keeps it inside its memory cap: a 30-day history never has
/// to be parsed to insert one line of text.
///
/// The struct itself holds no mutable state -- the file system is the state --
/// so it is `Sendable` without a lock, and the app, the keyboard and the widget
/// can each hold their own copy.
public struct TranscriptStore: Sendable {
    public enum StoreError: Error, Equatable, Sendable {
        case containerUnavailable(String)
    }

    public let directory: URL

    private var lastURL: URL { directory.appendingPathComponent("last.json") }
    private var historyURL: URL { directory.appendingPathComponent("history.json") }

    /// Injectable for tests; the app uses `shared(appGroup:)`.
    public init(directory: URL) {
        self.directory = directory
    }

    /// The App Group container. Throws when the entitlement is missing, which is
    /// a build configuration problem and should be loud rather than silent.
    public static func shared(appGroup: String = AppGroup.identifier) throws -> TranscriptStore {
        guard let container = FileManager.default
            .containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
            throw StoreError.containerUnavailable(appGroup)
        }
        let directory = container.appendingPathComponent("Transcripts", isDirectory: true)
        return TranscriptStore(directory: directory)
    }

    private func ensureDirectory() throws {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true,
            attributes: nil
        )
    }

    // MARK: - Last transcript

    /// The one the keyboard inserts. Written on every take, whether or not
    /// history is on: "insert my last dictation" must work with history off.
    public func saveLast(_ record: TranscriptRecord) throws {
        try ensureDirectory()
        let data = try Self.encoder.encode(record)
        try data.write(to: lastURL, options: [.atomic])
    }

    /// The newest take, at the cost of one small file.
    ///
    /// This is the read path for everything that wants the last transcript and
    /// not the list: the keyboard, which inserts it, and the app, which shows it
    /// at the top of History after a delivery. `last.json` holds exactly one
    /// record, so it costs the same whether the history behind it is one take or
    /// a month of them, and `history.json` is never opened.
    public func lastEntry() -> TranscriptRecord? {
        if case .record(let record) = readLast() { return record }
        return nil
    }

    /// What the keyboard extension actually needs to know: not just "is there a
    /// transcript" but "is there one I am allowed to read".
    public enum LastTranscript: Sendable, Equatable {
        /// Nothing has been dictated yet, or the user deleted it.
        case none
        /// The file is there and this process cannot open it. In the keyboard
        /// extension that means one thing: Allow Full Access is off, so the
        /// sandbox is refusing the App Group container. Telling the user to go
        /// and dictate something would be advice that cannot work.
        case unreadable
        case record(TranscriptRecord)
    }

    /// Distinguishes "nothing saved" from "not allowed to look", which
    /// `lastEntry()` collapses into nil.
    public func readLast() -> LastTranscript {
        do {
            let data = try Data(contentsOf: lastURL)
            guard let record = try? Self.decoder.decode(TranscriptRecord.self, from: data) else {
                // Present but unparseable: a truncated write, not a permission
                // problem. Nothing to insert, and nothing the user can fix.
                return .none
            }
            return .record(record)
        } catch {
            return Self.isPermissionError(error) ? .unreadable : .none
        }
    }

    /// A read that failed because the sandbox said no, rather than because the
    /// file is not there. Cocoa reports the first as `fileReadNoPermission` and
    /// the second as `fileReadNoSuchFile`; the POSIX codes are checked too,
    /// because a container the process cannot traverse surfaces as `EPERM` or
    /// `EACCES` rather than as a Cocoa error.
    static func isPermissionError(_ error: Error) -> Bool {
        let nsError = error as NSError
        if nsError.domain == NSCocoaErrorDomain {
            switch CocoaError.Code(rawValue: nsError.code) {
            case .fileReadNoPermission, .fileReadInvalidFileName:
                return true
            default:
                break
            }
        }
        if nsError.domain == NSPOSIXErrorDomain {
            return nsError.code == Int(EPERM) || nsError.code == Int(EACCES)
        }
        // A Cocoa error often carries the POSIX one underneath it.
        if let underlying = nsError.userInfo[NSUnderlyingErrorKey] as? NSError,
           underlying.domain == NSPOSIXErrorDomain {
            return underlying.code == Int(EPERM) || underlying.code == Int(EACCES)
        }
        return false
    }

    // MARK: - History

    /// The most records `history.json` is ever allowed to hold.
    ///
    /// The retention setting bounds the file in days, which is not a bound on
    /// its size: thirty days of somebody who dictates all day is however many
    /// takes that is, and every append decodes the lot and writes it back. A
    /// cap turns that into a ceiling somebody can reason about, and five hundred
    /// records is already more than the History list is usable at.
    ///
    /// The cap keeps the newest records, which is the same end retention keeps
    /// and the same end the ring keeps: what falls off is always the oldest.
    public static let maxEntries = 500

    public func loadHistory() -> [TranscriptRecord] {
        guard let data = try? Data(contentsOf: historyURL) else { return [] }
        return (try? Self.decoder.decode([TranscriptRecord].self, from: data)) ?? []
    }

    /// Append and prune in one write. `retentionDays` comes from settings;
    /// `now` is injected so retention is testable without waiting 30 days.
    ///
    /// This still decodes the file it is about to rewrite, which is the price of
    /// pruning by date, but `maxEntries` is what stops that decode growing
    /// without limit: past the cap the file stays the same size no matter how
    /// long the user keeps dictating.
    @discardableResult
    public func append(_ record: TranscriptRecord, retentionDays: Int, now: Date = Date()) throws -> [TranscriptRecord] {
        try ensureDirectory()
        var history = loadHistory()
        history.append(record)
        let kept = Self.pruned(history, retentionDays: retentionDays, now: now)
        try writeHistory(kept)
        return kept
    }

    /// Drop anything older than the window. Called on launch as well as on write,
    /// so lowering the retention setting takes effect without a new dictation.
    @discardableResult
    public func prune(retentionDays: Int, now: Date = Date()) throws -> [TranscriptRecord] {
        let kept = Self.pruned(loadHistory(), retentionDays: retentionDays, now: now)
        try ensureDirectory()
        try writeHistory(kept)
        return kept
    }

    public func delete(id: UUID) throws {
        let kept = loadHistory().filter { $0.id != id }
        try ensureDirectory()
        try writeHistory(kept)
    }

    /// Everything: history, the last transcript, the lot. The Settings screen's
    /// "delete all dictations" row.
    public func deleteAll() throws {
        for url in [historyURL, lastURL] {
            do { try FileManager.default.removeItem(at: url) }
            catch CocoaError.fileNoSuchFile { continue }
        }
    }

    static func pruned(_ history: [TranscriptRecord], retentionDays: Int, now: Date) -> [TranscriptRecord] {
        guard retentionDays > 0 else { return [] }
        let cutoff = now.addingTimeInterval(-Double(retentionDays) * 86_400)
        let kept = history
            .filter { $0.createdAt >= cutoff }
            .sorted { $0.createdAt < $1.createdAt }
        guard kept.count > maxEntries else { return kept }
        return Array(kept.suffix(maxEntries))
    }

    private func writeHistory(_ records: [TranscriptRecord]) throws {
        let data = try Self.encoder.encode(records)
        try data.write(to: historyURL, options: [.atomic])
    }

    func replaceHistory(_ records: [TranscriptRecord]) throws {
        try ensureDirectory()
        try writeHistory(records)
    }

    /// Dates go out as epoch milliseconds, not ISO-8601 strings. ISO-8601
    /// truncates to whole seconds, so a record written and read back was not
    /// equal to itself -- which matters here, because history rows are ordered
    /// by `createdAt` and the keyboard picks the last one.
    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .millisecondsSince1970
        return encoder
    }()

    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .millisecondsSince1970
        return decoder
    }()
}
