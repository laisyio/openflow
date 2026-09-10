import SwiftUI
import OpenFlowMobileCore

/// Every setting from PLAN.md section 4, plus the two numbers the plan insists
/// on making visible: what the model costs in memory, and whether it is loaded.
struct SettingsView: View {
    @Environment(DictationController.self) private var controller

    @State private var engine: EngineChoice = .qwen06
    @State private var stopOnSilence = false
    @State private var silenceHoldMs: Double = 1_200
    @State private var dictionary = ""
    @State private var clipboardExpirySeconds: Double = 60
    @State private var saveHistory = true
    @State private var historyRetentionDays: Double = 30
    @State private var unloadAfterMinutes: Double = 5
    @State private var prewarmOnCapture = true
    @State private var hapticOnStop = true
    @State private var showDiagnostics = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Engine") {
                    Picker("Recogniser", selection: $engine) {
                        ForEach(EngineChoice.allCases) { choice in
                            Text(choice.displayName).tag(choice)
                        }
                    }
                    LabeledContent("State", value: controller.modelState.describedForUI)
                    LabeledContent("Memory in use", value: residentLabel)
                    Button("Diagnostics") { showDiagnostics = true }
                }

                Section {
                    Toggle("Stop when I go quiet", isOn: $stopOnSilence)
                    if stopOnSilence {
                        VStack(alignment: .leading) {
                            Text("Silence before stopping: \(Int(silenceHoldMs)) ms")
                                .font(.caption)
                            Slider(value: $silenceHoldMs, in: 400...3_000, step: 100)
                        }
                    }
                    Toggle("Vibrate when a take ends", isOn: $hapticOnStop)
                } header: {
                    Text("Capture")
                }

                Section {
                    TextEditor(text: $dictionary)
                        .frame(minHeight: 88)
                    Text("\(dictionary.count) of 800 characters. Names and terms, separated by commas. Write `heard -> Correct` to fix a word the model mishears.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Dictionary")
                } footer: {
                    Text("Applied to the finished text on this phone. Nothing is sent anywhere to do it.")
                }

                Section("Clipboard") {
                    VStack(alignment: .leading) {
                        Text(clipboardExpiryLabel).font(.caption)
                        Slider(value: $clipboardExpirySeconds, in: 0...300, step: 15)
                    }
                    Text("Dictations are copied with Universal Clipboard off, so they never reach your other devices.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("History") {
                    Toggle("Keep a history", isOn: $saveHistory)
                    if saveHistory {
                        VStack(alignment: .leading) {
                            Text("Kept for \(Int(historyRetentionDays)) days").font(.caption)
                            Slider(value: $historyRetentionDays, in: 1...180, step: 1)
                        }
                    }
                }

                Section {
                    Toggle("Load the model while I speak", isOn: $prewarmOnCapture)
                    VStack(alignment: .leading) {
                        Text(unloadLabel).font(.caption)
                        Slider(value: $unloadAfterMinutes, in: 0...30, step: 1)
                    }
                } header: {
                    Text("Memory")
                } footer: {
                    Text("The model needs \(EngineProfile.profile(for: engine).residentDescription) while it is loaded. OpenFlow drops it when you stop using it, when iOS asks for memory, and when the phone gets hot. Loading it again takes a couple of seconds.")
                }

                Section {
                    Text("OpenFlow has no account, no server and no analytics. The only network request it can make is the one-time model download.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Settings")
            .sheet(isPresented: $showDiagnostics) {
                DiagnosticsView().environment(controller)
            }
            .onAppear(perform: load)
            .onChange(of: engine) { _, new in controller.settings.engine = new; commit() }
            .onChange(of: stopOnSilence) { _, new in controller.settings.stopOnSilence = new; commit() }
            .onChange(of: silenceHoldMs) { _, new in controller.settings.silenceHoldMs = Int(new); commit() }
            .onChange(of: dictionary) { _, new in controller.settings.dictionary = new; commit() }
            .onChange(of: clipboardExpirySeconds) { _, new in controller.settings.clipboardExpirySeconds = Int(new); commit() }
            .onChange(of: saveHistory) { _, new in controller.settings.saveHistory = new; commit() }
            .onChange(of: historyRetentionDays) { _, new in controller.settings.historyRetentionDays = Int(new); commit() }
            .onChange(of: unloadAfterMinutes) { _, new in controller.settings.unloadAfterMinutes = Int(new); commit() }
            .onChange(of: prewarmOnCapture) { _, new in controller.settings.prewarmOnCapture = new; commit() }
            .onChange(of: hapticOnStop) { _, new in controller.settings.hapticOnStop = new; commit() }
        }
    }

    private var residentLabel: String {
        let bytes = controller.residentBytes
        guard bytes > 0 else { return "None" }
        return ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .memory)
    }

    private var clipboardExpiryLabel: String {
        clipboardExpirySeconds <= 0
            ? "The clipboard keeps it until something replaces it"
            : "The clipboard forgets it after \(Int(clipboardExpirySeconds)) seconds"
    }

    private var unloadLabel: String {
        unloadAfterMinutes <= 0
            ? "Stay loaded while OpenFlow is open"
            : "Unload after \(Int(unloadAfterMinutes)) minutes idle"
    }

    private func load() {
        let settings = controller.settings
        engine = settings.engine
        stopOnSilence = settings.stopOnSilence
        silenceHoldMs = Double(settings.silenceHoldMs)
        dictionary = settings.dictionary
        clipboardExpirySeconds = Double(settings.clipboardExpirySeconds)
        saveHistory = settings.saveHistory
        historyRetentionDays = Double(settings.historyRetentionDays)
        unloadAfterMinutes = Double(settings.unloadAfterMinutes)
        prewarmOnCapture = settings.prewarmOnCapture
        hapticOnStop = settings.hapticOnStop
    }

    private func commit() {
        Task { await controller.applyChangedSettings() }
    }
}

/// The transition log from PLAN.md section 2, so "why was that slow" has an
/// answer that is not a guess.
struct DiagnosticsView: View {
    @Environment(DictationController.self) private var controller
    @Environment(\.dismiss) private var dismiss
    @State private var transitions: [ModelTransition] = []

    var body: some View {
        NavigationStack {
            List {
                Section("Model") {
                    LabeledContent("State", value: controller.modelState.describedForUI)
                    LabeledContent("Resident", value: "\(controller.residentBytes) bytes")
                }
                Section("Transitions") {
                    if transitions.isEmpty {
                        Text("Nothing yet.").foregroundStyle(.secondary)
                    }
                    ForEach(Array(transitions.enumerated().reversed()), id: \.offset) { _, entry in
                        VStack(alignment: .leading, spacing: 2) {
                            Text("\(entry.from.describedForUI) to \(entry.to.describedForUI)")
                            Text("\(entry.trigger.rawValue) at \(entry.wallClock.formatted(date: .omitted, time: .standard))")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .navigationTitle("Diagnostics")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) { Button("Done") { dismiss() } }
            }
            .task { transitions = await controller.transitionLog() }
        }
    }
}
