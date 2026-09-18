import SwiftUI
import OpenFlowMobileCore

/// Everything the app has ever transcribed, on this phone, for as long as the
/// retention setting says. There is nowhere else it could be.
struct HistoryView: View {
    @Environment(DictationController.self) private var controller

    var body: some View {
        NavigationStack {
            Group {
                if controller.history.isEmpty {
                    ContentUnavailableView(
                        "No dictations yet",
                        systemImage: "clock",
                        description: Text("Takes are kept on this phone for \(controller.settings.historyRetentionDays) days.")
                    )
                } else {
                    List {
                        ForEach(controller.history) { record in
                            VStack(alignment: .leading, spacing: 6) {
                                Text(record.text)
                                    .lineLimit(4)
                                HStack(spacing: 8) {
                                    Text(record.createdAt, style: .relative)
                                    Text(durationLabel(record.durationSeconds))
                                }
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                            }
                            .contentShape(Rectangle())
                            .onTapGesture { controller.copyToClipboard(record.text) }
                            .swipeActions {
                                Button(role: .destructive) {
                                    Task { await controller.deleteHistory(id: record.id) }
                                } label: {
                                    Label("Delete", systemImage: "trash")
                                }
                            }
                        }
                    }
                }
            }
            .navigationTitle("History")
            .toolbar {
                if !controller.history.isEmpty {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button("Delete all", role: .destructive) {
                            Task { await controller.deleteAllHistory() }
                        }
                    }
                }
            }
            .task { await controller.loadHistory() }
        }
    }

    private func durationLabel(_ seconds: Double) -> String {
        seconds < 1 ? "under a second" : String(format: "%.0f s", seconds)
    }
}
