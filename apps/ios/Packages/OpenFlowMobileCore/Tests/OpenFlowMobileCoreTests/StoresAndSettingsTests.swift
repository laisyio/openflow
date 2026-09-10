import Foundation
import Testing
@testable import OpenFlowMobileCore

@Suite struct SettingsTests {
    private func makeStore() -> (SettingsStore, UserDefaults, String) {
        let suite = "io.laisy.openflow.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        return (SettingsStore(defaults: defaults), defaults, suite)
    }

    /// PLAN.md section 4, key by key. A default that drifts from the plan is a
    /// product change wearing a typo's clothes.
    @Test func testDefaultsMatchThePlan() {
        let (settings, defaults, suite) = makeStore()
        defer { defaults.removePersistentDomain(forName: suite) }

        #expect(settings.engine == .moonshineBase)
        #expect(settings.stopOnSilence == false)
        #expect(settings.silenceHoldMs == 1_200)
        #expect(settings.dictionary == "")
        #expect(settings.clipboardExpirySeconds == 60)
        #expect(settings.saveHistory == true)
        #expect(settings.historyRetentionDays == 30)
        #expect(settings.unloadAfterMinutes == 5)
        #expect(settings.prewarmOnCapture == true)
        #expect(settings.hapticOnStop == true)
        #expect(settings.onboardingComplete == false)
    }

    /// M2 removed the two engine cases M1 shipped, so an upgraded phone really
    /// does find `qwen06` sitting in the App Group. Reading it must give the
    /// default rather than crashing, and rather than leaving the app pointed at
    /// an engine it can no longer build.
    @Test func testAStoredEngineThatNoLongerParsesFallsBackToTheDefault() {
        let (settings, defaults, suite) = makeStore()
        defer { defaults.removePersistentDomain(forName: suite) }

        for stale in ["qwen06", "whisper", "", "moonshine-base", "MoonshineBase"] {
            defaults.set(stale, forKey: SettingsStore.Key.engine.rawValue)
            #expect(
                settings.engine == SettingsStore.Defaults.engine,
                "a stored \"\(stale)\" must read back as the default"
            )
            #expect(settings.engine == .moonshineBase)
        }

        // A value that does parse is still honoured, so the fallback is not just
        // ignoring what is stored.
        defaults.set(EngineChoice.moonshineTiny.rawValue, forKey: SettingsStore.Key.engine.rawValue)
        #expect(settings.engine == .moonshineTiny)

        // And a non-string under the key, which `string(forKey:)` also refuses.
        defaults.set(42, forKey: SettingsStore.Key.engine.rawValue)
        #expect(settings.engine == .moonshineBase)
    }

    /// The raw values are the on-disk format. Renaming a case is a migration,
    /// not a refactor, so the strings are written out here as well.
    @Test func testTheStoredRawValuesAreTheOnesOnDisk() {
        #expect(EngineChoice.moonshineBase.rawValue == "moonshineBase")
        #expect(EngineChoice.moonshineTiny.rawValue == "moonshineTiny")
        #expect(EngineChoice.allCases.count == 2)
        #expect(EngineChoice(rawValue: "qwen06") == nil)
        #expect(EngineChoice(rawValue: "whisper") == nil)
    }

    /// The trap this guards: `UserDefaults.bool(forKey:)` returns false for a
    /// missing key, which is the wrong answer for `saveHistory`,
    /// `prewarmOnCapture` and `hapticOnStop`. If the reader regressed to the
    /// plain accessor, these three would silently ship as off.
    @Test func testTrueByDefaultSettingsSurviveAnUnsetKey() {
        let (settings, defaults, suite) = makeStore()
        defer { defaults.removePersistentDomain(forName: suite) }
        #expect(nil == defaults.object(forKey: SettingsStore.Key.saveHistory.rawValue))
        #expect(settings.saveHistory)
        #expect(settings.prewarmOnCapture)
        #expect(settings.hapticOnStop)
    }

    @Test func testRoundTripAndClamping() {
        let (settings, defaults, suite) = makeStore()
        defer { defaults.removePersistentDomain(forName: suite) }

        settings.engine = .moonshineTiny
        settings.stopOnSilence = true
        settings.unloadAfterMinutes = 90        // clamps to the 30 minute ceiling
        settings.clipboardExpirySeconds = -5    // clamps to never
        settings.historyRetentionDays = 0       // at least one day
        settings.dictionary = "  ENTRO.LY  "

        #expect(settings.engine == .moonshineTiny)
        #expect(settings.stopOnSilence)
        #expect(settings.unloadAfterMinutes == 30)
        #expect(settings.clipboardExpirySeconds == 0)
        #expect(settings.historyRetentionDays == 1)
        #expect(settings.dictionary == "ENTRO.LY")

        #expect(settings.modelPolicy.unloadAfterMinutes == 30)
        settings.resetAll()
        #expect(settings.unloadAfterMinutes == 5)
    }
}

@Suite struct TranscriptStoreTests {

    @Test func testLastTranscriptRoundTrips() throws {
        let store = TranscriptStore(directory: temporaryDirectory())
        #expect(nil == store.lastEntry())
        let record = TranscriptRecord(text: "hello phone", durationSeconds: 2.5, engine: "fake")
        try store.saveLast(record)
        #expect(store.lastEntry() == record)
    }

    /// The retention window from PLAN.md section 4, with `now` injected so the
    /// test does not have to be 31 days old.
    @Test func testRetentionDropsEntriesPastTheWindow() throws {
        let store = TranscriptStore(directory: temporaryDirectory())
        let now = Date()
        let fresh = TranscriptRecord(text: "today", createdAt: now.addingTimeInterval(-3_600))
        let edge = TranscriptRecord(text: "29 days ago", createdAt: now.addingTimeInterval(-29 * 86_400))
        let stale = TranscriptRecord(text: "31 days ago", createdAt: now.addingTimeInterval(-31 * 86_400))

        try store.append(stale, retentionDays: 30, now: now)
        try store.append(edge, retentionDays: 30, now: now)
        let kept = try store.append(fresh, retentionDays: 30, now: now)

        #expect(kept.map(\.text) == ["29 days ago", "today"])
        #expect(store.loadHistory().count == 2)

        // Lowering the setting prunes without waiting for a new dictation.
        let tightened = try store.prune(retentionDays: 1, now: now)
        #expect(tightened.map(\.text) == ["today"])
    }

    /// Retention bounds the file in days, which is not a bound at all for
    /// somebody who dictates all day: the cap is what turns it into one. The
    /// records that fall off are the oldest, the same end retention takes.
    @Test func testHistoryIsCappedAtItsDocumentedCeiling() throws {
        let store = TranscriptStore(directory: temporaryDirectory())
        let now = Date()
        let overflowing = TranscriptStore.maxEntries + 25
        let records = (0..<overflowing).map { index in
            TranscriptRecord(
                text: "take \(index)",
                createdAt: now.addingTimeInterval(Double(index) - Double(overflowing))
            )
        }
        try store.append(records[0], retentionDays: 30, now: now)
        let kept = TranscriptStore.pruned(records, retentionDays: 30, now: now)

        #expect(kept.count == TranscriptStore.maxEntries)
        #expect(kept.first?.text == "take 25", "the oldest are what the cap drops")
        #expect(kept.last?.text == "take \(overflowing - 1)", "the newest take survives")
    }

    /// The cap has to be capable of doing nothing. A history one record short of
    /// it must come back whole, or the assertion above would pass on a rule that
    /// truncates everything.
    @Test func testTheCapLeavesASmallerHistoryAlone() throws {
        let now = Date()
        let records = (0..<(TranscriptStore.maxEntries - 1)).map { index in
            TranscriptRecord(text: "take \(index)", createdAt: now.addingTimeInterval(Double(index) - 86_400))
        }
        let kept = TranscriptStore.pruned(records, retentionDays: 30, now: now)
        #expect(kept.count == TranscriptStore.maxEntries - 1)
        #expect(kept.first?.text == "take 0")
    }

    /// `lastEntry()` is the cheap path: it must answer from `last.json` alone,
    /// so a history file that is missing, or full of something else entirely,
    /// changes nothing about what it returns.
    @Test func testLastEntryNeverReadsTheHistoryFile() throws {
        let directory = temporaryDirectory()
        let store = TranscriptStore(directory: directory)
        let newest = TranscriptRecord(text: "the one the keyboard inserts")
        try store.append(TranscriptRecord(text: "older"), retentionDays: 30)
        try store.saveLast(newest)
        #expect(store.lastEntry() == newest)

        try Data("not json at all".utf8).write(to: directory.appendingPathComponent("history.json"))
        #expect(store.lastEntry() == newest, "a broken history file is not this call's problem")
        #expect(store.loadHistory().isEmpty, "and the list read is the one that has to cope with it")
    }

    @Test func testDeleteOneAndDeleteAll() throws {
        let store = TranscriptStore(directory: temporaryDirectory())
        let first = TranscriptRecord(text: "one")
        let second = TranscriptRecord(text: "two")
        try store.append(first, retentionDays: 30)
        try store.append(second, retentionDays: 30)
        try store.saveLast(second)

        try store.delete(id: first.id)
        #expect(store.loadHistory().map(\.text) == ["two"])

        try store.deleteAll()
        #expect(store.loadHistory().isEmpty)
        #expect(nil == store.lastEntry())
    }

    /// The keyboard extension has to tell "nothing dictated yet" apart from
    /// "the sandbox will not let me look", because only one of them is the
    /// user's to fix. `lastEntry()` collapses both to nil.
    @Test func testReadLastSeparatesAnEmptyStoreFromAnUnreadableOne() throws {
        let directory = temporaryDirectory()
        let store = TranscriptStore(directory: directory)
        #expect(store.readLast() == .none)

        let record = TranscriptRecord(text: "hello phone")
        try store.saveLast(record)
        #expect(store.readLast() == .record(record))

        // Make the file genuinely unreadable, the way the keyboard sandbox does.
        let file = directory.appendingPathComponent("last.json")
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: file.path)
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: file.path)
        }

        // Running as root would make the chmod meaningless and the assertion a
        // lie, so say so rather than passing for the wrong reason.
        guard getuid() != 0 else {
            Issue.record("test must not run as root; chmod 000 would still be readable")
            return
        }
        #expect(store.readLast() == .unreadable)
        #expect(store.lastEntry() == nil, "lastEntry still collapses it, which is why readLast exists")
    }

    @Test func testHistoryIsReadableFromASecondHandleOnTheSameDirectory() throws {
        // The keyboard extension is a different process reading the same files.
        let directory = temporaryDirectory()
        let writer = TranscriptStore(directory: directory)
        try writer.saveLast(TranscriptRecord(text: "from the app"))
        let reader = TranscriptStore(directory: directory)
        #expect(reader.lastEntry()?.text == "from the app")
    }
}

@Suite struct ModelStoreTests {

    @Test func testPrepareCreatesTheDirectoryAndExcludesItFromBackup() throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        try store.prepare()
        #expect(FileManager.default.fileExists(atPath: store.directory.path))
        #expect(store.isExcludedFromBackup(), "141 MB of re-downloadable weights must not enter a backup")
        try store.prepare()   // idempotent
    }

    @Test func testChecksumVerificationAcceptsTheRightFileAndRejectsATamperedOne() throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        try store.prepare()
        let payload = Data("the quick brown fox".utf8)
        let file = store.url(for: "weights.bin")
        try payload.write(to: file)

        // Known-answer test, so a wrong hashing routine cannot agree with itself.
        let expected = "9ecb36561341d18eb65484e833efea61edc74b84cf5e6ae1b81c63533e25fc8f"
        #expect(try ModelStore.sha256Hex(ofFileAt: file) == expected)
        try store.verify("weights.bin", sha256Hex: expected)

        try Data("the quick brown fix".utf8).write(to: file)
        do {
            try store.verify("weights.bin", sha256Hex: expected)
            Issue.record("a tampered file must not verify")
        } catch let error as ModelStore.StoreError {
            guard case .checksumMismatch = error else {
                Issue.record("expected a checksum mismatch, got \(error)")
                return
            }
        }
    }

    /// The chunked reader must produce the same digest as a single-shot one, or
    /// a 109 MB decoder would verify differently from a test fixture.
    @Test func testChunkedHashingMatchesWholeFileHashing() throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        try store.prepare()
        let file = store.url(for: "big.bin")
        var bytes = Data()
        for index in 0..<200_000 { bytes.append(UInt8(index % 251)) }
        try bytes.write(to: file)

        let chunked = try ModelStore.sha256Hex(ofFileAt: file, chunkBytes: 4_096)
        let single = try ModelStore.sha256Hex(ofFileAt: file, chunkBytes: 1 << 24)
        #expect(chunked == single)
    }

    @Test func testInstallReplacesAnExistingFile() throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        try store.prepare()
        try Data("old".utf8).write(to: store.url(for: "weights.bin"))

        let staging = temporaryDirectory().appendingPathComponent("download.tmp")
        try Data("new".utf8).write(to: staging)
        try store.install(from: staging, as: "weights.bin")

        #expect(try Data(contentsOf: store.url(for: "weights.bin")) == Data("new".utf8))
        #expect(store.sizeOnDisk("weights.bin") > 0)
        try store.remove("weights.bin")
        #expect(!store.exists("weights.bin"))
    }

    /// A model that is a set of files gets its own directory, and the flag that
    /// keeps 141 MB out of iCloud is set once at the top and inherited.
    @Test func testASubdirectoryIsRootedUnderTheStoreAndPreparesItsParents() throws {
        let store = ModelStore(directory: temporaryDirectory().appendingPathComponent("Models"))
        try store.prepare()
        let engine = store.subdirectory("moonshine/base-en")
        #expect(engine.directory.path.hasPrefix(store.directory.path))

        // Two levels down, and neither exists yet.
        try engine.prepare()
        #expect(FileManager.default.fileExists(atPath: engine.directory.path))

        try Data("weights".utf8).write(to: engine.url(for: "encoder_model.ort"))
        #expect(engine.exists("encoder_model.ort"))
        #expect(!store.exists("encoder_model.ort"), "the file belongs to the engine, not the store")

        try engine.removeAll()
        #expect(!FileManager.default.fileExists(atPath: engine.directory.path))
        #expect(FileManager.default.fileExists(atPath: store.directory.path), "removing one engine keeps the store")
    }
}

@Suite struct ClipboardWriterTests {
    @Test func testNoopWriterSatisfiesTheProtocol() {
        let writer: any ClipboardWriter = NoopClipboardWriter()
        writer.write("nothing happens", localOnly: true, expiresAfter: 60)
    }
}
