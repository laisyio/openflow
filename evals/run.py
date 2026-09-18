#!/usr/bin/env python3
"""Compare local ASR models on identical hash-checked audio, in fresh processes.

No model downloads or cloud calls by default. Every result includes raw outputs,
versions, model file hashes, failures, timing samples and separate memory metrics.
"""
import argparse
from datetime import datetime, timezone
import gc
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import random
import resource
import signal
import subprocess
import sys
import threading
import time
import wave
from itertools import combinations

from metrics import NORMALIZATION, aggregate, paired_human_interval, percentile, score

ROOT = Path(__file__).resolve().parent
DEFAULT_MANIFEST = ROOT / "corpus/human-manifest.json"


def write_json(path, data):
    """Atomic checkpoints: the supervisor never sees a half-written result."""
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def kill_group(process):
    try: os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError: pass
    process.wait()


def stop_worker(process):
    # Local command adapters have their own group for per-invocation cleanup.
    # Stop those descendants before reaping the worker as well.
    try: import psutil
    except ImportError: psutil = None
    if psutil:
        try:
            for child in reversed(psutil.Process(process.pid).children(recursive=True)):
                try: child.kill()
                except psutil.NoSuchProcess: pass
        except (psutil.Error, OSError): pass
    kill_group(process)


def command_text(argv, timeout):
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        if process.returncode: raise subprocess.CalledProcessError(process.returncode, argv, stdout, stderr)
        result = json.loads(stdout)["text"]
        if not isinstance(result, str): raise ValueError("Adapter text must be a string")
        return result
    finally:
        # Also runs after successful immediate-parent exit: descendants must
        # not survive and use resources during another clip/model.
        kill_group(process)
        if process.stdout: process.stdout.close()
        if process.stderr: process.stderr.close()


def supervised_worker(argv, result_path, config, env):
    """Bound native-library hangs too, retaining completed clip checkpoints.

    Python signals cannot interrupt all native decoders. The parent watches a
    progress heartbeat and terminates the isolated worker's entire process
    group, including any explicitly configured command-adapter descendants.
    """
    started = time.monotonic()
    process = subprocess.Popen(argv, env=env, start_new_session=True)
    result = None
    error = None
    try:
        while process.poll() is None:
            if result_path.exists(): result = json.loads(result_path.read_text())
            now = time.monotonic()
            progress = (result or {}).get("progress", {})
            phase = progress.get("phase", "load")
            deadline = config.get("load_timeout_s", 300) if phase == "load" else config.get("clip_timeout_s", 120)
            if now - started > config.get("suite_timeout_s", 1800): error = "Whole-model suite timeout"
            elif now - progress.get("monotonic_s", started) > deadline: error = f"{phase} timeout ({deadline}s)"
            if error: break
            time.sleep(0.05)
    finally:
        if process.poll() is None: stop_worker(process)
    if result_path.exists(): result = json.loads(result_path.read_text())
    if process.returncode != 0 and not error: error = f"Worker exit {process.returncode}"
    if result is None: result = dict(model={"id": config["id"]})
    if not error and not result.get("complete", False):
        error = result.get("error", "Worker exited without a complete result")
    if error: result.update(error=error, complete=False)
    if error or not result.get("complete", False):
        for row in result.get("rows", []):
            if row["status"] not in ("ok", "error"):
                row.update(status="error", error=error or result.get("error", "Worker incomplete"))
    return result


def digest(path):
    with Path(path).open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def load_manifest(path):
    data = json.loads(path.read_text())
    seen = set()
    for clip in data["clips"]:
        if clip["id"] in seen: raise ValueError("Duplicate clip id")
        seen.add(clip["id"])
        audio = (path.parent / clip["path"]).resolve()
        if not audio.is_relative_to(path.parent.resolve()): raise ValueError("Audio path escapes corpus")
        if not audio.is_file(): raise ValueError(f"Missing {clip['id']}; run evals/prepare.py first")
        if digest(audio) != clip["sha256"]: raise ValueError(f"Audio hash mismatch: {clip['id']}")
        with wave.open(str(audio), "rb") as wav:
            if wav.getframerate() != 16000 or wav.getnchannels() != 1 or wav.getsampwidth() != 2:
                raise ValueError(f"Audio must be mono 16kHz PCM16: {clip['id']}")
            duration = wav.getnframes() / wav.getframerate()
            if duration <= 0 or abs(duration - clip["duration_s"]) > 1 / 16000:
                raise ValueError(f"Empty audio or duration mismatch: {clip['id']}")
        clip["absolute_path"] = str(audio)
    return data


class Memory:
    """10ms sampled RSS plus monotonic OS process peak; neither is Metal memory."""
    def __init__(self):
        import psutil
        self.process = psutil.Process()
        self.peak = self.process.memory_info().rss
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stop.wait(0.01): self.peak = max(self.peak, self.rss())

    def rss(self): return self.process.memory_info().rss
    def __enter__(self): self.thread.start(); return self
    def __exit__(self, *_): self.stop.set(); self.thread.join(); self.peak = max(self.peak, self.rss())


def artifacts(directory):
    directory = Path(directory).expanduser().resolve()
    if not directory.is_dir(): raise ValueError(f"Install model first: {directory}")
    files = [p for p in sorted(directory.rglob("*")) if p.is_file() and not p.name.startswith(".")]
    if not files: raise ValueError("Empty model directory")
    return directory, dict(files=[dict(name=str(p.relative_to(directory)), bytes=p.stat().st_size,
                                      sha256=digest(p)) for p in files],
                           disk_bytes=sum(p.stat().st_size for p in files))


def worker(config, clips, repeats, checkpoint=lambda _: None):
    started = time.perf_counter()
    rows = [{**{k: v for k, v in clip.items() if k != "absolute_path"}, "status": "pending"} for clip in clips]
    result = dict(model={k: v for k, v in config.items() if k != "path"}, rows=rows, complete=False)
    def progress(phase, clip=None, repeat=None):
        result["progress"] = dict(phase=phase, clip=clip, repeat=repeat, monotonic_s=time.monotonic())
        checkpoint(result)
    progress("load")
    import numpy as np
    import soundfile as sf
    mx = None
    backend = config["backend"]
    path, model_artifacts = artifacts(config["path"]) if "path" in config else (None, None)
    baseline = Memory().rss()
    with Memory() as load_memory:
        begin = time.perf_counter()
        if backend == "qwen-mlx":
            import mlx.core as mx
            from mlx_audio.stt.utils import load_model
            model = load_model(path, strict=True)
            mx.synchronize()
        elif backend == "moonshine":
            from moonshine_voice import Transcriber
            from moonshine_voice.moonshine_api import ModelArch
            model = Transcriber(path, model_arch=ModelArch[config["arch"]])
        elif backend == "command":
            # An explicit local adapter: argv receives WAV path and language.
            # Adapter stdout must be {"text": ...}; no shell or reference text.
            model = None
        else: raise ValueError(f"Unknown backend: {backend}")
        load_s = time.perf_counter() - begin
    resident = load_memory.rss()
    result.update(artifacts=model_artifacts, load_s=load_s)
    def recognize(clip):
        if backend == "qwen-mlx":
            # Same path-based API as the shipped runner, including audio decode.
            # No prompt, dictionary or reference transcript reaches the engine.
            result = model.generate(clip["absolute_path"], temperature=0.0, max_tokens=8192, verbose=False)
            mx.synchronize()
            return result.text
        if backend == "moonshine":
            audio, rate = sf.read(clip["absolute_path"], dtype="float32")
            if rate != 16000 or audio.ndim != 1: raise ValueError("Corpus must be mono 16kHz")
            return " ".join(line.text for line in model.transcribe_without_streaming(audio.tolist(), rate).lines)
        return command_text([*config["argv"], clip["absolute_path"], clip["language"]], config.get("timeout_s", 120))
    # The same anchor is the first-ever inference in every new model process.
    anchor = next((c for c in clips if c["group"] == "human-clean"), clips[0])
    progress("first-inference", anchor["id"])
    with Memory() as cold_memory:
        begin = time.perf_counter()
        cold_text = recognize(anchor)
        cold_s = time.perf_counter() - begin
    result["cold"] = dict(anchor=anchor["id"], inference_s=cold_s, transcript=cold_text, rss_peak_sampled_bytes=cold_memory.peak,
                          note="Fresh process, warm OS file cache: artifact verification reads every model file before load.")
    order = list(clips)
    random.Random(20260910).shuffle(order)
    for clip in order:
        row = next(r for r in rows if r["id"] == clip["id"])
        outputs, timings, peaks, metal_peaks = [], [], [], []
        for repeat in range(repeats):
            progress("recognition", clip["id"], repeat)
            try:
                if mx: mx.reset_peak_memory()
                with Memory() as memory:
                    begin = time.perf_counter(); text = recognize(clip); elapsed = time.perf_counter() - begin
                outputs.append(text); timings.append(elapsed); peaks.append(memory.peak)
                if mx: metal_peaks.append(mx.get_peak_memory())
            except Exception as error:
                row.update(status="error", error=f"{type(error).__name__}: {error}")
                if isinstance(error, subprocess.CalledProcessError): row["adapter_stderr"] = (error.stderr or "")[-4096:]
                break
        else:
            row.update(status="ok", transcript=outputs[0], score=score(clip["reference"], outputs[0], clip.get("terms", [])),
                       median_s=percentile(timings, 50), rtf=percentile(timings, 50) / clip["duration_s"])
        row.update(outputs=outputs, timings_s=timings, rss_peak_sampled_bytes=max(peaks, default=None),
                   metal_peak_allocated_bytes=max(metal_peaks, default=None),
                   repeat_agreement=len(set(outputs)) <= 1)
        progress("scoring-complete", clip["id"])
        print(f"{config['id']}: {clip['id']} {row['status']} {timings}", file=sys.stderr, flush=True)
    process_peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    before_unload = Memory().rss()
    if backend == "moonshine": model.close()
    del model
    gc.collect()
    if mx: mx.clear_cache(); mx.synchronize()
    after_unload = Memory().rss()
    versions = {}
    for package in ("numpy", "soundfile", "psutil", "mlx", "mlx-metal", "mlx-audio", "moonshine-voice", "transformers", "huggingface-hub"):
        try: versions[package] = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError: pass
    result.update(versions=versions, complete=True,
                non_warm_overhead_s=time.perf_counter() - started - sum(sum(r["timings_s"]) for r in rows),
                memory=dict(baseline_rss_bytes=baseline, loaded_rss_bytes=resident, load_peak_rss_bytes=load_memory.peak,
                            process_peak_rss_bytes=process_peak, before_unload_rss_bytes=before_unload, after_unload_rss_bytes=after_unload,
                            note="RSS, Metal allocated bytes, and model disk bytes are different measures; do not add or substitute them. Command adapter child RSS is not captured."),
                rows=sorted(rows, key=lambda r: r["id"]))
    progress("complete")
    return result


def summarize(results):
    summaries = {}
    for result in results:
        if "rows" not in result:
            summaries[result["model"]["id"]] = {"error": result.get("error", "Worker produced no clip results")}; continue
        groups = sorted({(r["group"], r["language"]) for r in result["rows"]})
        summaries[result["model"]["id"]] = {f"{g}/{lang}": aggregate([r for r in result["rows"] if (r["group"], r["language"]) == (g, lang)]) for g, lang in groups}
    paired = {}
    for a, b in combinations([r for r in results if "rows" in r], 2):
        paired[f"{a['model']['id']} minus {b['model']['id']}"] = paired_human_interval(a["rows"], b["rows"])
    return dict(strata=summaries, paired_human_wer=paired)


def markdown(report):
    lines = ["# OpenFlow ASR comparison", "", f"Corpus SHA-256: `{report['manifest_sha256']}`", "",
             "Local offline recognition, not app end-to-end latency. Synthetic/derived clips are separate strata, not extra independent readers.", "",
             "| Model | Stratum | Coverage | WER | CER | p50 / p95 seconds | RTF |", "|---|---|---:|---:|---:|---:|---:|"]
    percent = lambda n: "n/a" if n is None else f"{100*n:.2f}%"
    number = lambda n: "n/a" if n is None else f"{n:.3f}"
    for model, strata in report["summary"]["strata"].items():
        if "error" in strata: lines.append(f"| {model} | FAILED | 0 | n/a | n/a | n/a | n/a |"); continue
        for name, row in strata.items():
            lines.append(f"| {model} | {name} | {row['successful']}/{row['clips']} | {percent(row['wer'])} | {percent(row['cer'])} | {number(row['latency_p50_s'])} / {number(row['latency_p95_s'])} | {number(row['rtf'])} |")
    lines += ["", "## Completion and failures", ""]
    for result in report["results"]:
        failures = [r for r in result.get("rows", []) if r["status"] != "ok"]
        error = result.get("error")
        status = error or ("incomplete" if result.get("complete") is False else "completed")
        lines.append(f"- {result['model']['id']}: {status}; {len(failures)} unsuccessful clips.")
        for row in failures: lines.append(f"  - {row['id']}: {row.get('error', row['status'])}")
    lines += ["", "## Fresh-process startup and memory", "", "File hashes are verified before loading, warming the OS file cache. These are not disk-cold startup measurements. RSS is not Metal allocated memory.", "", "| Model | Load s | First inference s | Loaded RSS MiB | OS peak RSS MiB | After unload RSS MiB |", "|---|---:|---:|---:|---:|---:|"]
    for r in report["results"]:
        if "memory" not in r: continue
        m = r["memory"]
        lines.append(f"| {r['model']['id']} | {r['load_s']:.3f} | {r['cold']['inference_s']:.3f} | {m['loaded_rss_bytes']/2**20:.1f} | {m['process_peak_rss_bytes']/2**20:.1f} | {m['after_unload_rss_bytes']/2**20:.1f} |")
    lines += ["", "Metal allocated peaks during warm inference (not total footprint; not additive with RSS):", ""]
    for r in report["results"]:
        peaks = [row["metal_peak_allocated_bytes"] for row in r.get("rows", []) if row.get("metal_peak_allocated_bytes") is not None]
        if peaks: lines.append(f"- {r['model']['id']}: {max(peaks)/2**20:.1f} MiB.")
    lines += ["", "## Critical terms and non-speech", "", "Aliases are fixed before recognition; they do not replace the stricter WER score.", "", "| Model | Stratum | Term recall | Non-speech false positives |", "|---|---|---:|---:|"]
    for model, strata in report["summary"]["strata"].items():
        if "error" in strata: continue
        for name, row in strata.items():
            if row["term_recall"] is not None or row["silence_false_positive_rate"] is not None:
                lines.append(f"| {model} | {name} | {percent(row['term_recall'])} | {percent(row['silence_false_positive_rate'])} |")
    lines += ["", "## Paired human accuracy uncertainty", "", "WER difference, with 95% speaker-cluster bootstrap interval; negative favors the first model. Small corpus, not population-level proof.", ""]
    for pair, values in report["summary"]["paired_human_wer"].items():
        if values: lines.append(f"- {pair}: {percent(values['delta_wer'])} [{percent(values['low'])}, {percent(values['high'])}], {values['speaker_clusters']} readers, {values['paired_clips']} successful shared clips (complete-case comparison).")
    lines += ["", "See JSON for raw transcripts, critical-term recall, non-speech hallucinations, per-repeat timings, file hashes, versions and failures.", "",
              "Do not use pooled averages to hide unsupported languages, number/name mistakes or failed clips. WER retains fillers and does not equate digit formatting with written numbers; critical-term aliases are scored separately. Model selection needs real dictation and phone measurements before changing defaults."]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--models", type=Path, default=ROOT / "models.local.json")
    parser.add_argument("--out", type=Path, default=ROOT / "results/comparison")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--limit", type=int)
    parser.add_argument("--model", action="append")
    parser.add_argument("--worker", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.worker:
        job = json.loads(args.worker.read_text())
        try: result = worker(job["model"], job["clips"], job["repeats"], lambda data: write_json(args.out, data))
        except Exception as error:
            result = json.loads(args.out.read_text()) if args.out.exists() else dict(model={"id": job["model"]["id"]})
            result.update(error=f"{type(error).__name__}: {error}", complete=False)
        write_json(args.out, result)
        return
    if args.repeats < 1 or (args.limit is not None and args.limit < 1): parser.error("repeats and limit must be positive")
    manifest = load_manifest(args.manifest)
    configs = json.loads(args.models.read_text())["models"]
    if len({m['id'] for m in configs}) != len(configs): raise ValueError("Duplicate model IDs")
    if args.model:
        unknown = set(args.model) - {m["id"] for m in configs}
        if unknown: raise ValueError(f"Unknown models: {unknown}")
        configs = [m for m in configs if m["id"] in args.model]
    if not configs: raise ValueError("No models selected")
    args.out.mkdir(parents=True, exist_ok=True)
    clips = manifest["clips"][:args.limit] if args.limit else manifest["clips"]
    if not clips: raise ValueError("Empty corpus")
    for config in configs:
        for key in ("load_timeout_s", "clip_timeout_s", "suite_timeout_s"):
            if key in config and config[key] <= 0: raise ValueError(f"{key} must be positive")
    results = []
    for index, config in enumerate(configs):
        job = args.out / f"job-{index}.json"
        result_path = args.out / f"model-{index}.json"
        # Never inherit checkpoints from an earlier run of a different model.
        write_json(result_path, dict(model={"id": config["id"]}, complete=False))
        job.write_text(json.dumps(dict(model=config, clips=clips, repeats=args.repeats)))
        env = dict(os.environ, HF_HUB_OFFLINE="1", HF_DATASETS_OFFLINE="1", TOKENIZERS_PARALLELISM="false")
        # argv config is explicitly supplied local executable code, never taken
        # from corpus text. Model downloads are a separate, opt-in operation.
        result = supervised_worker([sys.executable, str(Path(__file__).resolve()), "--worker", str(job), "--out", str(result_path)], result_path, config, env)
        write_json(result_path, result)
        results.append(result)
    report = dict(schema_version=1, normalization=NORMALIZATION, manifest_sha256=digest(args.manifest),
                  created_at=datetime.now(timezone.utc).isoformat(),
                  harness_sha256={name: digest(ROOT / name) for name in ("run.py", "metrics.py")},
                  measurement_notes=["Fresh isolated processes, fixed model order; not a randomized crossover latency trial.",
                                     "Model files read for SHA-256 before load: OS file cache is warm.",
                                     "non_warm_overhead_s includes file hashing, imports, first inference, scoring, checkpoint I/O and unload; it is not startup latency."],
                  host=dict(platform=platform.platform(), machine=platform.machine(), python=sys.version,
                            cpu_count=os.cpu_count()), repeats=args.repeats, subset_limit=args.limit,
                  results=results, summary=summarize(results))
    (args.out / "comparison.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    (args.out / "comparison.md").write_text(markdown(report))
    print(args.out / "comparison.md")
    if any("error" in r or any(c["status"] != "ok" for c in r.get("rows", [])) for r in results): sys.exit(1)


if __name__ == "__main__": main()
