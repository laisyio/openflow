import Foundation

public enum DictationPhase: Sendable, Equatable {
    case idle, starting, recording, stopping, transcribing, cancelling
    case finished(String)
    case failed(String)

    public var isBusy: Bool {
        switch self {
        case .starting, .recording, .stopping, .transcribing, .cancelling: true
        case .idle, .finished, .failed: false
        }
    }
}

/// Session identity is checked after every suspension in the capture UI.
/// Cancellation invalidates delivery immediately but keeps admission closed
/// until microphone and inference cleanup have actually finished.
public struct DictationSession: Sendable {
    public private(set) var phase: DictationPhase = .idle
    public private(set) var id: UUID?

    public init() {}

    public mutating func begin() -> UUID? {
        guard !phase.isBusy else { return nil }
        let id = UUID()
        self.id = id
        phase = .starting
        return id
    }

    public func owns(_ id: UUID) -> Bool { self.id == id && phase != .cancelling }

    public mutating func beginTranscription() -> UUID? {
        guard let id = begin() else { return nil }
        phase = .transcribing
        return id
    }

    @discardableResult
    public mutating func advance(_ id: UUID, to next: DictationPhase) -> Bool {
        guard owns(id) else { return false }
        let allowed: Bool
        switch (phase, next) {
        case (.starting, .recording), (.recording, .stopping), (.stopping, .transcribing):
            allowed = true
        case (_, .failed): allowed = true
        case (.transcribing, .finished): allowed = true
        default: allowed = false
        }
        guard allowed else { return false }
        phase = next
        if !next.isBusy { self.id = nil }
        return true
    }

    @discardableResult
    public mutating func cancel() -> UUID? {
        guard phase.isBusy, phase != .cancelling else { return nil }
        let cancelled = id
        id = nil
        phase = .cancelling
        return cancelled
    }

    public mutating func cancellationFinished(error: String? = nil) {
        guard phase == .cancelling else { return }
        phase = error.map(DictationPhase.failed) ?? .idle
    }
}
