import Foundation

/// All app history disk work is serialized away from the main actor. The
/// keyboard still reads the independent, small last.json file directly.
public actor TranscriptRepository {
    private let store: TranscriptStore
    private var cached: [TranscriptRecord]?

    public init(store: TranscriptStore) { self.store = store }

    public func history(retentionDays: Int, now: Date = Date()) throws -> [TranscriptRecord] {
        let records = cached ?? store.loadHistory()
        let kept = TranscriptStore.pruned(records, retentionDays: retentionDays, now: now)
        if kept != records { try store.replaceHistory(kept) }
        cached = kept
        return kept.reversed()
    }

    public func deliver(_ record: TranscriptRecord, saveHistory: Bool, retentionDays: Int,
                        authorization: TranscriptDeliveryAuthorization = TranscriptDeliveryAuthorization()) throws -> [TranscriptRecord] {
        // The controller can cancel while this actor hop is queued. Claim
        // persistence before either file is touched, using its shared token.
        try authorization.beginCommit()
        try store.saveLast(record)
        if saveHistory {
            let kept = try store.append(record, retentionDays: retentionDays)
            cached = kept
            return kept.reversed()
        }
        return try history(retentionDays: retentionDays)
    }

    public func delete(id: UUID, retentionDays: Int) throws -> [TranscriptRecord] {
        try store.delete(id: id)
        cached = nil
        return try history(retentionDays: retentionDays)
    }

    public func deleteAll() throws {
        try store.deleteAll()
        cached = []
    }
}
