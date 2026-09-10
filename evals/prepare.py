#!/usr/bin/env python3
"""Build a pinned multi-speaker corpus without extracting untrusted tar paths.

Human audio is CC-BY-4.0 LibriSpeech test-clean. Synthetic prompts are original;
macOS speech output stays in ignored local generated/, never a released fixture.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import random
import subprocess
import tarfile
import tempfile

ARCHIVE_MD5 = "32fa31d27d2e1cad72775fee3f4849a9"
SOURCE = "https://www.openslr.org/12/"
ROOT = Path(__file__).resolve().parent


def digest(path, algorithm="sha256"):
    with Path(path).open("rb") as handle:
        return hashlib.file_digest(handle, algorithm).hexdigest()


def prepare(archive, out, synthetic=True, human_only=False):
    import numpy as np
    import soundfile as sf
    if digest(archive, "md5") != ARCHIVE_MD5:
        raise ValueError("LibriSpeech archive does not match the publisher's checksum")
    out.mkdir(parents=True, exist_ok=True)
    human = out / "human"
    generated = out / "generated"
    human.mkdir(exist_ok=True)
    generated.mkdir(exist_ok=True)
    entries = []

    def add(identifier, samples, reference, group, **meta):
        if len(samples) < 1600:
            raise ValueError(f"Empty/too-short audio for {identifier}; macOS say may need audio-service access outside a sandbox")
        directory = human if group == "human-clean" else generated
        path = directory / f"{identifier}.wav"
        sf.write(path, samples, 16000, subtype="PCM_16")
        entry = dict(id=identifier, path=str(path.relative_to(out)), sha256=digest(path),
                     duration_s=len(samples) / 16000, reference=reference, group=group,
                     language="en", tags=[], terms=[], **meta)
        entries.append(entry)
        return entry

    with tarfile.open(archive, "r:gz") as tar:
        members = {m.name: m for m in tar.getmembers() if m.isfile()}
        def read(name):
            member = members[name]
            if member.size > 20 * 1024 * 1024:
                raise ValueError("Unexpectedly large corpus member")
            return tar.extractfile(member).read()
        speakers = {}
        for line in read("LibriSpeech/SPEAKERS.TXT").decode().splitlines():
            if not line or line.startswith(";"): continue
            fields = [p.strip() for p in line.split("|")]
            if len(fields) >= 5 and fields[2] == "test-clean":
                speakers[fields[0]] = dict(sex=fields[1], reader=fields[4])
        # Six speakers in each publisher-provided sex category, independent of
        # transcript or recognizer performance. Two seeded random clips each.
        selected = []
        rng = random.Random(20260910)
        for sex in ("F", "M"):
            pool = sorted(s for s, metadata in speakers.items() if metadata["sex"] == sex)
            selected.extend(rng.sample(pool, 6))
        for speaker in selected:
            prefixes = f"LibriSpeech/test-clean/{speaker}/"
            transcript = {}
            for name in members:
                if name.startswith(prefixes) and name.endswith(".trans.txt"):
                    for line in read(name).decode().splitlines():
                        identifier, text = line.split(" ", 1); transcript[identifier] = text
            candidates = sorted(n for n in members if n.startswith(prefixes) and n.endswith(".flac"))
            rng.shuffle(candidates)
            count = 0
            for name in candidates:
                encoded = read(name)
                audio, rate = sf.read(io.BytesIO(encoded), dtype="float32")
                if rate != 16000 or audio.ndim != 1 or not 3 <= len(audio) / rate <= 16: continue
                identifier = Path(name).stem
                add("ls-" + identifier, audio, transcript[identifier], "human-clean",
                    source=SOURCE, source_member=name, source_sha256=hashlib.sha256(encoded).hexdigest(),
                    speaker=speaker, reader=speakers[speaker]["reader"], sex=speakers[speaker]["sex"],
                    license="CC-BY-4.0", transforms=["FLAC decoded to 16-bit PCM WAV; no trimming"])
                count += 1
                if count == 2: break
            if count != 2: raise ValueError(f"Not enough eligible clips for {speaker}")
        (out / "LIBRISPEECH-LICENSE.txt").write_bytes(read("LibriSpeech/LICENSE.TXT"))

    # Derived cases stay in their own strata and are not independent speakers.
    clean = list(entries)
    metadata = dict(schema_version=1, seed=20260910,
                    selection="six F and six M readers sampled before recognition; two 3–16s utterances each",
                    source_archive_sha256=digest(archive), sample_rate=16000)
    # A fresh checkout can run this manifest without generating macOS voices or
    # downloading the full archive; all 24 licensed WAVs are committed.
    (out / "human-manifest.json").write_text(json.dumps(dict(metadata, clips=clean), ensure_ascii=False, indent=2) + "\n")
    if human_only: return
    for index, mode in enumerate(("noise-10db", "quiet", "clipped", "leading-silence", "noise-5db", "trailing-silence")):
        source = clean[index * 3]
        audio, _ = sf.read(out / source["path"], dtype="float32")
        if mode.startswith("noise"):
            snr = int(mode.split("-")[1].replace("db", ""))
            noise = np.random.default_rng(20260910 + index).normal(size=len(audio))
            noise *= np.sqrt(np.mean(audio ** 2)) / (10 ** (snr / 20) * np.sqrt(np.mean(noise ** 2)))
            audio = np.clip(audio + noise, -1, 1)
        elif mode == "quiet": audio *= 0.025
        elif mode == "clipped": audio = np.clip(audio * 8, -1, 1)
        elif mode == "leading-silence": audio = np.concatenate([np.zeros(32000), audio])
        else: audio = np.concatenate([audio, np.zeros(48000)])
        add(f"stress-{mode}", audio, source["reference"], "human-derived", source_id=source["id"],
            speaker=source["speaker"], license="CC-BY-4.0", transforms=[mode])
    for seconds in (60, 180):
        chunks, refs, ids = [], [], []
        length = 0
        for source in clean * 3:
            audio, _ = sf.read(out / source["path"], dtype="float32")
            chunks.extend([audio, np.zeros(4000)]); refs.append(source["reference"]); ids.append(source["id"])
            length += len(audio) + 4000
            if length >= seconds * 16000: break
        add(f"long-{seconds}s", np.concatenate(chunks), " ".join(refs), "human-stitched",
            source_ids=ids, license="CC-BY-4.0", transforms=["whole utterances joined with 250ms silence; not a natural conversation"])
    for name, audio in [("silence", np.zeros(80000)),
                        ("noise-only", np.random.default_rng(19).normal(0, 0.008, 80000)),
                        ("tone-only", 0.04 * np.sin(np.arange(48000) * 2 * np.pi * 440 / 16000))]:
        add(name, audio, "", "non-speech", license="MIT", transforms=[name])
    if synthetic:
        for prompt in json.loads((ROOT / "prompts.json").read_text()):
            with tempfile.TemporaryDirectory(prefix="openflow-eval-say-") as temp:
                aiff = Path(temp) / "speech.aiff"
                subprocess.run(["say", "-v", prompt["voice"], "-r", "165", "-o", str(aiff), prompt["text"]], check=True, timeout=60)
                wav = Path(temp) / "speech.wav"
                subprocess.run(["afconvert", str(aiff), str(wav), "-f", "WAVE", "-d", "LEI16@16000", "-c", "1"], check=True, timeout=30)
                audio, _ = sf.read(wav, dtype="float32")
            e = add("tts-" + prompt["id"], audio, prompt["text"], "synthetic-dictation",
                    voice=prompt["voice"], source="locally generated macOS say; audio not redistributed",
                    license="local-evaluation-only", transforms=["macOS say at 165 words/minute; afconvert PCM16/16kHz"])
            e.update(language=prompt["language"], tags=prompt["tags"], terms=prompt["terms"])
    manifest = dict(metadata, clips=entries)
    (out / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    print(f"Prepared {len(entries)} clips; {sum(e['duration_s'] for e in entries):.1f}s; 12 human speakers. Manifest: {out / 'manifest.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--librispeech-archive", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=ROOT / "corpus")
    parser.add_argument("--no-synthetic", action="store_true")
    parser.add_argument("--human-only", action="store_true", help="Only rebuild the redistributable 24-clip human corpus; preserve full manifest")
    args = parser.parse_args()
    prepare(args.librispeech_archive, args.out, not args.no_synthetic, args.human_only)
