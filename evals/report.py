#!/usr/bin/env python3
"""Re-render a saved experiment without rerunning or altering raw recognition."""
import argparse
import copy
from datetime import datetime, timezone
import json
from pathlib import Path
from run import digest, markdown, summarize, write_json


def render(source, output, notes=(), executed_harness=None):
    report = copy.deepcopy(json.loads(source.read_text()))
    report["report_provenance"] = dict(input_sha256=digest(source), rendered_at=datetime.now(timezone.utc).isoformat(),
                                     renderer_sha256={name: digest(Path(__file__).with_name(name)) for name in ("report.py", "run.py", "metrics.py")})
    if executed_harness: report["report_provenance"]["executed_harness_sha256"] = digest(executed_harness)
    report.setdefault("measurement_notes", []).extend(notes)
    for result in report["results"]:
        # Legacy field was never pure startup: preserve the exact measured
        # number under a name that describes its actual calculation.
        if "process_setup_s" in result: result["non_warm_overhead_s"] = result.pop("process_setup_s")
    report["summary"] = summarize(report["results"])
    output.mkdir(parents=True, exist_ok=True)
    write_json(output / "comparison.json", report)
    text = markdown(report)
    if report.get("measurement_notes"):
        text += "\n## Measurement notes\n\n" + "\n".join("- " + note for note in report["measurement_notes"]) + "\n"
    (output / "comparison.md").write_text(text)
    print(output / "comparison.md")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("comparison", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--note", action="append", default=[])
    parser.add_argument("--executed-harness", type=Path)
    args = parser.parse_args()
    render(args.comparison, args.out, args.note, args.executed_harness)
