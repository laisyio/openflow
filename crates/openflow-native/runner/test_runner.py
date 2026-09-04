#!/usr/bin/env python3
"""Tests for the sidecar's handling of the scratch recording.

    python3 crates/openflow-native/runner/test_runner.py

Standard library only, and no model: `ModelHolder.transcribe` is replaced with
a stub that sleeps, so a signal can land while a request is in flight without
`mlx_audio` or a gigabyte of weights being present. Nothing here touches the
network beyond loopback, and nothing here touches the app.

What is being pinned: a recording of the user speaking must not be on disk
after the process that wrote it has gone. The supervisor's `terminate` sends
SIGTERM first (`crates/openflow-core/src/runner.rs`), which Python's default
disposition turns into an exit with no unwinding -- so the `finally` around the
decode never covered the app's own quit path.
"""

from __future__ import annotations

import importlib.util
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
RUNNER = os.path.join(HERE, "runner.py")
BOUNDARY = "----openflowrunnertest"
# Kept here rather than read off the module under test, so a regression shows
# up as the wrong behaviour rather than as an AttributeError.
SCRATCH_PREFIX = "openflow-scratch-"
# Long enough that the signal always lands mid-decode, short enough that a
# test that somehow gets past the signal still finishes.
STUB_SECONDS = "20"

# A child that loads runner.py, stubs the decode, and serves. Passed with `-c`
# so the shipped file needs no test hook in it.
BOOTSTRAP = """
import importlib.util, sys, time
spec = importlib.util.spec_from_file_location("openflow_runner", %r)
module = importlib.util.module_from_spec(spec)
sys.modules["openflow_runner"] = module
spec.loader.exec_module(module)
module.ModelHolder.transcribe = lambda self, path, language: (
    time.sleep(float(%r)), "stub")[1]
sys.argv = ["runner.py", "--port", "0", "--model", "stub", "--idle-minutes", "10"]
sys.exit(module.main())
""" % (RUNNER, STUB_SECONDS)


def load_runner():
    """Import runner.py under its own name, for the tests that call into it
    directly rather than over HTTP."""
    spec = importlib.util.spec_from_file_location("openflow_runner_under_test", RUNNER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def recordings_in(directory: str) -> list[str]:
    return sorted(name for name in os.listdir(directory) if name.endswith(".wav"))


def a_dead_pid() -> int:
    """A pid that has certainly exited and been reaped."""
    finished = subprocess.Popen([sys.executable, "-c", "pass"])
    finished.wait()
    return finished.pid


class Sidecar:
    """A runner process with a stubbed decode, and one request in flight."""

    def __init__(self, temp_dir: str) -> None:
        self.lines: list[str] = []
        self.process = subprocess.Popen(
            [sys.executable, "-c", BOOTSTRAP],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            env={**os.environ, "TMPDIR": temp_dir, "PYTHONUNBUFFERED": "1"},
        )
        threading.Thread(target=self._pump, daemon=True).start()
        self.port = self._await_port()

    def _pump(self) -> None:
        for raw in self.process.stderr:
            self.lines.append(raw.decode("utf-8", "replace").rstrip())

    def _await_port(self) -> int:
        marker = "runner listening on http://127.0.0.1:"
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            for line in list(self.lines):
                if marker in line:
                    digits = line.split(marker, 1)[1].split()[0]
                    return int(digits)
            if self.process.poll() is not None:
                raise AssertionError("the runner exited: %s" % self.lines)
            time.sleep(0.02)
        raise AssertionError("the runner never listened: %s" % self.lines)

    def start_transcription(self) -> None:
        audio = b"RIFF" + b"\x00" * 4 + b"WAVEfmt " + b"\x11" * 4096
        body = (
            ("--%s\r\n" % BOUNDARY).encode()
            + b'Content-Disposition: form-data; name="file"; filename="a.wav"\r\n'
            + b"Content-Type: audio/wav\r\n\r\n"
            + audio
            + ("\r\n--%s--\r\n" % BOUNDARY).encode()
        )
        request = urllib.request.Request(
            "http://127.0.0.1:%d/v1/audio/transcriptions" % self.port,
            data=body,
            headers={
                "Content-Type": "multipart/form-data; boundary=%s" % BOUNDARY
            },
        )

        def send() -> None:
            try:
                urllib.request.urlopen(request, timeout=60).read()
            except (OSError, urllib.error.URLError):
                pass  # the process is signalled out from under it on purpose

        self.sender = threading.Thread(target=send, daemon=True)
        self.sender.start()

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()
        self.process.stderr.close()


class ScratchRecordingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="openflow-runner-test-")
        self.dir = self.temp.name
        self.addCleanup(self.temp.cleanup)

    def runner_using_temp_dir(self):
        """runner.py with `tempfile.gettempdir()` pointed at this test's
        directory. `tempfile` is a shared module, so the previous value is put
        back before the directory goes away."""
        module = load_runner()
        previous = tempfile.tempdir
        self.addCleanup(setattr, tempfile, "tempdir", previous)
        tempfile.tempdir = self.dir
        return module

    def await_recording(self, sidecar: Sidecar) -> str:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            found = recordings_in(self.dir)
            if found:
                return found[0]
            self.assertIsNone(
                sidecar.process.poll(), "the runner exited: %s" % sidecar.lines
            )
            time.sleep(0.02)
        raise AssertionError("the runner never wrote a recording")

    def signal_mid_transcription(self, number: int) -> None:
        sidecar = Sidecar(self.dir)
        self.addCleanup(sidecar.close)
        sidecar.start_transcription()
        name = self.await_recording(sidecar)
        self.assertEqual(
            0o600, os.stat(os.path.join(self.dir, name)).st_mode & 0o777
        )

        sidecar.process.send_signal(number)
        sidecar.process.wait(timeout=30)
        # The point of the test, asserted before anything about the name, so a
        # regression reports the leak rather than a detail of the leak.
        self.assertEqual(
            [],
            recordings_in(self.dir),
            "%s left the user's recording on disk" % signal.Signals(number).name,
        )
        self.assertTrue(name.startswith(SCRATCH_PREFIX), name)

    @unittest.skipUnless(os.name == "posix", "signals are posix here")
    def test_sigterm_mid_transcription_leaves_no_recording(self) -> None:
        # SIGTERM is the app's ordinary quit path, not a corner case: it is the
        # first signal `terminate` sends in openflow-core's runner supervisor.
        self.signal_mid_transcription(signal.SIGTERM)

    @unittest.skipUnless(os.name == "posix", "signals are posix here")
    def test_sigint_mid_transcription_leaves_no_recording(self) -> None:
        # The decode runs on a daemon thread, so an interpreter shutdown that
        # unwinds the main thread still does not unwind the decode.
        self.signal_mid_transcription(signal.SIGINT)

    def test_a_finished_transcription_leaves_no_recording(self) -> None:
        module = self.runner_using_temp_dir()
        path = module.new_scratch(b"audio")
        self.assertEqual([os.path.basename(path)], recordings_in(self.dir))
        module.drop_scratch(path)
        self.assertEqual([], recordings_in(self.dir))

    @unittest.skipUnless(os.name == "posix", "the sweep is posix-only")
    def test_a_starting_runner_sweeps_what_a_killed_one_left(self) -> None:
        # The residue of SIGKILL and of a power cut: no process was alive to
        # clean up, so the next one does it.
        module = self.runner_using_temp_dir()
        orphan = os.path.join(
            self.dir, "%s%d-abcd.wav" % (SCRATCH_PREFIX, a_dead_pid())
        )
        with open(orphan, "wb") as handle:
            handle.write(b"speech")
        module.sweep_orphan_scratch()
        self.assertEqual([], recordings_in(self.dir))

    @unittest.skipUnless(os.name == "posix", "the sweep is posix-only")
    def test_the_sweep_spares_a_running_runners_recording(self) -> None:
        # Two sidecars can briefly overlap. Deleting the live one's audio
        # would fail a dictation, so a leftover is only swept once its writer
        # is gone.
        module = self.runner_using_temp_dir()
        mine = os.path.join(
            self.dir, "%s%d-efgh.wav" % (SCRATCH_PREFIX, os.getpid())
        )
        with open(mine, "wb") as handle:
            handle.write(b"speech")
        module.sweep_orphan_scratch()
        self.assertEqual([os.path.basename(mine)], recordings_in(self.dir))


if __name__ == "__main__":
    unittest.main(verbosity=2)
