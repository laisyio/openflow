"""Dependency-free, versioned scoring. No LLM judges or reference hints."""
import math
import random
import unicodedata

NORMALIZATION = "nfkc-casefold-punctuation-v1; numbers NOT rewritten; fillers retained"


def normalize(text):
    text = unicodedata.normalize("NFKC", text).casefold().replace("’", "'")
    return " ".join("".join(c if c.isalnum() or c == "'" else " " for c in text).split())


def edits(reference, hypothesis):
    """Levenshtein S/D/I counts; deterministic substitution-first tie break."""
    previous = [(j, 0, 0, j) for j in range(len(hypothesis) + 1)]
    for i, word in enumerate(reference, 1):
        row = [(i, 0, i, 0)]
        for j, actual in enumerate(hypothesis, 1):
            if word == actual:
                row.append(previous[j - 1])
            else:
                cost, sub, delete, insert = previous[j - 1]
                a = (cost + 1, sub + 1, delete, insert)
                cost, sub, delete, insert = previous[j]
                b = (cost + 1, sub, delete + 1, insert)
                cost, sub, delete, insert = row[-1]
                c = (cost + 1, sub, delete, insert + 1)
                row.append(min((a, b, c), key=lambda value: value[0]))
        previous = row
    errors, substitutions, deletions, insertions = previous[-1]
    return dict(errors=errors, substitutions=substitutions, deletions=deletions,
                insertions=insertions, reference_count=len(reference))


def score(reference, hypothesis, terms=()):
    ref, hyp = normalize(reference), normalize(hypothesis)
    word = edits(ref.split(), hyp.split())
    char = edits(list(ref.replace(" ", "")), list(hyp.replace(" ", "")))
    def contains(alias):
        value = normalize(alias)
        # CJK reference words need no spaces in a correctly rendered sentence.
        if any("\u3400" <= char <= "\u9fff" for char in value): return value in hyp
        return f" {value} " in f" {hyp} "
    matched = sum(any(contains(alias) for alias in alternatives) for alternatives in terms)
    return dict(word=word, char=char, wer=word["errors"] / word["reference_count"] if ref else None,
                cer=char["errors"] / char["reference_count"] if ref else None,
                exact=ref == hyp, terms_matched=matched, terms_total=len(terms),
                speech_on_empty=not ref and bool(hyp))


def percentile(values, percent):
    if not values: return None
    ordered = sorted(values)
    index = (len(ordered) - 1) * percent / 100
    lower, upper = math.floor(index), math.ceil(index)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (index - lower)


def aggregate(rows):
    good = [r for r in rows if r.get("status") == "ok"]
    def ratio(errors, denominator):
        total = sum(r["score"][denominator] for r in good)
        return sum(r["score"][errors] for r in good) / total if total else None
    words = sum(r["score"]["word"]["reference_count"] for r in good)
    chars = sum(r["score"]["char"]["reference_count"] for r in good)
    speech = [r for r in good if r["reference"].strip()]
    empty = [r for r in good if not r["reference"].strip()]
    return dict(clips=len(rows), successful=len(good), failed=len(rows) - len(good),
                wer=sum(r["score"]["word"]["errors"] for r in speech) / words if words else None,
                cer=sum(r["score"]["char"]["errors"] for r in speech) / chars if chars else None,
                term_recall=ratio("terms_matched", "terms_total"),
                silence_false_positive_rate=sum(r["score"]["speech_on_empty"] for r in empty) / len(empty) if empty else None,
                latency_p50_s=percentile([r["median_s"] for r in good], 50),
                latency_p95_s=percentile([r["median_s"] for r in good], 95),
                rtf=sum(r["median_s"] for r in good) / sum(r["duration_s"] for r in good) if good else None,
                reference_words=words)


def paired_human_interval(a, b, iterations=2000):
    """Paired speaker-cluster bootstrap of WER(A)-WER(B), clean human only.

    Related stress/stitched/synthetic clips never increase the sample size.
    """
    left = {r["id"]: r for r in a if r["status"] == "ok" and r["group"] == "human-clean"}
    right = {r["id"]: r for r in b if r["status"] == "ok" and r["group"] == "human-clean"}
    pairs = [(left[key], right[key]) for key in sorted(left.keys() & right.keys())]
    speakers = sorted({l["speaker"] for l, _ in pairs})
    if len(speakers) < 2: return None
    clusters = {s: [(l, r) for l, r in pairs if l["speaker"] == s] for s in speakers}
    def delta(sample):
        denominator = sum(l["score"]["word"]["reference_count"] for l, _ in sample)
        return sum(l["score"]["word"]["errors"] - r["score"]["word"]["errors"] for l, r in sample) / denominator
    rng = random.Random(20260910)
    samples = [delta([p for s in rng.choices(speakers, k=len(speakers)) for p in clusters[s]]) for _ in range(iterations)]
    return dict(delta_wer=delta(pairs), low=percentile(samples, 2.5), high=percentile(samples, 97.5),
                paired_clips=len(pairs), speaker_clusters=len(speakers), bootstrap_iterations=iterations)
