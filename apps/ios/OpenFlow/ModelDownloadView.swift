import SwiftUI
import OpenFlowMobileCore

/// The one screen where the app admits what it costs, before it spends anything.
///
/// PLAN.md section 0: "the cost is honesty about overhead". No progress bar
/// theatre, no "preparing your experience": the size, the memory, where it comes
/// from and what happens if the file is wrong, in plain words.
struct ModelDownloadView: View {
    @Environment(DictationController.self) private var controller
    @Environment(\.dismiss) private var dismiss

    @State private var state: Phase = .idle
    @State private var received: Int64 = 0
    @State private var expected: Int64 = 0

    enum Phase: Equatable {
        case idle
        case downloading
        case verifying
        case installing
        case installed
        case failed(String)
    }

    private var pin: ModelDownloader.Pin {
        ModelDownloader.pin(for: controller.settings.engine)
    }

    /// What the selected recogniser costs. The figures below used to be typed
    /// into this file, so picking the lighter engine still read the heavier
    /// engine's numbers, and moving a pin left them quietly wrong.
    private var profile: EngineProfile {
        EngineProfile.profile(for: controller.settings.engine)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("OpenFlow recognises speech on this phone. To do that it needs the recogniser itself, which is a large file.")
                    LabeledContent("Download size", value: "\(profile.downloadDescription), once")
                    LabeledContent("Space on disk", value: "\(profile.downloadDescription), kept out of your backups")
                    LabeledContent("Memory while dictating", value: profile.residentDescription)
                } header: {
                    Text("What this costs")
                }

                Section {
                    Text("The file is checked against a fingerprint built into the app. If it does not match, OpenFlow deletes it and refuses to use it.")
                    Text("This is the only network request OpenFlow ever makes. After it finishes, the app works with the phone in Airplane Mode.")
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Where it comes from")
                }

                Section {
                    switch state {
                    case .idle:
                        Button("Download the recogniser") { start() }
                    case .downloading:
                        VStack(alignment: .leading, spacing: 6) {
                            ProgressView(value: fraction)
                            Text(progressLabel).font(.caption).foregroundStyle(.secondary)
                            Button("Cancel", role: .destructive) { state = .idle }
                        }
                    case .verifying:
                        ProgressView("Checking the fingerprint")
                    case .installing:
                        ProgressView("Putting it in place")
                    case .installed:
                        Label("Ready. Nothing else needs to be downloaded.", systemImage: "checkmark.circle")
                            .foregroundStyle(.green)
                    case .failed(let reason):
                        VStack(alignment: .leading, spacing: 8) {
                            Label(reason, systemImage: "exclamationmark.triangle")
                                .foregroundStyle(.red)
                            Text("Nothing was installed. You can try again on a different network.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Button("Try again") { start() }
                        }
                    }
                }
            }
            .navigationTitle("The recogniser")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) { Button("Done") { dismiss() } }
            }
        }
    }

    private var fraction: Double {
        expected > 0 ? min(1, Double(received) / Double(expected)) : 0
    }

    private var progressLabel: String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        return "\(formatter.string(fromByteCount: received)) of \(formatter.string(fromByteCount: expected))"
    }

    private func start() {
        state = .downloading
        received = 0
        expected = pin.expectedBytes
        let pin = self.pin
        Task {
            do {
                let store = try ModelStore.applicationSupport()
                let downloader = ModelDownloader(store: store)
                for try await progress in await downloader.download(pin: pin) {
                    switch progress {
                    case .downloading(let got, let want):
                        received = got
                        expected = want
                    case .verifying:
                        state = .verifying
                    case .installing:
                        state = .installing
                    case .finished:
                        state = .installed
                    }
                }
            } catch ModelDownloader.DownloadError.placeholderPin {
                state = .failed("This build has no recogniser pinned yet. Milestone M2 adds it.")
            } catch ModelDownloader.DownloadError.checksumMismatch {
                state = .failed("The downloaded file did not match its fingerprint, so it was deleted.")
            } catch {
                state = .failed((error as NSError).localizedDescription)
            }
        }
    }
}
