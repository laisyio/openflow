import SwiftUI
import OpenFlowMobileCore

/// One trigger, one speak, one paste. The sheet shows what the model is doing so
/// a slow first take is explained rather than mysterious.
struct CaptureSheet: View {
    @Environment(DictationController.self) private var controller
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 20) {
            header

            Group {
                switch controller.phase {
                case .idle:
                    Text("Tap record and speak.")
                        .foregroundStyle(.secondary)
                case .recording:
                    RecordingIndicator(stopsOnSilence: controller.settings.stopOnSilence)
                case .transcribing:
                    ProgressView("Transcribing on this device")
                case .finished(let text):
                    VStack(alignment: .leading, spacing: 10) {
                        ScrollView {
                            Text(text)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .textSelection(.enabled)
                        }
                        // Only set when the take hit the length ceiling. It sits
                        // with the transcript rather than replacing it: the
                        // words are still the point, and a take missing its
                        // opening otherwise reads as one that started late.
                        if let notice = controller.captureNotice {
                            Label(notice, systemImage: "clock.badge.exclamationmark")
                                .font(.footnote)
                                .foregroundStyle(.secondary)
                        }
                    }
                case .failed(let reason):
                    Label(reason, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                }
            }
            .frame(maxHeight: .infinity)

            controls
        }
        .padding(24)
        .task {
            // Prewarm the moment the sheet appears, before the user has decided
            // to speak; by the time they stop, the model is usually there.
            await controller.prewarm(trigger: .captureStart)
        }
    }

    private var header: some View {
        HStack {
            Circle()
                .fill(statusColour)
                .frame(width: 10, height: 10)
            Text(controller.modelState.describedForUI)
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            Button("Close") {
                Task { await controller.cancelRecording() }
                dismiss()
            }
            .font(.caption)
        }
    }

    @ViewBuilder
    private var controls: some View {
        switch controller.phase {
        case .recording:
            Button(role: .destructive) {
                Task { await controller.stopRecording() }
            } label: {
                Label("Stop", systemImage: "stop.fill")
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
            }
            .buttonStyle(.borderedProminent)

        case .finished(let text):
            VStack(spacing: 12) {
                Text("Copied to the clipboard. Paste it anywhere, or use the OpenFlow keyboard.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                HStack {
                    ShareLink(item: text) {
                        Label("Share", systemImage: "square.and.arrow.up")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.bordered)
                    Button {
                        Task { await controller.startRecording() }
                    } label: {
                        Label("Again", systemImage: "mic.fill")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                }
            }

        default:
            Button {
                Task { await controller.startRecording() }
            } label: {
                Label("Record", systemImage: "mic.fill")
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
            }
            .buttonStyle(.borderedProminent)
        }
    }

    private var statusColour: Color {
        switch controller.modelState {
        case .ready: return .green
        case .loading, .unloading: return .orange
        case .failed: return .red
        case .unloaded: return .secondary
        }
    }
}

private struct RecordingIndicator: View {
    let stopsOnSilence: Bool
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "waveform")
                .font(.system(size: 44))
                .foregroundStyle(.red)
                .opacity(pulse ? 0.4 : 1)
                .animation(.easeInOut(duration: 0.8).repeatForever(), value: pulse)
                .onAppear { pulse = true }
            Text(stopsOnSilence ? "Listening. It stops itself when you go quiet." : "Listening.")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }
}
