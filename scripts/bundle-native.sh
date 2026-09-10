#!/usr/bin/env bash
# Assemble target/OpenFlow.app around the native binary, sign it ad hoc, and
# optionally wrap it in a DMG.
#
#   bash scripts/bundle-native.sh                   build + bundle + sign
#   bash scripts/bundle-native.sh --dmg             also produce the disk image
#   bash scripts/bundle-native.sh --skip-build
#   bash scripts/bundle-native.sh --print-artifacts version and DMG name, then stop
#
# The DMG is target/OpenFlow_<version>_<arch>.dmg, and --print-artifacts says
# what that name will be without building anything, so a release workflow can
# name the asset it is about to upload without keeping a second copy of the
# rule. There is exactly one source for each half of a build's identity: the
# version comes from crates/openflow-native/Cargo.toml, and the commit is asked
# of the binary itself (`--version`), never recomputed here -- a bundle can then
# never claim a commit its executable was not built from, however stale the tree
# it was assembled in.
#
# "Deterministic" here means the same inputs give the same *name*, layout and
# contents: a fixed volume name, an Applications symlink, no background image,
# no window geometry, and a re-run that overwrites rather than accumulating.
# It does not mean a bit-identical image: hdiutil stamps HFS+ creation times
# into the filesystem it builds, so two runs a second apart differ in bytes.
# Reproducing byte-for-byte would need a different container format than the
# one macOS users expect to double-click.
#
# The signature matters more than it looks: macOS binds microphone and
# accessibility grants to the signature's designated requirement, and an ad hoc
# one can only name itself by code hash, so every rebuild that changes a byte
# asks for both permissions again. Signed with a certificate the requirement
# names the certificate instead and the grants survive. Run
# scripts/local-signing-identity.sh once to install a local one; set
# OPENFLOW_SIGN_IDENTITY to sign with something else.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP="$ROOT/target/OpenFlow.app"
BINARY="$ROOT/target/release/openflow-native"
IDENTIFIER="io.laisy.openflow"

# The manifest and the machine are overridable so the test that guards this
# extraction can point it at a temporary Cargo.toml and ask for a named
# architecture, instead of asserting against whatever version the tree happens
# to carry today. Neither override is meant for a real build.
CARGO_TOML="${OPENFLOW_CARGO_TOML:-$ROOT/crates/openflow-native/Cargo.toml}"
# The first `version = "..."` under [package], which is the crate's own; a
# dependency's version line is always indented or inside a table further down,
# and `head -1` would take it if [package] ever lost its version, so the match
# is anchored to the start of the line and the file is read from the top.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO_TOML" | head -1)"
if [ -z "$VERSION" ]; then
  echo "No version = \"...\" line in $CARGO_TOML" >&2
  exit 1
fi

# The rust triple's word for the machine, not uname's: the DMG name should
# match what `--target` would have been asked for.
if [ -n "${OPENFLOW_DMG_ARCH-}" ]; then
  ARCH="$OPENFLOW_DMG_ARCH"
else
  case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    *) ARCH="$(uname -m)" ;;
  esac
fi
DMG_NAME="OpenFlow_${VERSION}_${ARCH}.dmg"
DMG_PATH="$ROOT/target/$DMG_NAME"

# Everything the bundle carries that comes out of the repository, as
# `<path from the repo root>|<path under Contents>`. The executable is not in
# here: it is built rather than copied, and it is the one file whose absence
# already stops the script.
#
# One table, read twice -- by the copy loop below and by --print-payload -- so
# a resource that stops being copied also stops being reported, and
# crates/openflow-native/tests/bundle_payload.rs notices. Nothing else does:
# `LocalRunner::script_path` falls back to an absolute path into the source
# tree of the machine that compiled the binary, so a bundle assembled without
# runner.py runs correctly for whoever built it and fails for everyone else.
PAYLOAD=(
  "src-tauri/icons/icon.icns|Resources/icon.icns"
  "crates/openflow-native/runner/runner.py|Resources/runner/runner.py"
)

BUILD=1
DMG=0
for argument in "$@"; do
  case "$argument" in
    --dmg) DMG=1 ;;
    --skip-build) BUILD=0 ;;
    --print-artifacts)
      echo "version=$VERSION"
      echo "arch=$ARCH"
      echo "dmg=$DMG_NAME"
      exit 0
      ;;
    --print-payload)
      printf '%s\n' "${PAYLOAD[@]}"
      exit 0
      ;;
    *) echo "Unknown option: $argument" >&2; exit 2 ;;
  esac
done

if [ "$BUILD" = "1" ]; then
  cargo build -p openflow-native --release
fi
if [ ! -x "$BINARY" ]; then
  echo "No binary at $BINARY. Run without --skip-build." >&2
  exit 1
fi

# Ask the binary what it was built from rather than asking git, which would
# answer for the tree as it is now: under --skip-build those are two different
# commits, and the plist has to describe the executable it is shipping beside.
# `--version` prints "OpenFlow <version> (<commit>)" and exits before AppKit,
# the instance lock and the keychain, so this starts nothing.
COMMIT="$("$BINARY" --version | sed -n 's/^OpenFlow .* (\(.*\))$/\1/p')"
COMMIT="${COMMIT:-unknown}"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The bundle executable keeps the plain name; only the cargo target is suffixed.
cp "$BINARY" "$APP/Contents/MacOS/openflow"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# The payload, including the local transcription sidecar:
# `LocalRunner::script_path` looks for it under Resources relative to the
# executable, and falls back to the source tree for a `cargo run`. The
# virtualenv it runs under is *not* here: a venv hard-codes its own absolute
# path, so one inside the bundle would break the first time the app moved and
# be thrown away by every update. It lives beside the database in the app's
# data directory instead.
#
# A missing source stops the script rather than producing a bundle with a hole
# in it, which is the shape this failure has always taken: `cp` of a path that
# does not exist is the one error `set -e` would have caught, and a payload
# entry silently deleted is the one it would not.
for entry in "${PAYLOAD[@]}"; do
  from="$ROOT/${entry%%|*}"
  to="$APP/Contents/${entry##*|}"
  if [ ! -f "$from" ]; then
    echo "Missing bundle payload: $from" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$to")"
  cp "$from" "$to"
done

# Info.plist: the usage strings and LSUIElement come from src-tauri/Info.plist,
# which Tauri merges into its own bundle, so the two builds ask for the same
# permissions with the same words. The bundle keys Tauri generates are added
# here because a hand-assembled bundle has no generator to add them.
USAGE_KEYS="$(/usr/libexec/PlistBuddy -x -c 'Print' "$ROOT/src-tauri/Info.plist" \
  | sed -n '/<dict>/,/<\/dict>/p' | sed '1d;$d')"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>OpenFlow</string>
    <key>CFBundleDisplayName</key>
    <string>OpenFlow</string>
    <key>CFBundleIdentifier</key>
    <string>${IDENTIFIER}</string>
    <key>CFBundleExecutable</key>
    <string>openflow</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <!-- Not a CFBundle key, and deliberately so: CFBundleVersion is the build
         number macOS compares between installs, so hiding a commit hash in it
         would make every build look like a downgrade. This one is only ever
         read by a human asking which tree an installed app came from. -->
    <key>OpenFlowCommit</key>
    <string>${COMMIT}</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
${USAGE_KEYS}
</dict>
</plist>
PLIST

/usr/bin/plutil -lint "$APP/Contents/Info.plist" > /dev/null

# An explicit override first, then the local identity, then ad hoc -- which
# still produces a working bundle, just one whose permissions expire with the
# build. Note the ordering: this never *creates* an identity, because a build
# quietly minting a signing certificate is worse than a build that says why it
# could not.
IDENTITY_NAME="${OPENFLOW_SIGN_IDENTITY-}"
if [ -z "$IDENTITY_NAME" ]; then
  IDENTITY_NAME="$(bash "$ROOT/scripts/local-signing-identity.sh" --check 2>/dev/null || true)"
fi
if [ -z "$IDENTITY_NAME" ]; then
  IDENTITY_NAME="-"
  echo "warning: no signing identity, falling back to ad hoc. Microphone and" >&2
  echo "         accessibility will have to be granted again after this build." >&2
  echo "         Run: bash scripts/local-signing-identity.sh" >&2
fi

codesign --force --deep --sign "$IDENTITY_NAME" \
  --entitlements "$ROOT/src-tauri/entitlements.plist" \
  --options runtime \
  "$APP"

# The requirement is the whole point of the exercise, so say what came out:
# `certificate root` keeps its grants across rebuilds, `cdhash` does not. An ad
# hoc signature has no stored requirement, only the one codesign derives, which
# it marks with a leading "# " -- hence the optional prefix in the match.
codesign --display --requirements - "$APP" 2>/dev/null \
  | sed -n 's/^#\{0,1\} *designated => /signed: /p' >&2

if [ "$DMG" = "1" ]; then
  # A staged folder, not -srcfolder on the .app: the image has to hold the
  # Applications symlink next to the app, which is the whole of the install
  # instruction a Mac user needs and the reason not to add a background image
  # explaining it.
  STAGE="$(mktemp -d)"
  trap 'rm -rf "$STAGE"' EXIT
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  # Both the current name and the one a previous version of this script wrote,
  # so a re-run leaves one image in target/ rather than a museum of them.
  rm -f "$DMG_PATH" "$ROOT/target/OpenFlow.dmg"
  # UDZO: zlib-compressed and read-only, which is what every other Mac download
  # is and what Gatekeeper checks in one pass. -ov because the interesting
  # failure is a half-written image from an interrupted run, not a name clash.
  hdiutil create -volname OpenFlow -srcfolder "$STAGE" -ov -format UDZO \
    "$DMG_PATH" > /dev/null
  rm -rf "$STAGE"
  trap - EXIT

  # Path then checksum, so a local build can be compared against the one the
  # release page publishes without a second command. The workflow writes the
  # same number into a .sha256 asset in shasum's own `hash  name` format,
  # because that is the form `shasum -c` reads back.
  echo "$DMG_PATH"
  shasum -a 256 "$DMG_PATH" | awk '{print $1}'
fi

echo "$APP"
