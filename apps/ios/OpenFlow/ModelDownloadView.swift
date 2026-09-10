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

    @State private var download: ModelDownloadController?
    @State private var configurationError: String?

    private var state: ModelDownloadController.Phase {
        configurationError.map(ModelDownloadController.Phase.failed) ?? download?.phase ?? .checking
    }

    private var pin: ModelDownloader.ModelPin {
        ModelDownloader.pin(for: controller.settings.engine)
    }

    /// What the selected recogniser costs. The figures below used to be typed
    /// into this file, so picking the lighter engine still read the heavier
    /// engine's numbers, and moving a pin left them quietly wrong.
    private var profile: EngineProfile {
        EngineProfile.profile(for: controller.settings.engine)
    }

    /// "three", from the pin, rather than typed into three sentences. A Moonshine
    /// model is an encoder, a decoder and a tokenizer today; the day one is
    /// pinned that is not, the copy follows instead of lying.
    ///
    /// Spelled out rather than run through a number formatter, for the same
    /// reason `EngineProfile` formats its sizes by hand: the sentence around it
    /// is English, and a localised digit in the middle of it would be the worse
    /// of the two.
    private var fileCount: String {
        switch profile.fileCount {
        case 1: return "one"
        case 2: return "two"
        case 3: return "three"
        default: return String(profile.fileCount)
        }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("OpenFlow recognises speech on this phone. To do that it needs the recogniser itself, which is \(fileCount) files.")
                    LabeledContent("Download size", value: "\(profile.downloadDescription) in \(fileCount) files, once")
                    LabeledContent("Space on disk", value: "\(profile.downloadDescription), kept out of your backups")
                    LabeledContent("Memory while dictating", value: profile.residentDescription)
                } header: {
                    Text("What this costs")
                }

                Section {
                    Text("Each of the \(fileCount) files is checked against a fingerprint built into the app. Only a complete, verified recogniser is installed. Cancelling keeps verified files so you can resume.")
                    Text("This is the only network request OpenFlow ever makes. After it finishes, the app works with the phone in Airplane Mode.")
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Where it comes from")
                }

                Section {
                    switch state {
                    case .checking:
                        ProgressView("Checking installed recogniser")
                    case .idle:
                        Button("Download the recogniser") { download?.start() }
                    case .paused:
                        Text("The download is paused. Verified files are kept; unfinished transfers resume when the server supports it.")
                            .font(.caption)
                        Button("Resume download") { download?.start() }
                    case .cancelling:
                        ProgressView("Pausing download")
                    case .downloading:
                        VStack(alignment: .leading, spacing: 6) {
                            ProgressView(value: fraction)
                            Text(progressLabel).font(.caption).foregroundStyle(.secondary)
                            Button("Cancel", role: .destructive) { Task { await download?.cancel() } }
                        }
                    case .verifying(let file):
                        ProgressView("Checking \(file)")
                        Button("Cancel", role: .destructive) { Task { await download?.cancel() } }
                    case .installing:
                        ProgressView("Putting it in place")
                    case .installed:
                        Label("Ready. Nothing else needs to be downloaded.", systemImage: "checkmark.circle")
                            .foregroundStyle(.green)
                    case .failed(let reason):
                        VStack(alignment: .leading, spacing: 8) {
                            Label(reason, systemImage: "exclamationmark.triangle")
                                .foregroundStyle(.red)
                            Text("Your existing recogniser is unchanged. You can retry the download.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Button("Try again") { download?.start() }
                        }
                    }
                }
            }
            .navigationTitle("The recogniser")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) { Button("Done") { dismiss() } }
            }
        }
        .task {
            do {
                if download == nil {
                    let store = try ModelStore.applicationSupport()
                    download = ModelDownloadController(downloader: ModelDownloader(store: store), pin: pin)
                }
                await download?.refresh()
            } catch { configurationError = error.localizedDescription }
        }
        .onDisappear { Task { await download?.cancel() } }
    }

    private var fraction: Double {
        let expected = download?.expected ?? pin.expectedBytes
        return expected > 0 ? min(1, Double(download?.received ?? 0) / Double(expected)) : 0
    }

    private var progressLabel: String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        return "\(formatter.string(fromByteCount: download?.received ?? 0)) of \(formatter.string(fromByteCount: download?.expected ?? pin.expectedBytes))"
    }

}
