import Foundation

/// Linearizes cancellation against one transcript's persistence admission.
///
/// Cancellation that wins before `beginCommit` prevents both last/history
/// writes. Once admitted, persistence may finish even if the UI subsequently
/// closes; cancelling is not deletion of a take already accepted for saving.
/// The lock protects only this state change, never encoding or disk I/O.
public final class TranscriptDeliveryAuthorization: @unchecked Sendable {
    private enum State { case pending, cancelled, committed }
    private let lock = NSLock()
    private var state: State = .pending

    public init() {}

    /// Called synchronously by the controller before invalidating the session
    /// or awaiting task drainage. True means this call revoked a pending commit;
    /// false means it was already admitted or already cancelled.
    @discardableResult
    public func cancel() -> Bool {
        lock.withLock {
            guard state == .pending else { return false }
            state = .cancelled
            return true
        }
    }

    func beginCommit() throws {
        try lock.withLock {
            guard state == .pending, !Task.isCancelled else { throw CancellationError() }
            state = .committed
        }
    }
}
