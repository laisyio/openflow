import Foundation
import Testing
@testable import OpenFlowMobileCore

@Suite struct TranscriptDeliveryTests {
    @Test func cancelledDeliveryDoesNotWriteLastOrHistory() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = TranscriptStore(directory: directory)
        let repository = TranscriptRepository(store: store)
        let delivery = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await repository.deliver(TranscriptRecord(text: "cancelled"),
                                                saveHistory: true, retentionDays: 30)
        }
        await #expect(throws: CancellationError.self) { try await delivery.value }
        #expect(store.lastEntry() == nil)
        #expect(store.loadHistory().isEmpty)
    }

    @Test func cancellationBeforeCommitPreservesExistingFiles() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = TranscriptStore(directory: directory)
        let repository = TranscriptRepository(store: store)
        let existing = TranscriptRecord(text: "already accepted")
        _ = try await repository.deliver(existing, saveHistory: true, retentionDays: 30)
        let authorization = TranscriptDeliveryAuthorization()
        #expect(authorization.cancel())
        await #expect(throws: CancellationError.self) {
            try await repository.deliver(TranscriptRecord(text: "cancelled"), saveHistory: true,
                                         retentionDays: 30, authorization: authorization)
        }
        #expect(store.lastEntry() == existing)
        #expect(store.loadHistory() == [existing])
    }

    @Test func cancellationAfterCommitIsNotDeletionOrASecondCommit() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = TranscriptStore(directory: directory)
        let repository = TranscriptRepository(store: store)
        let authorization = TranscriptDeliveryAuthorization()
        let accepted = TranscriptRecord(text: "accepted before cancel")
        _ = try await repository.deliver(accepted, saveHistory: true, retentionDays: 30,
                                         authorization: authorization)
        #expect(!authorization.cancel(), "an already accepted persistence operation cannot be revoked")
        await #expect(throws: CancellationError.self) {
            try await repository.deliver(TranscriptRecord(text: "duplicate"), saveHistory: true,
                                         retentionDays: 30, authorization: authorization)
        }
        #expect(store.lastEntry() == accepted)
        #expect(store.loadHistory() == [accepted])
    }
}
