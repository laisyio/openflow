// swift-tools-version: 6.0
import PackageDescription

// OpenFlowMobileCore is the whole brain of the phone app: the load/unload state
// machine, the audio maths, the dictionary post-pass and the stores. It has no
// dependencies at all, so it builds and tests with the Command Line Tools alone
// (no Xcode, no Metal toolchain) on the macOS host as well as on iOS.
//
// The engine package (OpenFlowMoonshineEngine) is NOT listed here on purpose:
// the dependency runs the other way. The engine depends on this package for the
// `SpeechEngine` seam it implements, and this package stays dependency-free so
// the brain builds and tests even where the engine's binary target cannot.
let package = Package(
    name: "OpenFlowMobileCore",
    platforms: [
        .iOS(.v18),
        .macOS(.v14),
    ],
    products: [
        .library(name: "OpenFlowMobileCore", targets: ["OpenFlowMobileCore"])
    ],
    targets: [
        .target(
            name: "OpenFlowMobileCore",
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
        .testTarget(
            name: "OpenFlowMobileCoreTests",
            dependencies: ["OpenFlowMobileCore"],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
    ]
)
