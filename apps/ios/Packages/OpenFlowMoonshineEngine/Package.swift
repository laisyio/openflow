// swift-tools-version: 6.0
import PackageDescription

// The recogniser: `SpeechEngine` on Moonshine's official Swift package.
//
// Unlike the engine stubs it replaces, this one is inside the Command Line Tools
// gate. Moonshine ships a prebuilt static library rather than a model that needs
// compiling, and its xcframework carries a `macos-arm64_x86_64` slice alongside
// the `ios-arm64` one that ships, so `swift build` and `swift test` run the real
// recogniser on this Mac with no Xcode and no Metal toolchain.
//
// The dependency is pinned by exact revision, never `from:` or `branch:`. It
// carries a `binaryTarget` whose contents are fetched by URL and checked against
// a checksum in the upstream manifest; a floating pin would let that artefact
// change under us between two builds of the same commit.
let package = Package(
    name: "OpenFlowMoonshineEngine",
    platforms: [
        .iOS(.v18),
        .macOS(.v14),
    ],
    products: [
        .library(name: "OpenFlowMoonshineEngine", targets: ["OpenFlowMoonshineEngine"])
    ],
    dependencies: [
        .package(path: "../OpenFlowMobileCore"),
        .package(
            url: "https://github.com/moonshine-ai/moonshine-swift.git",
            // v0.1.5, 2026-08-24.
            revision: "45a14f9edf1f2a6913d3aff38c1fd4e72d5b7daa"
        ),
    ],
    targets: [
        .target(
            name: "OpenFlowMoonshineEngine",
            dependencies: [
                "OpenFlowMobileCore",
                .product(name: "MoonshineVoice", package: "moonshine-swift"),
            ],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
        .testTarget(
            name: "OpenFlowMoonshineEngineTests",
            dependencies: ["OpenFlowMoonshineEngine", "OpenFlowMobileCore"],
            resources: [.copy("Resources")],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
    ]
)
