#!/usr/bin/env python3
"""OpenFlow's local transcription sidecar: MLX Qwen3-ASR behind an
OpenAI-compatible endpoint on loopback.

    python3 runner.py --port 8123 --model mlx-community/Qwen3-ASR-0.6B-8bit \
                      --idle-minutes 10

Standard library plus `mlx_audio`, nothing else. The app supervises this
process (`crates/openflow-core/src/runner.rs`): it creates the virtualenv,
downloads the weights, picks a free port, waits for `/health`, restarts this
process if it dies and kills it on quit.

Three endpoints:

  GET  /health                     {"state": "loading" | "ready" | "unloaded", ...}
  POST /prewarm                    load if unloaded, answer immediately
  POST /v1/audio/transcriptions    multipart `file`, `model`, `language`;
                                   `prompt` is accepted and ignored, because
                                   Qwen does not honour it -- the dictionary is
                                   applied by the app as a post-pass instead.
                                   Answers {"text": ...}, like the LAN bridge.

Three things about it are deliberate and load-bearing:

* **Loopback by construction.** The listening address is the literal
  `127.0.0.1`; there is no flag for it. A `Host` header naming anything else is
  refused, which costs nothing and closes the DNS-rebinding path a browser on
  this machine could otherwise use to reach the port.
* **One inference at a time.** MLX is not safe to drive from two threads at
  once, and a second concurrent decode would only make both slower on the same
  GPU. Requests queue on a lock.
* **Idle unload.** A resident 0.6B holds about 1 GB of unified memory and 1.7B
  about 2.5 GB, which is not nothing on a 16 GB machine. After the idle window
  the weights are dropped; the next dictation pays about 3 s to reload, which
  the app hides behind `/prewarm` when recording starts.
"""

from __future__ import annotations

import argparse
import gc
import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Loopback, and not a parameter: see the module docstring.
BIND_HOST = "127.0.0.1"
LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "::1", "[::1]")
# A minute of audio at 16 kHz mono float32 is about 4 MB; 50 MB is the same cap
# the app puts on a recording before it refuses to send it.
MAX_UPLOAD_BYTES = 50 * 1024 * 1024


class ModelHolder:
    """The weights, and the two things that may touch them: a loader and an
    inference. Both take `lock`, so a request that arrives mid-load waits for
    the load rather than starting a second one."""

    def __init__(self, repo: str, idle_seconds: float) -> None:
        self.repo = repo
        self.idle_seconds = idle_seconds
        self.lock = threading.Lock()
        self._model = None
        self._loading = False
        self._error: str | None = None
        self.last_used = time.monotonic()

    # ── state ────────────────────────────────────────────
    def state(self) -> str:
        """Which of the three states this holder is in.

        `_loading` is read *without* taking `lock`, deliberately. The loader
        holds that lock for the several seconds a model load takes, so a
        `/health` that took it would block for the whole load -- and a health
        endpoint that stops answering while the thing it reports on is busy is
        exactly how the supervisor decides a sidecar is dead. Reading the flag
        unlocked can only be briefly stale in one direction at each edge
        (`unloaded` a moment before `loading`, `loading` a moment before
        `ready`), which is the resolution `/health` promises anyway: the next
        poll, 120 ms later, corrects it. Nothing branches on the difference.
        """
        if self._loading:
            return "loading"
        return "ready" if self._model is not None else "unloaded"

    def error(self) -> str | None:
        return self._error

    # ── loading ──────────────────────────────────────────
    def ensure_loaded(self):
        with self.lock:
            return self._load_locked()

    def _load_locked(self):
        if self._model is not None:
            return self._model
        self._loading = True
        self._error = None
        try:
            from mlx_audio.stt.utils import load_model

            self._model = load_model(self.repo)
            return self._model
        except BaseException as failure:  # noqa: BLE001 - reported over HTTP
            self._error = f"{type(failure).__name__}: {failure}"
            raise
        finally:
            self._loading = False
            self.last_used = time.monotonic()

    def prewarm_async(self) -> None:
        """Load in the background and answer now. The app calls this when
        recording starts, so the load overlaps the user speaking."""
        if self.state() != "unloaded":
            return

        def load() -> None:
            try:
                self.ensure_loaded()
            except BaseException:  # noqa: BLE001 - /health reports it
                pass

        threading.Thread(target=load, name="prewarm", daemon=True).start()

    def unload(self) -> None:
        with self.lock:
            if self._model is None:
                return
            self._model = None
        gc.collect()
        try:
            import mlx.core as mx

            mx.clear_cache()
        except Exception:  # noqa: BLE001 - freeing the cache is best effort
            pass

    # ── inference ────────────────────────────────────────
    def transcribe(self, wav_path: str, language: str | None) -> str:
        with self.lock:
            model = self._load_locked()
            self.last_used = time.monotonic()
            result = _generate(model, wav_path, language)
            self.last_used = time.monotonic()
        text = getattr(result, "text", None)
        if text is None:
            text = str(result)
        return text.strip()

    def idle_sweep(self) -> None:
        """Drop the weights once the machine has been quiet for the window."""
        while True:
            time.sleep(min(30.0, max(1.0, self.idle_seconds / 4.0)))
            if self.state() != "ready":
                continue
            if time.monotonic() - self.last_used >= self.idle_seconds:
                self.unload()


def _generate(model, wav_path: str, language: str | None):
    """`model.generate(path)` is the API that works across mlx-audio versions;
    `language` is passed only when this build's signature takes it, rather than
    guessed at and turned into a TypeError mid-dictation."""
    if language:
        try:
            import inspect

            if "language" in inspect.signature(model.generate).parameters:
                return model.generate(wav_path, language=language)
        except (TypeError, ValueError):
            pass
    return model.generate(wav_path)


# ── memory reporting ──────────────────────────────────────


def resident_bytes() -> int:
    """Current resident set size, which is what the Settings screen shows as
    the cost of keeping the model loaded. `ps` is stdlib-reachable and gives the
    live number; `ru_maxrss` is the high-water mark and only a fallback."""
    try:
        output = subprocess.run(
            ["/bin/ps", "-o", "rss=", "-p", str(os.getpid())],
            capture_output=True,
            text=True,
            timeout=2,
        ).stdout.strip()
        if output:
            return int(output) * 1024
    except Exception:  # noqa: BLE001 - fall through to the coarse number
        pass
    try:
        import resource

        return int(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
    except Exception:  # noqa: BLE001
        return 0


def active_memory_bytes() -> int | None:
    """What MLX itself thinks it is holding, when it is loaded at all."""
    try:
        import mlx.core as mx

        return int(mx.get_active_memory())
    except Exception:  # noqa: BLE001
        return None


# ── the scratch recording ─────────────────────────────────
#
# `model.generate()` takes a path, so a recording of the user speaking has to
# sit on disk for the length of one decode. It must not outlive that, and the
# `finally` around the decode is not enough on its own:
#
# * SIGTERM is not a corner case here. It is what the supervisor sends when the
#   app quits and when the model or the idle window changes
#   (`terminate` in `crates/openflow-core/src/runner.rs`), and Python's default
#   disposition for it ends the process without unwinding -- so a dictation in
#   flight at that moment left its wav behind.
# * SIGINT is no better. The request runs on one of `ThreadingHTTPServer`'s
#   daemon threads, and those are stopped at interpreter shutdown without their
#   `finally` blocks running.
# * `watch_parent` leaves through `os._exit`, which skips `finally` as well.
#
# So the paths in flight are tracked and every exit this process can see
# unlinks them first. Nothing in a process sees SIGKILL or a power cut, which
# is why the names carry the pid that wrote them: a starting runner can delete
# the leftovers of runners that are gone, and leave alone the ones that are not.

SCRATCH_PREFIX = "openflow-scratch-"
# Held only around a set operation, never across I/O, so the signal handler on
# the main thread waits for a worker at most momentarily. The main thread never
# takes it itself, which is what would turn that wait into a deadlock.
_scratch_lock = threading.Lock()
_scratch_paths: set[str] = set()


def new_scratch(audio: bytes) -> str:
    """Write `audio` to a private file and remember it until it is dropped.

    `mkstemp` opens 0600, and the file goes in this user's temp directory --
    on macOS a per-user one the operating system creates 0700.
    """
    handle, path = tempfile.mkstemp(
        prefix="%s%d-" % (SCRATCH_PREFIX, os.getpid()), suffix=".wav"
    )
    with _scratch_lock:
        _scratch_paths.add(path)
    try:
        with os.fdopen(handle, "wb") as scratch:
            scratch.write(audio)
    except BaseException:
        drop_scratch(path)
        raise
    return path


def drop_scratch(path: str) -> None:
    """Unlink one recording. Safe to call twice."""
    with _scratch_lock:
        _scratch_paths.discard(path)
    try:
        os.unlink(path)
    except OSError:
        pass


def drop_all_scratch() -> None:
    """Unlink every recording still in flight. Safe from a signal handler and
    safe to call twice."""
    with _scratch_lock:
        paths = list(_scratch_paths)
        _scratch_paths.clear()
    for path in paths:
        try:
            os.unlink(path)
        except OSError:
            pass


def _pid_is_running(pid: int) -> bool:
    """Conservative: anything other than "that pid is definitely gone" counts
    as running, because the only use of this is deciding whether a file is
    safe to delete."""
    if pid <= 0:
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        # EPERM says something is there; it is just not ours.
        return True
    return True


def sweep_orphan_scratch() -> None:
    """Delete recordings left behind by runners that are no longer running.

    A SIGKILL or a power cut leaves the file and nothing in that process can
    help; the next start can. Every name carries the pid that wrote it, so a
    leftover whose writer is gone is unambiguously finished with, and one whose
    writer is alive is left alone -- two sidecars can briefly overlap, and
    deleting a live one's audio would fail a dictation.
    """
    if os.name != "posix":
        # `os.kill(pid, 0)` is not a liveness test off POSIX, and without one
        # this cannot tell a leftover from a recording being decoded right now.
        return
    directory = tempfile.gettempdir()
    try:
        names = os.listdir(directory)
    except OSError:
        return
    for name in names:
        if not name.startswith(SCRATCH_PREFIX) or not name.endswith(".wav"):
            continue
        owner = name[len(SCRATCH_PREFIX) :].split("-", 1)[0]
        if not owner.isdigit() or _pid_is_running(int(owner)):
            continue
        path = os.path.join(directory, name)
        try:
            if os.stat(path).st_uid != os.getuid():
                continue
            os.unlink(path)
        except OSError:
            continue


def install_exit_handlers() -> None:
    """Unlink the recording in flight before this process goes away.

    The handler re-raises with the default disposition rather than exiting
    itself, so the status the supervisor waits on is exactly the one it saw
    before: killed by that signal. Only the unlink in front of it is new.
    """

    def handler(number, _frame) -> None:
        drop_all_scratch()
        signal.signal(number, signal.SIG_DFL)
        os.kill(os.getpid(), number)

    for number in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(number, handler)
        except (ValueError, OSError):  # not the main thread, or no such signal
            pass


# ── multipart ─────────────────────────────────────────────


def parse_multipart(body: bytes, content_type: str) -> dict[str, bytes]:
    """The subset of `multipart/form-data` this endpoint needs.

    Written out rather than taken from a library because `cgi` was removed in
    Python 3.13 and this file may not add a dependency. Parts are returned by
    field name; the last part with a given name wins, as in every other
    implementation.
    """
    marker = "boundary="
    if marker not in content_type:
        raise ValueError("multipart body has no boundary")
    boundary = content_type.split(marker, 1)[1].strip().strip('"')
    separator = b"--" + boundary.encode()
    fields: dict[str, bytes] = {}
    for chunk in body.split(separator):
        if chunk in (b"", b"--", b"--\r\n", b"\r\n"):
            continue
        chunk = chunk.lstrip(b"\r\n")
        if chunk.startswith(b"--"):
            continue
        head, _, payload = chunk.partition(b"\r\n\r\n")
        if not _:
            continue
        name = None
        for line in head.split(b"\r\n"):
            lowered = line.lower()
            if lowered.startswith(b"content-disposition:") and b"name=" in lowered:
                after = line.split(b"name=", 1)[1]
                if after.startswith(b'"'):
                    name = after[1:].split(b'"', 1)[0].decode("utf-8", "replace")
                else:
                    name = after.split(b";", 1)[0].strip().decode("utf-8", "replace")
                break
        if name is None:
            continue
        fields[name] = payload[:-2] if payload.endswith(b"\r\n") else payload
    return fields


# ── HTTP ──────────────────────────────────────────────────


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    holder: ModelHolder  # set on the class before the server starts

    def log_message(self, format: str, *args) -> None:  # noqa: A002
        """One line per request on stderr, which the app captures. The default
        writes to stderr too but with a timestamp format nothing reads."""
        sys.stderr.write("runner %s\n" % (format % args))

    # ── helpers ──────────────────────────────────────────
    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _host_is_loopback(self) -> bool:
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0].strip().lower()
        # An empty Host is HTTP/1.0 and cannot have been aimed by a browser.
        return host == "" or host in LOOPBACK_HOSTS

    def _guard(self) -> bool:
        if self._host_is_loopback():
            return True
        self._reply(403, {"error": "this runner answers on loopback only"})
        return False

    def _health(self) -> dict:
        holder = self.holder
        payload = {
            "state": holder.state(),
            "model": holder.repo,
            "resident_bytes": resident_bytes(),
            "idle_seconds": round(time.monotonic() - holder.last_used, 1),
            "idle_unload_seconds": holder.idle_seconds,
            "pid": os.getpid(),
        }
        active = active_memory_bytes()
        if active is not None:
            payload["active_memory_bytes"] = active
        if holder.error():
            payload["error"] = holder.error()
        return payload

    # ── routes ───────────────────────────────────────────
    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's name
        if not self._guard():
            return
        if self.path.split("?", 1)[0] == "/health":
            self._reply(200, self._health())
        else:
            self._reply(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if not self._guard():
            return
        route = self.path.split("?", 1)[0]
        if route == "/prewarm":
            self.holder.prewarm_async()
            self._reply(200, self._health())
            return
        if route != "/v1/audio/transcriptions":
            self._reply(404, {"error": "not found"})
            return
        self._transcribe()

    def _transcribe(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0:
            self._reply(400, {"error": "no audio uploaded"})
            return
        if length > MAX_UPLOAD_BYTES:
            self._reply(413, {"error": "recording is too large"})
            return
        body = self.rfile.read(length)
        try:
            fields = parse_multipart(body, self.headers.get("Content-Type") or "")
        except ValueError as failure:
            self._reply(400, {"error": str(failure)})
            return
        audio = fields.get("file")
        if not audio:
            self._reply(400, {"error": "no file part in the request"})
            return
        language = (fields.get("language") or b"").decode("utf-8", "replace").strip()
        if language in ("", "auto"):
            language = None
        # `model` and `prompt` are accepted and ignored on purpose: the model is
        # whichever one this process was started with, and Qwen does not honour
        # a prompt, which is why the app applies the dictionary itself.

        path = None
        try:
            path = new_scratch(audio)
            started = time.monotonic()
            text = self.holder.transcribe(path, language)
            elapsed_ms = int((time.monotonic() - started) * 1000)
            self.log_message("transcribed %d bytes in %d ms", len(audio), elapsed_ms)
            self._reply(200, {"text": text})
        except BaseException as failure:  # noqa: BLE001 - reported to the app
            self._reply(500, {"error": f"{type(failure).__name__}: {failure}"})
        finally:
            if path:
                drop_scratch(path)


def watch_parent(parent_pid: int) -> None:
    """Exit when the app that started us is gone.

    The supervisor kills this process on quit, but a crashed or SIGKILLed app
    kills nothing, and a sidecar holding 2.5 GB of weights must not outlive it.
    Re-parenting to pid 1 is the signal, and checking for it costs one integer
    read every few seconds.
    """
    while True:
        time.sleep(5)
        if os.getppid() != parent_pid:
            # `os._exit` skips every `finally`, so the recording a decode is
            # holding has to go first: the app being SIGKILLed is exactly the
            # case that leaves one behind otherwise.
            drop_all_scratch()
            os._exit(0)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--idle-minutes", type=float, default=10.0)
    arguments = parser.parse_args()

    # Before anything can be recorded: clear out what a SIGKILLed or
    # power-cut predecessor could not clear out itself.
    sweep_orphan_scratch()
    install_exit_handlers()

    holder = ModelHolder(arguments.model, max(0.5, arguments.idle_minutes) * 60.0)
    Handler.holder = holder
    threading.Thread(target=holder.idle_sweep, name="idle", daemon=True).start()
    threading.Thread(
        target=watch_parent, args=(os.getppid(),), name="parent", daemon=True
    ).start()

    server = ThreadingHTTPServer((BIND_HOST, arguments.port), Handler)
    server.daemon_threads = True
    # `--port 0` asks the kernel for a free one, which is how the app starts us:
    # a parent that picks the port has to let go of it before we bind, and two
    # sidecars starting at once can be handed the same number in that gap. The
    # bound port is published on the line below, and the supervisor reads it
    # from our stderr rather than assuming it already knows.
    port = server.server_address[1]
    sys.stderr.write(
        "runner listening on http://%s:%d model=%s idle=%.1fm\n"
        % (BIND_HOST, port, arguments.model, arguments.idle_minutes)
    )
    sys.stderr.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        drop_all_scratch()
    return 0


if __name__ == "__main__":
    sys.exit(main())
