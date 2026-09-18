import SwiftUI
import UIKit
import OpenFlowMobileCore

/// OpenFlow for iPhone. No account, no server, no analytics: the only network
/// request the app can make is the one-time model download, and it lives in one
/// file (`ModelDownloader`) so a reviewer can check that claim with grep.
@main
struct OpenFlowApp: App {
    @State private var controller = DictationController.shared
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(controller)
                .task {
                    await controller.refresh()
                }
                .sheet(isPresented: $controller.isCaptureSheetPresented, onDismiss: {
                    Task { await controller.cancelRecording() }
                }) {
                    CaptureSheet()
                        .environment(controller)
                        .presentationDetents([.medium])
                }
                // The keyboard extension's "Open OpenFlow" button. An extension
                // can only reach its host app through a URL.
                .onOpenURL { url in
                    guard url.scheme == "openflow" else { return }
                    Task {
                        await controller.prewarm(trigger: .intentPrewarm)
                        controller.isCaptureSheetPresented = true
                    }
                }
        }
        .onChange(of: scenePhase) { _, phase in
            // Only `.background` arms the unload and only `.active` cancels it.
            // `.inactive` -- app switcher, Control Centre, a call banner -- is
            // neither, and must not start the 20 s clock.
            let mapped: DictationController.LifecyclePhase
            switch phase {
            case .active: mapped = .active
            case .background: mapped = .background
            case .inactive: mapped = .inactive
            @unknown default: mapped = .inactive
            }
            Task { await controller.handleScenePhase(mapped) }
        }
    }
}

struct RootView: View {
    @Environment(DictationController.self) private var controller

    var body: some View {
        TabView {
            DictateTab()
                .tabItem { Label("Dictate", systemImage: "mic") }
            HistoryView()
                .tabItem { Label("History", systemImage: "clock") }
            SettingsView()
                .tabItem { Label("Settings", systemImage: "gearshape") }
        }
        // The two OS events the state machine in PLAN.md section 2 reacts to.
        .onReceive(NotificationCenter.default.publisher(for: UIApplication.didReceiveMemoryWarningNotification)) { _ in
            Task { await controller.handleMemoryWarning() }
        }
        .onReceive(NotificationCenter.default.publisher(for: ProcessInfo.thermalStateDidChangeNotification)) { _ in
            Task { await controller.handleThermalChange() }
        }
    }
}

/// The whole app in one button. Everything else is settings and history.
struct DictateTab: View {
    @Environment(DictationController.self) private var controller
    @State private var showDownload = false

    var body: some View {
        NavigationStack {
            VStack(spacing: 28) {
                Spacer()
                Text("Your voice never leaves this phone.")
                    .font(.title3)
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 32)

                Button {
                    Task {
                        await controller.prewarm(trigger: .intentPrewarm)
                        controller.isCaptureSheetPresented = true
                    }
                } label: {
                    Label("Start dictation", systemImage: "mic.fill")
                        .font(.headline)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 14)
                }
                .buttonStyle(.borderedProminent)
                .padding(.horizontal, 32)

                if case .failed(let reason) = controller.phase {
                    Text(reason)
                        .font(.footnote)
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 32)
                }
                Spacer()
            }
            .navigationTitle("OpenFlow")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Model") { showDownload = true }
                }
            }
            .sheet(isPresented: $showDownload) {
                ModelDownloadView()
                    .environment(controller)
            }
        }
    }
}
