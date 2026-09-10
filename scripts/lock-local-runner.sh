#!/usr/bin/env bash
# Rewrite crates/openflow-core/requirements/local-runner.txt.
#
# The lock is the resolved closure of the versions runner.rs asks for, with the
# hash of every artifact each package is allowed to come from. Regenerate it
# whenever MLX_AUDIO_VERSION or HUGGINGFACE_HUB_VERSION moves; a test in
# runner.rs fails when the file and those constants disagree.
#
# Needs `uv` (https://astral.sh/uv). uv rather than pip-tools because only a
# universal resolve produces one file that installs on every Python the runner
# will pick: numpy and scipy have each dropped versions the runner still
# supports, so a single pin per package does not exist across that range.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
runner_rs="$here/crates/openflow-core/src/runner.rs"
out="$here/crates/openflow-core/requirements/local-runner.txt"

command -v uv >/dev/null || { echo "uv is not installed: https://astral.sh/uv" >&2; exit 1; }

# The versions are read out of runner.rs rather than repeated here, so this
# script cannot pin something the app does not ask for.
read_const() {
  local value
  value="$(sed -n "s/^pub const $1: &str = \"\\(.*\\)\";$/\\1/p" "$runner_rs")"
  [ -n "$value" ] || { echo "could not read $1 from $runner_rs" >&2; exit 1; }
  printf '%s' "$value"
}
mlx_audio="$(read_const MLX_AUDIO_VERSION)"
hub="$(read_const HUGGINGFACE_HUB_VERSION)"

# The floor of the supported range: a universal resolve covers it and upwards.
minimum="$(sed -n 's/^pub const MINIMUM_PYTHON: (u32, u32) = (\([0-9]*\), \([0-9]*\));$/\1.\2/p' "$runner_rs")"
[ -n "$minimum" ] || { echo "could not read MINIMUM_PYTHON from $runner_rs" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf 'mlx-audio==%s\nhuggingface_hub==%s\n' "$mlx_audio" "$hub" > "$work/local-runner.in"

echo "resolving mlx-audio==$mlx_audio + huggingface_hub==$hub for python >= $minimum"
# From inside $work with a relative path: uv writes the input's path into the
# `# via -r ...` annotations, and an absolute one would put this machine's
# temporary directory in the file and stop it being reproducible.
( cd "$work" && uv pip compile local-runner.in \
  --universal \
  --generate-hashes \
  --python-version "$minimum" \
  --no-header \
  -o lock.txt )

# The header lives here rather than in the file so regenerating cannot lose it.
{
  cat <<'HDR'
# The exact package set `pip` installs into the local runner's virtualenv, with
# the hash of every artifact that set is allowed to come from.
#
# Generated -- do not edit by hand. `scripts/lock-local-runner.sh` rewrites it
# from the versions crates/openflow-core/src/runner.rs asks for, and a test in
# that module fails when the two disagree.
#
# Compiled across every Python the runner will pick (see `SUPPORTED_PYTHONS`),
# which is why some packages appear more than once: `numpy` and `scipy` have
# both dropped Python versions the runner still supports, so the newest release
# that installs on 3.14 does not exist for 3.10. The `python_full_version`
# markers are how one file serves all five.
HDR
  cat "$work/lock.txt"
} > "$out.new"
mv "$out.new" "$out"
echo "wrote $out"
