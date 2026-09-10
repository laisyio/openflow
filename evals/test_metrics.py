import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import wave
from metrics import aggregate, edits, normalize, paired_human_interval, percentile, score
from run import DEFAULT_MANIFEST, command_text, load_manifest, markdown, summarize, supervised_worker, write_json
from rescore import rescore


class MetricsTests(unittest.TestCase):
    def test_default_corpus_runs_without_ignored_generated_audio(self):
        manifest = load_manifest(DEFAULT_MANIFEST)
        self.assertEqual(len(manifest["clips"]), 24)
        self.assertEqual(len({c["speaker"] for c in manifest["clips"]}), 12)
        self.assertTrue(all(c["path"].startswith("human/") for c in manifest["clips"]))

    def test_known_edit_counts(self):
        self.assertEqual(edits("a b c".split(), "a d c e".split()),
                         dict(errors=2, substitutions=1, deletions=0, insertions=1, reference_count=3))
        self.assertEqual(edits(["a"], [])["deletions"], 1)

    def test_empty_reference_is_hallucination_not_divide_by_zero(self):
        row = score("", "Thanks for watching")
        self.assertIsNone(row["wer"])
        self.assertTrue(row["speech_on_empty"])
        self.assertFalse(score("", "")['speech_on_empty'])

    def test_no_number_or_filler_error_hidden_by_normalization(self):
        self.assertEqual(normalize("Héllo, WORLD!"), "héllo world")
        self.assertNotEqual(normalize("twenty four"), normalize("24"))
        self.assertGreater(score("um twenty four", "24")['wer'], 0)
        self.assertEqual(score("twenty four", "24", [["twenty four", "24"]])["terms_matched"], 1)
        self.assertEqual(score("cat", "concatenate", [["cat"]])["terms_matched"], 0)

    def test_chinese_uses_character_metric(self):
        self.assertEqual(score("星期五", "星期四")["cer"], 1/3)
        self.assertEqual(score("明天星期五开会", "明天星期五开会", [["星期五"]])["terms_matched"], 1)

    def test_percentile_interpolates(self):
        self.assertEqual(percentile([1, 2, 3], 95), 2.9)
        self.assertIsNone(percentile([], 95))

    def test_failures_visible_and_strata_separate(self):
        good = dict(id="a", speaker="1", status="ok", group="human-clean", language="en",
                    reference="hello", score=score("hello", "hello"), median_s=1, duration_s=2)
        bad = dict(good, id="b", status="error")
        group = aggregate([good, bad])
        self.assertEqual((group["successful"], group["failed"], group["wer"]), (1, 1, 0))
        synthetic = dict(good, group="synthetic-dictation")
        result = summarize([dict(model={"id":"m"}, rows=[good, synthetic])])
        self.assertEqual(len(result["strata"]["m"]), 2)

    def test_paired_resampling_excludes_derivatives_and_repeats(self):
        a = [dict(id=str(i), speaker=str(i), status="ok", group="human-clean", score=score("hello", "hello")) for i in range(3)]
        b = [dict(row, score=score("hello", "wrong")) for row in a]
        a.append(dict(a[0], id="noise", group="human-derived"))
        interval = paired_human_interval(a, b, 100)
        self.assertEqual((interval["delta_wer"], interval["low"], interval["high"]), (-1, -1, -1))
        self.assertEqual(interval["paired_clips"], 3)

    def test_manifest_hash_and_path_validation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with wave.open(str(root / "a.wav"), "wb") as wav:
                wav.setparams((1, 2, 16000, 0, "NONE", "not compressed")); wav.writeframes(b"\0\0" * 16000)
            clip = dict(id="a", path="a.wav", duration_s=1, sha256=hashlib.sha256((root / "a.wav").read_bytes()).hexdigest())
            path = root / "manifest.json"
            path.write_text(json.dumps(dict(clips=[clip])))
            self.assertEqual(len(load_manifest(path)["clips"]), 1)
            clip["duration_s"] = 0
            path.write_text(json.dumps(dict(clips=[clip])))
            with self.assertRaisesRegex(ValueError, "duration mismatch"): load_manifest(path)
            clip["duration_s"] = 1
            path.write_text(json.dumps(dict(clips=[clip])))
            (root / "a.wav").write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "hash mismatch"): load_manifest(path)
            clip["path"] = "../escape.wav"
            path.write_text(json.dumps(dict(clips=[clip])))
            with self.assertRaisesRegex(ValueError, "escapes"): load_manifest(path)

    def test_native_hang_keeps_completed_rows_and_marks_missing_coverage(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "result.json"
            write_json(path, dict(model={"id": "test"}, complete=False,
                                 progress=dict(phase="recognition", monotonic_s=time.monotonic()),
                                 rows=[dict(id="done", status="ok"), dict(id="bad", status="error", error="Unsupported language"), dict(id="next", status="pending")]))
            started = time.monotonic()
            result = supervised_worker([sys.executable, "-c", "import time; time.sleep(30)"], path,
                                       dict(id="test", clip_timeout_s=0.15), os.environ)
            self.assertLess(time.monotonic() - started, 5)
            self.assertIn("recognition timeout", result["error"])
            self.assertEqual([row["status"] for row in result["rows"]], ["ok", "error", "error"])
            self.assertEqual(result["rows"][1]["error"], "Unsupported language")

    def test_worker_failure_is_not_successful_empty_report(self):
        with tempfile.TemporaryDirectory() as temp:
            result = supervised_worker([sys.executable, "-c", "raise SystemExit(9)"], Path(temp) / "result.json",
                                       dict(id="test"), os.environ)
            self.assertEqual(result["error"], "Worker exit 9")
            self.assertFalse(result["complete"])
            result = supervised_worker([sys.executable, "-c", "raise SystemExit(0)"], Path(temp) / "result.json",
                                       dict(id="test"), os.environ)
            self.assertIn("without a complete result", result["error"])
            self.assertIn("error", summarize([result])["strata"]["test"])

    def test_interrupt_always_stops_worker(self):
        with tempfile.TemporaryDirectory() as temp, patch("run.time.sleep", side_effect=KeyboardInterrupt), patch("run.stop_worker", wraps=__import__("run").stop_worker) as stop:
            with self.assertRaises(KeyboardInterrupt):
                supervised_worker([sys.executable, "-c", "import time; time.sleep(30)"], Path(temp) / "result.json", dict(id="test"), os.environ)
            stop.assert_called_once()
            self.assertIsNotNone(stop.call_args.args[0].returncode)

    def test_adapter_children_cleaned_up_on_success_and_timeout(self):
        for timeout_case in (False, True):
            with self.subTest(timeout=timeout_case), tempfile.TemporaryDirectory() as temp:
                pid_file = Path(temp) / "child.pid"
                script = ("import pathlib,subprocess,sys,time; "
                          "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); "
                          "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); "
                          "print('{\"text\":\"hello\"}',flush=True); " + ("time.sleep(30)" if timeout_case else ""))
                argv = [sys.executable, "-c", script, str(pid_file)]
                if timeout_case:
                    with self.assertRaises(subprocess.TimeoutExpired): command_text(argv, 1)
                else: self.assertEqual(command_text(argv, 3), "hello")
                pid = int(pid_file.read_text())
                # An orphan can briefly be a zombie until init reaps it; that
                # state has no runnable descendant or retained decoder memory.
                for _ in range(40):
                    state = subprocess.run(["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True).stdout.strip()
                    if not state or state.startswith("Z"): break
                    time.sleep(0.05)
                self.assertTrue(not state or state.startswith("Z"), state)

    def test_report_keeps_top_level_failure_and_rejects_double_postpass(self):
        report = dict(manifest_sha256="test", results=[dict(model={"id": "test"}, rows=[], error="unload failed")],
                      summary=dict(strata={"test": {}}, paired_human_wer={}))
        self.assertIn("unload failed", markdown(report))
        with self.assertRaisesRegex(ValueError, "original raw"):
            rescore(dict(postpass={}), Path("unused"), "")

    def test_postpass_keeps_raw_input_outputs_and_timing_unchanged(self):
        row = dict(id="a", status="ok", reference="hello", transcript="wrong", score=score("hello", "wrong"),
                   repeat_agreement=True, outputs=["wrong", "wrong"], timings_s=[1, 1], median_s=1,
                   duration_s=2, group="synthetic-dictation", language="en")
        report = dict(results=[dict(model={"id": "test"}, rows=[row])])
        original = json.dumps(report, sort_keys=True)
        with tempfile.TemporaryDirectory() as temp:
            executable = Path(temp) / "adapter"
            executable.write_bytes(b"fixture executable fingerprint")
            with patch("rescore.subprocess.run", return_value=SimpleNamespace(stdout='"hello"\n')):
                corrected = rescore(report, executable, "wrong -> hello")
        actual = corrected["results"][0]["rows"][0]
        self.assertEqual(json.dumps(report, sort_keys=True), original)
        self.assertEqual((actual["raw_transcript"], actual["transcript"]), ("wrong", "hello"))
        self.assertEqual(actual["raw_score"]["wer"], 1)
        self.assertEqual(actual["score"]["wer"], 0)
        self.assertTrue(actual["raw_repeat_agreement"])
        self.assertEqual(actual["outputs"], ["wrong", "wrong"])
        self.assertEqual(actual["timings_s"], [1, 1])


if __name__ == "__main__": unittest.main()
