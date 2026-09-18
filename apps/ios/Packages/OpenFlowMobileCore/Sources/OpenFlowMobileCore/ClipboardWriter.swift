import Foundation

#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

/// Where a finished transcript goes so another app can receive it.
///
/// iOS gives a third-party app no way to type into another app (PLAN.md section
/// 0), so the clipboard is one of the three routes text can take, alongside the
/// share sheet and our own keyboard extension.
public protocol ClipboardWriter: Sendable {
    /// - Parameters:
    ///   - localOnly: keeps the item off Universal Clipboard, so a dictation
    ///     never appears on the user's other devices. Always true for us.
    ///   - expiresAfter: seconds until the item is dropped, or nil for never.
    @MainActor func write(_ text: String, localOnly: Bool, expiresAfter: TimeInterval?)
}

#if canImport(UIKit)

/// `UIPasteboard` with `localOnly` and an expiry, per PLAN.md section 4's
/// `clipboardExpirySeconds` (default 60, 0 = never).
public struct SystemClipboardWriter: ClipboardWriter {
    public init() {}

    public func write(_ text: String, localOnly: Bool = true, expiresAfter: TimeInterval? = 60) {
        var options: [UIPasteboard.OptionsKey: Any] = [.localOnly: localOnly]
        if let expiresAfter, expiresAfter > 0 {
            options[.expirationDate] = Date().addingTimeInterval(expiresAfter)
        }
        UIPasteboard.general.setItems([[UTType.plainTextIdentifier: text]], options: options)
    }
}

/// Spelled out rather than importing UniformTypeIdentifiers for one string.
private enum UTType {
    static let plainTextIdentifier = "public.utf8-plain-text"
}

#elseif canImport(AppKit)

/// The macOS host build. `NSPasteboard` has no localOnly or expiry, so this
/// exists to keep the package building and testing on this machine, not because
/// the Mac is a shipping target.
public struct SystemClipboardWriter: ClipboardWriter {
    public init() {}

    public func write(_ text: String, localOnly: Bool = true, expiresAfter: TimeInterval? = 60) {
        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }
}

#else

public struct SystemClipboardWriter: ClipboardWriter {
    public init() {}
    public func write(_ text: String, localOnly: Bool = true, expiresAfter: TimeInterval? = 60) {}
}

#endif

/// Writes nowhere. Used by previews and by any test that must not touch the
/// machine's real pasteboard.
public struct NoopClipboardWriter: ClipboardWriter {
    public init() {}
    public func write(_ text: String, localOnly: Bool, expiresAfter: TimeInterval?) {}
}
