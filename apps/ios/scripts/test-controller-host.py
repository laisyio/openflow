#!/usr/bin/env python3
"""Run the real controller regressions with macOS Command Line Tools.

The temporary SwiftPM package copies current app/controller test sources and
depends on the current MobileCore package. Only the iOS Activity/Intent bridge
types are stubbed; this does not exercise UIKit, permissions or a real model.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


PLATFORM_STUBS = """import Foundation
struct DictationActivityAttributes {
    enum Stage { case recording, transcribing, idle }
}
@MainActor
final class DictationIntentBridge {
    static let shared = DictationIntentBridge()
    func register(_ handler: @escaping @MainActor () async -> Void) {}
}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep-temp", action="store_true", help="Keep the generated package and build output for inspection")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("This host gate requires macOS and Swift 6 Command Line Tools")
    swift = shutil.which("swift")
    if swift is None:
        parser.error("swift was not found; install Swift 6 Command Line Tools or Xcode")

    ios = Path(__file__).resolve().parents[1]
    package = Path(tempfile.mkdtemp(prefix="openflow-controller-host-"))
    try:
        source = package / "Sources/OpenFlow"
        tests = package / "Tests/OpenFlowTests"
        source.mkdir(parents=True)
        tests.mkdir(parents=True)
        shutil.copy2(ios / "OpenFlow/DictationController.swift", source)
        for test in sorted((ios / "OpenFlowAppTests").glob("*.swift")):
            shutil.copy2(test, tests)
        (source / "PlatformStubs.swift").write_text(PLATFORM_STUBS)
        # JSON string quoting is also valid for this filesystem path in Swift.
        core_path = json.dumps(str(ios / "Packages/OpenFlowMobileCore"), ensure_ascii=False)
        (package / "Package.swift").write_text("""// swift-tools-version: 6.0
import PackageDescription
let package = Package(
    name: "OpenFlowControllerHarness",
    platforms: [.macOS(.v14)],
    dependencies: [.package(path: CORE_PATH)],
    targets: [
        .target(name: "OpenFlow",
                dependencies: [.product(name: "OpenFlowMobileCore", package: "OpenFlowMobileCore")],
                swiftSettings: [.define("OPENFLOW_FAKE_ENGINE")]),
        .testTarget(name: "OpenFlowTests",
                    dependencies: ["OpenFlow", .product(name: "OpenFlowMobileCore", package: "OpenFlowMobileCore")])
    ]
)
""".replace("CORE_PATH", core_path))
        print(f"Temporary controller package: {package}", flush=True)
        return subprocess.run([swift, "test", "--package-path", str(package)], check=False).returncode
    finally:
        if args.keep_temp:
            print(f"Kept generated package: {package}", flush=True)
        else:
            shutil.rmtree(package)


if __name__ == "__main__":
    raise SystemExit(main())
