import test from "node:test";
import assert from "node:assert/strict";
import { SpeechPlayback } from "../src/ttsPlayback.js";

function fixture(limit, audio = null, completionTimeoutMs, drainTimeoutMs) {
  const states = [], errors = [], cancelled = [], urls = [];
  const player = new SpeechPlayback({ audio: () => audio, url: u => urls.push(u),
    state: s => states.push(s), error: e => errors.push(e), cancel: id => cancelled.push(id) }, limit, completionTimeoutMs, drainTimeoutMs);
  const chunk = (id, n, text) => player.chunk({ request_id: id, sequence: n, data_base64: btoa(text) });
  return { player, states, errors, cancelled, urls, chunk };
}
test("replacement rejects late chunks, errors and completions", () => {
  const f = fixture(); f.player.start("old", "audio/wav"); f.chunk("old", 0, "ab");
  f.player.start("new", "audio/wav"); f.chunk("old", 1, "late");
  f.player.finish({ request_id: "old", bytes: 6 }); f.player.fail("old", "old error");
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, "new");
  assert.deepEqual(f.cancelled, ["old"]); f.player.dispose();
});

// Deterministic browser lifecycle double. Appending stays asynchronous until
// update() is called, so tests can interleave terminal events and disposal.
function mediaFixture(t, completionTimeoutMs, drainTimeoutMs) {
  class TrackedTarget extends EventTarget {
    listeners = new Map();
    addEventListener(name, fn, options) {
      if (!this.listeners.has(name)) this.listeners.set(name, new Set());
      this.listeners.get(name).add(fn);
      super.addEventListener(name, fn, options);
    }
    removeEventListener(name, fn, options) {
      this.listeners.get(name)?.delete(fn);
      super.removeEventListener(name, fn, options);
    }
    listenerCount() { return [...this.listeners.values()].reduce((n, fns) => n + fns.size, 0); }
  }
  class Buffer extends TrackedTarget {
    updating = false;
    buffered = { length: 0 };
    appended = [];
    aborts = 0;
    throwOnAppend = false;
    appendBuffer(bytes) {
      if (this.throwOnAppend) throw new Error("append failed");
      assert.equal(this.updating, false, "cannot append while an update is pending");
      this.appended.push(new TextDecoder().decode(bytes));
      this.updating = true;
    }
    update() {
      this.updating = false; this.buffered.length = 1;
      this.dispatchEvent(new Event("updateend"));
    }
    abort() { this.aborts++; this.updating = false; this.dispatchEvent(new Event("updateend")); }
  }
  const sources = [], objects = new Map(), revoked = [];
  class Source extends TrackedTarget {
    static isTypeSupported() { return true; }
    readyState = "closed";
    buffer = new Buffer();
    ends = 0;
    constructor() { super(); sources.push(this); }
    open() { this.readyState = "open"; this.dispatchEvent(new Event("sourceopen")); }
    addSourceBuffer() { return this.buffer; }
    endOfStream() {
      assert.equal(this.buffer.updating, false);
      this.ends++; this.readyState = "ended";
    }
  }
  const original = Object.getOwnPropertyDescriptor(globalThis, "MediaSource");
  Object.defineProperty(globalThis, "MediaSource", { configurable: true, writable: true, value: Source });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "MediaSource", original);
    else delete globalThis.MediaSource;
  });
  t.mock.method(URL, "createObjectURL", value => {
    const url = `blob:test-${objects.size}`; objects.set(url, value); return url;
  });
  t.mock.method(URL, "revokeObjectURL", url => revoked.push(url));
  const audio = { plays: 0, pauses: 0,
    play() { this.plays++; return Promise.resolve(); }, pause() { this.pauses++; } };
  return { ...fixture(undefined, audio, completionTimeoutMs, drainTimeoutMs), sources, objects, revoked, audio };
}

test("completed MSE with a stalled append releases its chunks to Blob replay", async t => {
  const f = mediaFixture(t, 100, 5); t.after(() => f.player.dispose());
  f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1); source.open();
  f.chunk("a", 0, "ab"); f.chunk("a", 1, "cd");
  f.player.finish({ request_id: "a", bytes: 4 });
  assert.equal(source.buffer.updating, true);
  await new Promise(resolve => setTimeout(resolve, 15));
  const blobUrl = f.urls.at(-1), blob = f.objects.get(blobUrl);
  assert.ok(blob instanceof Blob); assert.equal(await blob.text(), "abcd");
  assert.equal(source.buffer.aborts, 1); assert.equal(source.ends, 0);
  assert.equal(source.listenerCount(), 0); assert.equal(source.buffer.listenerCount(), 0);
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.states.at(-1), "ready");
  assert.deepEqual(f.revoked, [streamUrl]);
  source.buffer.update(); assert.equal(f.urls.at(-1), blobUrl);
});

test("completed MSE drain deadline resets on each real append completion", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const f = mediaFixture(t, 100, 5); t.after(() => f.player.dispose());
  f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1); source.open();
  f.chunk("a", 0, "ab"); f.chunk("a", 1, "cd"); f.chunk("a", 2, "ef");
  f.player.finish({ request_id: "a", bytes: 6 });
  for (let append = 0; append < 3; append++) {
    t.mock.timers.tick(4);
    assert.equal(source.buffer.aborts, 0, "recent progress keeps draining alive");
    assert.equal(f.urls.at(-1), streamUrl);
    source.buffer.update();
  }
  assert.equal(source.ends, 1); assert.equal(f.player.retainedBytes, 0);
  t.mock.timers.tick(100);
  assert.equal(f.urls.at(-1), streamUrl); assert.deepEqual(f.revoked, []);
});

test("spurious updateend during a pending append does not extend its drain deadline", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const f = mediaFixture(t, 100, 5); t.after(() => f.player.dispose());
  f.player.start("a", "audio/mpeg");
  const source = f.sources[0]; source.open();
  f.chunk("a", 0, "ab"); f.player.finish({ request_id: "a", bytes: 2 });
  t.mock.timers.tick(4); source.buffer.dispatchEvent(new Event("updateend"));
  t.mock.timers.tick(1);
  assert.ok(f.objects.get(f.urls.at(-1)) instanceof Blob);
  assert.equal(source.buffer.aborts, 1); assert.equal(f.player.retainedBytes, 0);
});

test("replacing completed MSE during drain clears its deadline and stale callbacks", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const f = mediaFixture(t, 100, 5); t.after(() => f.player.dispose());
  f.player.start("old", "audio/mpeg");
  const source = f.sources[0], oldUrl = f.urls.at(-1); source.open();
  f.chunk("old", 0, "ab"); f.player.finish({ request_id: "old", bytes: 2 });
  const stale = [...source.buffer.listeners.get("updateend")];
  f.player.start("new", "audio/mpeg"); const newUrl = f.urls.at(-1);
  t.mock.timers.tick(100); for (const callback of stale) callback();
  assert.equal(source.buffer.aborts, 1); assert.equal(source.listenerCount(), 0);
  assert.equal(f.player.requestId, "new"); assert.equal(f.urls.at(-1), newUrl);
  assert.deepEqual(f.revoked, [oldUrl]); assert.deepEqual(f.cancelled, []);
});

test("completed MSE that never opens releases its chunks to Blob replay", async t => {
  const f = mediaFixture(t, 5); f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1);
  f.chunk("a", 0, "ab"); f.chunk("a", 1, "cd");
  f.player.finish({ request_id: "a", bytes: 4 });
  assert.equal(f.player.retainedBytes, 4);
  await new Promise(resolve => setTimeout(resolve, 15));
  const blobUrl = f.urls.at(-1), blob = f.objects.get(blobUrl);
  assert.ok(blob instanceof Blob); assert.equal(await blob.text(), "abcd");
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.states.at(-1), "ready");
  assert.equal(source.listenerCount(), 0); assert.deepEqual(f.revoked, [streamUrl]);
  source.open();
  assert.deepEqual(source.buffer.appended, [], "late sourceopen is detached");
  assert.equal(f.urls.at(-1), blobUrl); f.player.dispose();
});

test("MSE that opens before its deadline retains the completed streaming replay", async t => {
  const f = mediaFixture(t, 5); f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1);
  f.chunk("a", 0, "ab"); f.player.finish({ request_id: "a", bytes: 2 });
  source.open(); source.buffer.update();
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(source.ends, 1); assert.equal(f.urls.at(-1), streamUrl);
  assert.equal(f.player.retainedBytes, 0); assert.deepEqual(f.revoked, []);
  f.player.dispose();
});

test("the unopened-source deadline does not abort a stream that has since opened", async t => {
  const f = mediaFixture(t, 5); f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1);
  f.chunk("a", 0, "ab"); f.player.finish({ request_id: "a", bytes: 2 });
  source.open();
  assert.equal(source.buffer.updating, true);
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(source.buffer.aborts, 0, "an active append is not an unattached source");
  assert.equal(f.urls.at(-1), streamUrl); assert.deepEqual(f.revoked, []);
  source.buffer.update(); assert.equal(source.ends, 1);
  assert.equal(f.player.retainedBytes, 0); f.player.dispose();
});

test("replacing unopened completed MSE cancels its fallback deadline", async t => {
  const f = mediaFixture(t, 5); f.player.start("old", "audio/mpeg");
  const source = f.sources[0], oldUrl = f.urls.at(-1);
  f.chunk("old", 0, "ab"); f.player.finish({ request_id: "old", bytes: 2 });
  f.player.start("new", "audio/mpeg"); const newUrl = f.urls.at(-1);
  await new Promise(resolve => setTimeout(resolve, 15));
  source.open();
  assert.equal(f.player.requestId, "new"); assert.equal(f.urls.at(-1), newUrl);
  assert.equal(f.states.at(-1), "streaming"); assert.deepEqual(f.revoked, [oldUrl]);
  assert.equal(f.player.retainedBytes, 0); f.player.dispose();
});

test("MSE completion before sourceopen drains in order and keeps only replay URL", t => {
  const f = mediaFixture(t);
  f.player.start("a", "audio/mpeg");
  f.chunk("a", 1, "cd"); f.chunk("a", 0, "ab");
  f.player.finish({ request_id: "a", bytes: 4 });
  const source = f.sources[0], url = f.urls.at(-1);
  assert.equal(f.player.retainedBytes, 4);
  source.open();
  assert.deepEqual(source.buffer.appended, ["ab"]);
  source.buffer.update();
  assert.deepEqual(source.buffer.appended, ["ab", "cd"]);
  assert.equal(source.ends, 0);
  source.buffer.update();
  assert.equal(source.ends, 1); assert.equal(f.audio.plays, 1);
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, null);
  assert.equal(source.listenerCount(), 0); assert.equal(source.buffer.listenerCount(), 0);
  assert.equal(f.urls.at(-1), url); assert.deepEqual(f.revoked, []);
  f.player.finish({ request_id: "a", bytes: 4 }); source.buffer.update();
  assert.equal(source.ends, 1); assert.equal(f.states.filter(s => s === "ready").length, 1);
  f.player.dispose(); assert.deepEqual(f.revoked, [url]);
  assert.deepEqual(f.cancelled, [], "completed replay is not a provider cancellation");
});

test("MSE terminal event waits for a pending append before ending", t => {
  const f = mediaFixture(t); f.player.start("a", "audio/mpeg");
  const source = f.sources[0]; source.open();
  f.chunk("a", 0, "ab"); f.chunk("a", 1, "cd");
  f.player.finish({ request_id: "a", bytes: 4 });
  assert.equal(source.ends, 0); assert.equal(source.buffer.updating, true);
  source.buffer.update(); assert.equal(source.ends, 0);
  source.buffer.update(); assert.equal(source.ends, 1);
  assert.equal(f.player.retainedBytes, 0); f.player.dispose();
});

test("MSE append failures preserve every chunk for a bounded Blob fallback", async t => {
  const f = mediaFixture(t); f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1);
  source.buffer.throwOnAppend = true; source.open();
  f.chunk("a", 0, "ab"); f.chunk("a", 1, "cd");
  f.player.finish({ request_id: "a", bytes: 4 });
  const blobUrl = f.urls.at(-1), blob = f.objects.get(blobUrl);
  assert.ok(blob instanceof Blob); assert.equal(await blob.text(), "abcd");
  assert.equal(blob.type, "audio/mpeg"); assert.deepEqual(f.revoked, [streamUrl]);
  assert.equal(source.listenerCount(), 0); assert.equal(source.buffer.listenerCount(), 0);
  assert.equal(f.player.retainedBytes, 0); f.player.dispose();
  assert.deepEqual(f.revoked, [streamUrl, blobUrl]);
});

test("replacing MSE during append aborts old work and ignores captured callbacks", t => {
  const f = mediaFixture(t); f.player.start("old", "audio/mpeg");
  const source = f.sources[0], url = f.urls.at(-1); source.open();
  f.chunk("old", 0, "ab");
  const stale = [...source.buffer.listeners.get("updateend")];
  f.player.start("new", "audio/mpeg");
  for (const callback of stale) callback();
  source.buffer.update(); source.dispatchEvent(new Event("sourceopen"));
  assert.equal(source.buffer.aborts, 1); assert.equal(source.listenerCount(), 0);
  assert.equal(source.buffer.listenerCount(), 0); assert.equal(source.ends, 0);
  assert.equal(f.player.requestId, "new"); assert.equal(f.player.retainedBytes, 0);
  assert.deepEqual(f.cancelled, ["old"]); assert.deepEqual(f.revoked, [url]);
  f.player.dispose();
});

test("MSE detachment during download falls back without losing previously played bytes", async t => {
  const f = mediaFixture(t); f.player.start("a", "audio/mpeg");
  const source = f.sources[0], streamUrl = f.urls.at(-1); source.open();
  f.chunk("a", 0, "ab"); source.buffer.update();
  source.readyState = "closed"; source.dispatchEvent(new Event("sourceclose"));
  f.chunk("a", 1, "cd"); f.player.finish({ request_id: "a", bytes: 4 });
  const blob = f.objects.get(f.urls.at(-1));
  assert.ok(blob instanceof Blob); assert.equal(await blob.text(), "abcd");
  assert.equal(f.player.retainedBytes, 0); assert.equal(source.listenerCount(), 0);
  assert.deepEqual(f.revoked, [streamUrl]); f.player.dispose();
});
test("completion is idempotent, orders bytes and releases application buffers", () => {
  const f = fixture(); f.player.start("a", "audio/wav"); f.chunk("a", 1, "cd"); f.chunk("a", 0, "ab");
  f.player.finish({ request_id: "a", bytes: 4 });
  f.player.finish({ request_id: "a", bytes: 4 }); f.chunk("a", 2, "late");
  assert.equal(f.states.filter(s => s === "ready").length, 1);
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, null); f.player.dispose();
});
test("missing chunks, empty speech and excessive audio fail closed", () => {
  for (const scenario of ["missing", "empty", "large", "duplicate"]) {
    const f = fixture(4); f.player.start("a", "audio/wav");
    if (scenario === "missing") f.chunk("a", 1, "ab");
    if (scenario === "large") f.chunk("a", 0, "abcde");
    if (scenario === "duplicate") { f.chunk("a", 0, "ab"); f.chunk("a", 0, "ab"); }
    f.player.finish({ request_id: "a", bytes: scenario === "empty" ? 0 : 2 });
    assert.equal(f.states.at(-1), "error", scenario); assert.equal(f.player.retainedBytes, 0);
  }
});

test("command completion can overtake every chunk without losing a valid preview", async () => {
  const f = fixture(undefined, null, 5); f.player.start("a", "audio/wav");
  f.player.finish({ request_id: "a", bytes: 4, mime_type: "audio/wav" });
  assert.equal(f.states.at(-1), "streaming"); assert.equal(f.player.requestId, "a");
  f.chunk("a", 1, "cd"); f.player.finish({ request_id: "a", bytes: 4 });
  assert.equal(f.states.at(-1), "streaming");
  f.chunk("a", 0, "ab");
  assert.equal(f.states.at(-1), "ready"); assert.equal(f.player.retainedBytes, 0);
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(f.states.at(-1), "ready", "a completed session clears its assembly deadline");
  assert.equal(f.states.filter(s => s === "ready").length, 1); f.player.dispose();
});

test("missing bridge audio times out, releases bytes and ignores late delivery", async () => {
  const f = fixture(undefined, null, 5); f.player.start("a", "audio/wav");
  f.chunk("a", 0, "ab"); f.player.finish({ request_id: "a", bytes: 4 });
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(f.states.at(-1), "error"); assert.match(f.errors.at(-1), /timed out/);
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, null);
  assert.deepEqual(f.cancelled, ["a"]);
  f.chunk("a", 1, "cd"); f.player.finish({ request_id: "a", bytes: 4 });
  assert.equal(f.states.at(-1), "error"); assert.equal(f.player.retainedBytes, 0);
});

test("MSE detachment preserves the incomplete-audio assembly deadline", async t => {
  const f = mediaFixture(t, 5); f.player.start("a", "audio/mpeg");
  const source = f.sources[0]; source.open();
  f.chunk("a", 0, "ab"); source.buffer.update();
  f.player.finish({ request_id: "a", bytes: 4 });
  source.readyState = "closed"; source.dispatchEvent(new Event("sourceclose"));
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(f.states.at(-1), "error"); assert.match(f.errors.at(-1), /timed out/);
  assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, null);
  assert.deepEqual(f.cancelled, ["a"]); assert.equal(source.listenerCount(), 0);
});

test("replacing a session clears its assembly deadline", async () => {
  const f = fixture(undefined, null, 5); f.player.start("old", "audio/wav");
  f.player.finish({ request_id: "old", bytes: 4 });
  f.player.start("new", "audio/wav");
  await new Promise(resolve => setTimeout(resolve, 15));
  assert.equal(f.states.at(-1), "streaming"); assert.equal(f.player.requestId, "new");
  assert.equal(f.errors.at(-1), ""); assert.deepEqual(f.cancelled, ["old"]);
  f.player.dispose();
});

test("conflicting terminal byte counts fail closed", () => {
  const f = fixture(); f.player.start("a", "audio/wav");
  f.player.finish({ request_id: "a", bytes: 4 });
  f.player.finish({ request_id: "a", bytes: 5 });
  assert.equal(f.states.at(-1), "error"); assert.equal(f.player.requestId, null);
});

test("a finite chunk sequence space bounds tiny-chunk bookkeeping", () => {
  const f = fixture(); f.player.start("a", "audio/wav");
  f.chunk("a", 65_535, "x");
  assert.equal(f.player.retainedBytes, 1); assert.equal(f.states.at(-1), "streaming");
  f.chunk("a", 65_536, "x");
  assert.equal(f.states.at(-1), "error"); assert.equal(f.player.retainedBytes, 0);
  assert.equal(f.player.requestId, null); assert.deepEqual(f.cancelled, ["a"]);
});
test("many cancelled/completed previews retain no old sessions", () => {
  const f = fixture();
  for (let n = 0; n < 200; n++) {
    const id = String(n); f.player.start(id, "audio/wav"); f.chunk(id, 0, "hello");
    if (n % 2) f.player.finish({ request_id: id, bytes: 5 });
    f.player.dispose(); f.chunk(id, 1, "late");
    assert.equal(f.player.retainedBytes, 0); assert.equal(f.player.requestId, null);
  }
});
