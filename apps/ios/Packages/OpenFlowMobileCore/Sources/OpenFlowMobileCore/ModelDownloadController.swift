import Foundation
import Observation

/// The download screen's owned task, also exercised by host-side tests.
@MainActor
@Observable
public final class ModelDownloadController {
    public enum Phase: Sendable, Equatable {
        case checking, idle, paused, downloading, cancelling
        case verifying(String)
        case installing, installed
        case failed(String)
    }

    public private(set) var phase: Phase = .checking
    public private(set) var received: Int64 = 0
    public let expected: Int64
    private let downloader: ModelDownloader
    private let pin: ModelDownloader.ModelPin
    private var task: Task<Void, Never>?
    private var generation = 0

    public init(downloader: ModelDownloader, pin: ModelDownloader.ModelPin) {
        self.downloader = downloader
        self.pin = pin
        self.expected = pin.expectedBytes
    }

    public func refresh() async {
        guard task == nil else { return }
        let current = generation
        let installed = await downloader.isPresent(pin: pin)
        let resumable = await downloader.hasResumeData(pin: pin)
        guard current == generation, task == nil else { return }
        phase = installed ? .installed : (resumable ? .paused : .idle)
    }

    public func start() {
        guard task == nil else { return }
        generation += 1
        let current = generation
        received = 0
        phase = .downloading
        task = Task { [weak self] in
            guard let self else { return }
            do {
                try Task.checkCancellation()
                for try await progress in await downloader.download(pin: pin) {
                    guard current == generation, !Task.isCancelled else { return }
                    switch progress {
                    case .downloading(let bytes, _):
                        received = max(received, min(expected, bytes))
                        phase = .downloading
                    case .verifying(let file): phase = .verifying(file)
                    case .installing: phase = .installing
                    case .finished: phase = .installed
                    }
                }
            } catch {
                guard current == generation, !Task.isCancelled else { return }
                switch error {
                case ModelDownloader.DownloadError.placeholderPin:
                    phase = .failed("This build has no recogniser pinned yet.")
                case ModelDownloader.DownloadError.alreadyDownloading:
                    phase = .failed("This recogniser is already being downloaded.")
                case ModelDownloader.DownloadError.checksumMismatch(let file, _, _):
                    phase = .failed("\(file) did not match its fingerprint. The existing recogniser was kept.")
                default: phase = .failed(error.localizedDescription)
                }
            }
            if current == generation { task = nil }
        }
    }

    public func cancel() async {
        guard let pending = task, phase != .cancelling else { return }
        generation += 1
        phase = .cancelling
        pending.cancel()
        await downloader.cancelAndWait()
        await pending.value
        await downloader.cancelAndWait()
        task = nil
        await refresh()
    }
}
