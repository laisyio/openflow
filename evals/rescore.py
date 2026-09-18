#!/usr/bin/env python3
"""Score the real app dictionary post-pass without contaminating raw ASR scores."""
import argparse
import copy
import json
from pathlib import Path
import subprocess
from metrics import score
from run import digest, markdown, summarize


def rescore(report, executable, dictionary):
    if "postpass" in report: raise ValueError("Use the original raw comparison, not an already post-processed report")
    corrected = copy.deepcopy(report)
    rows = [row for result in corrected["results"] for row in result.get("rows", []) if row["status"] == "ok"]
    payload = "".join(json.dumps(dict(text=row["transcript"], dictionary=dictionary)) + "\n" for row in rows)
    result = subprocess.run([str(executable.resolve())], input=payload, text=True, capture_output=True, check=True, timeout=60)
    lines = result.stdout.splitlines()
    if len(lines) != len(rows): raise ValueError("Post-pass adapter returned the wrong number of rows")
    for row, line in zip(rows, lines):
        row["raw_transcript"] = row["transcript"]
        row["raw_score"] = row["score"]
        row["raw_repeat_agreement"] = row.pop("repeat_agreement")
        row["transcript"] = json.loads(line)
        row["score"] = score(row["reference"], row["transcript"], row.get("terms", []))
    corrected["summary"] = summarize(corrected["results"])
    corrected["postpass"] = dict(dictionary=dictionary, executable_sha256=digest(executable),
                                  scoring_sha256={name: digest(Path(__file__).with_name(name)) for name in ("rescore.py", "metrics.py")},
                                  note="Scoring only. All timings remain RAW recognition; dictionary is never provided to models.")
    return corrected


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("comparison", type=Path)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--dictionary", type=Path, default=Path(__file__).with_name("dictionary.txt"))
    args = parser.parse_args()
    result = rescore(json.loads(args.comparison.read_text()), args.executable, args.dictionary.read_text())
    output = args.comparison.with_name("comparison-postpass.json")
    output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    text = markdown(result).replace("# OpenFlow ASR comparison", "# OpenFlow ASR + dictionary comparison", 1)
    text += "\n## Post-pass scope\n\n" + result["postpass"]["note"] + " Raw outputs and agreement are preserved in JSON; only the first output per clip is corrected.\n"
    if result.get("measurement_notes"):
        text += "\n## Measurement notes\n\n" + "\n".join("- " + note for note in result["measurement_notes"]) + "\n"
    output.with_suffix(".md").write_text(text)
    print(output)
