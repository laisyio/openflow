import Foundation

/// The dictionary, applied after recognition instead of before it.
///
/// On the desktop the dictionary is a Whisper `prompt` (see `dictionary_prompt`
/// in `src-tauri/src/transcribe.rs`): the model is told the spellings and gets
/// them right itself. Moonshine has no prompt, and its key-term biasing works
/// only on the streaming architectures this app does not load, so on the phone
/// the same string drives a deterministic replacement over the finished text.
/// base-en writes "enterol I" for "entro.ly" and "fast pay" for "FastPay"; this
/// is what fixes both.
///
/// ## The format
///
/// Exactly the desktop's: one free-text field, trimmed, capped at 800 Unicode
/// scalars, entries separated by commas or newlines. `capped(_:)` reproduces
/// `dictionary_prompt` so the two stay interchangeable -- the same string is the
/// desktop's prompt, this post-pass, and the key terms the engine is offered,
/// and none of the three sees something the others did not.
///
/// Two entry shapes:
/// - `Term` -- match `Term` case-insensitively, rewrite it with this spelling.
///   `ENTRO.LY` fixes `entro.ly`, `Entro.Ly` and `ENTRO.ly`.
/// - `heard -> Term` (or `=>`) -- match the left side, write the right side.
///   This is what catches a mishearing the model has no way to spell correctly,
///   e.g. `intro dot lie -> ENTRO.LY`. The desktop's prompt form cannot express
///   this, so a dictionary using it stays valid as a prompt (the arrow is just
///   more prompt text) while doing strictly more here.
///
/// ## The matching rules
///
/// 1. **Whole-word only.** A match must not be flanked by a letter or a digit on
///    either side. `Sop` does not fire inside `Sopranos`. Punctuation inside an
///    entry (`ENTRO.LY`) is part of the entry, not a boundary.
/// 2. **Longest entry first.** Entries are sorted by match length descending, so
///    `ENTRO.LY affiliate` wins over `ENTRO.LY`, and `ENTRO.LY` over `ENTRO`.
///    Ties break on the order the user typed them, so the result never depends
///    on dictionary iteration order.
/// 3. **Case-insensitive match, dictionary casing wins.** The entry decides how
///    the term is spelled; that is the entire point of the feature.
/// 4. **Sentence-initial capitalisation is preserved, but only for an entry that
///    is entirely lower-case.** If the match starts the text, or follows `.`,
///    `!`, `?`, `:` or a newline plus whitespace, the first character of the
///    replacement is upper-cased -- so a dictionary entry of `openflow` becomes
///    `Openflow` there and stays `openflow` mid-sentence.
///
///    An entry carrying any capital of its own is left exactly as written. That
///    capital is the reason the entry exists: `iPhone`, `eBay`, `macOS` and
///    `ENTRO.LY` are spellings the user typed deliberately, and upper-casing
///    their first letter produces `IPhone` and `EBay`, which is worse than the
///    mistake the dictionary was added to fix. Rule 3 says the dictionary's
///    casing wins; this rule only fills in a decision the entry did not make.
/// 5. **One left-to-right pass, no rescanning.** Replaced spans are never
///    matched again, so `a -> b` and `b -> c` cannot chain. The output is a pure
///    function of (text, dictionary).
public enum DictionaryPostPass {
    /// The desktop's `dictionary_prompt`: trim, drop if empty, keep the first
    /// 800 Unicode scalars.
    ///
    /// `String.prefix` counts grapheme clusters, which is not what Rust's
    /// `chars().take(800)` counts, so this walks `unicodeScalars` to keep the
    /// two caps identical for every input.
    public static func capped(_ dictionary: String?, limit: Int = 800) -> String? {
        guard let dictionary else { return nil }
        let text = dictionary.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty { return nil }
        let scalars = text.unicodeScalars
        if scalars.count <= limit { return text }
        return String(String.UnicodeScalarView(scalars.prefix(limit)))
    }

    /// One parsed rule.
    public struct Entry: Sendable, Equatable {
        /// What to look for, lower-cased for matching.
        public let matchLowercased: String
        /// What to write instead.
        public let replacement: String
    }

    /// Parse the dictionary field into rules, in the order they will be applied.
    public static func entries(from dictionary: String?) -> [Entry] {
        guard let text = capped(dictionary) else { return [] }
        var seen = Set<String>()
        var parsed: [Entry] = []
        let pieces = text.split(whereSeparator: { $0 == "," || $0 == "\n" || $0 == ";" })
        for piece in pieces {
            let raw = piece.trimmingCharacters(in: .whitespacesAndNewlines)
            if raw.isEmpty { continue }
            var match = raw
            var replacement = raw
            for arrow in ["->", "=>"] where raw.contains(arrow) {
                let halves = raw.components(separatedBy: arrow)
                if halves.count == 2 {
                    let left = halves[0].trimmingCharacters(in: .whitespacesAndNewlines)
                    let right = halves[1].trimmingCharacters(in: .whitespacesAndNewlines)
                    if !left.isEmpty && !right.isEmpty {
                        match = left
                        replacement = right
                    }
                }
                break
            }
            let key = match.lowercased()
            if key.isEmpty || seen.contains(key) { continue }
            seen.insert(key)
            parsed.append(Entry(matchLowercased: key, replacement: replacement))
        }
        // Rule 2: longest first, stable within a length so ties keep typed order.
        return parsed.enumerated()
            .sorted { lhs, rhs in
                if lhs.element.matchLowercased.count != rhs.element.matchLowercased.count {
                    return lhs.element.matchLowercased.count > rhs.element.matchLowercased.count
                }
                return lhs.offset < rhs.offset
            }
            .map(\.element)
    }

    /// Apply the dictionary to a finished transcript.
    public static func apply(_ text: String, dictionary: String?) -> String {
        apply(text, entries: entries(from: dictionary))
    }

    /// Apply already-parsed rules. Separated so a caller that transcribes in a
    /// loop parses the dictionary once.
    public static func apply(_ text: String, entries: [Entry]) -> String {
        guard !entries.isEmpty, !text.isEmpty else { return text }
        let source = Array(text)
        let lowered = Array(text.lowercased())
        // `lowercased()` can change length for a few scripts (e.g. the Turkish
        // dotted capital I). When it does, matching by index is unsound, so fall
        // back to leaving the text alone rather than corrupting it.
        guard lowered.count == source.count else { return text }

        var output = String()
        output.reserveCapacity(text.count)
        var index = 0
        while index < source.count {
            var matched = false
            for entry in entries {
                let needle = Array(entry.matchLowercased)
                guard !needle.isEmpty, index + needle.count <= lowered.count else { continue }
                if !Array(lowered[index..<(index + needle.count)]).elementsEqual(needle) { continue }
                // Rule 1: whole-word only.
                let before = index > 0 ? source[index - 1] : nil
                let after = index + needle.count < source.count ? source[index + needle.count] : nil
                if isWordCharacter(before) || isWordCharacter(after) { continue }
                // Rule 4: sentence-initial capitalisation, for entries that
                // expressed no capitalisation of their own.
                let replacement = startsSentence(output)
                    ? sentenceCased(entry.replacement)
                    : entry.replacement
                output.append(replacement)
                index += needle.count
                matched = true
                break
            }
            if !matched {
                output.append(source[index])
                index += 1
            }
        }
        return output
    }

    private static func isWordCharacter(_ character: Character?) -> Bool {
        guard let character else { return false }
        return character.isLetter || character.isNumber
    }

    /// True when nothing but sentence-ending punctuation and whitespace precedes
    /// the cursor.
    private static func startsSentence(_ emitted: String) -> Bool {
        var sawWhitespace = false
        for character in emitted.reversed() {
            if character.isNewline { return true }
            if character.isWhitespace {
                sawWhitespace = true
                continue
            }
            if ".!?:".contains(character) { return sawWhitespace }
            return false
        }
        return true
    }

    /// Upper-cases the first character, but only for an entry that is entirely
    /// lower-case. `openflow` becomes `Openflow`; `iPhone`, `eBay` and
    /// `ENTRO.LY` are returned untouched, because their capitals are the point.
    private static func sentenceCased(_ value: String) -> String {
        guard let first = value.first, first.isLowercase else { return value }
        guard !value.contains(where: { $0.isUppercase }) else { return value }
        return String(first).uppercased() + value.dropFirst()
    }
}
