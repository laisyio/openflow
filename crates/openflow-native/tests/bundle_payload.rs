//! What `scripts/bundle-native.sh` puts inside the bundle, asserted against the
//! script itself rather than against a copy of its list.
//!
//! This exists because the failure it guards is invisible on the machine most
//! likely to cause it. `LocalRunner::script_path` tries, in order, the bundle's
//! `Resources/runner/runner.py`, a `runner/` beside the executable, and finally
//! `crates/openflow-core/src/../openflow-native/runner/runner.py` -- an
//! absolute path baked in at compile time by `env!("CARGO_MANIFEST_DIR")`. That
//! last candidate is what makes a `cargo run` work, and it is also what hides a
//! bundle assembled without the sidecar: on the machine that built it the
//! fallback resolves and on-device transcription works, while every copy handed
//! to anyone else answers "Could not find runner.py. Reinstall OpenFlow." No
//! test that runs the app can see this, because the app under test is always
//! sitting in the tree that satisfies the fallback.
//!
//! So the assertion is on the bundle's manifest instead, from both ends: every
//! path the script says it will copy has to exist, and the sidecar has to land
//! where the executable will look for it. Deleting the entry fails the second;
//! moving or renaming the file without updating the table fails the first.
//!
//! `cfg(unix)`: the script is bash and the bundle is macOS-only, but the
//! workspace's tests also run on a Windows runner, where there is no bash to
//! ask.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the crate sits two levels under the repo root")
}

/// Run `bundle-native.sh --print-payload` and return its `source|destination`
/// pairs. Nothing is built and nothing is written: the flag returns before the
/// first `cargo build`, the same way `--print-artifacts` does.
fn payload() -> Vec<(String, String)> {
    let root = repo_root();
    let output = Command::new("bash")
        .arg(root.join("scripts/bundle-native.sh"))
        .arg("--print-payload")
        .current_dir(&root)
        .output()
        .expect("bash is on the path");
    assert!(
        output.status.success(),
        "--print-payload failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pairs: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (from, to) = line
                .split_once('|')
                .unwrap_or_else(|| panic!("payload line is not `source|destination`: {line:?}"));
            (from.to_string(), to.to_string())
        })
        .collect();
    assert!(
        !pairs.is_empty(),
        "the payload table is empty, so the assertions below would all pass vacuously"
    );
    pairs
}

/// Every file the script promises to copy has to be there to copy. This is the
/// half that catches a rename: moving `runner.py` and updating every `use` and
/// import would leave the build green and the bundle hollow.
#[test]
fn every_file_the_bundle_carries_exists_in_the_tree() {
    let root = repo_root();
    for (from, to) in payload() {
        let source = root.join(&from);
        assert!(
            source.is_file(),
            "the bundle would copy {from} to Contents/{to}, and it is not in the tree"
        );
    }
}

/// The sidecar has to land where the executable will look for it.
///
/// `Contents/MacOS/openflow` is the binary, and `script_path` asks for
/// `../Resources/runner/runner.py` relative to its own directory, so the
/// destination under `Contents` is fixed. This is the half that catches the
/// entry being dropped from the table.
#[test]
fn the_sidecar_lands_where_script_path_looks_for_it() {
    let pairs = payload();
    let sidecar = pairs
        .iter()
        .find(|(from, _)| from.ends_with("runner/runner.py"))
        .unwrap_or_else(|| panic!("nothing in the payload is the runner sidecar: {pairs:?}"));

    assert_eq!(
        sidecar.1, "Resources/runner/runner.py",
        "the executable resolves ../Resources/runner/runner.py from Contents/MacOS, \
         so anywhere else is a bundle that only works where it was built"
    );

    // And it is the sidecar the Python suite covers, not some other copy.
    assert_eq!(
        sidecar.0, "crates/openflow-native/runner/runner.py",
        "the bundled sidecar has to be the one the tests run against"
    );
}

/// A destination that escaped `Contents` would be written outside the bundle,
/// and two entries claiming one path would make the order of the table decide
/// what shipped.
#[test]
fn destinations_stay_inside_the_bundle_and_are_unique() {
    let pairs = payload();
    let mut seen: Vec<&str> = Vec::new();
    for (from, to) in &pairs {
        let path = Path::new(to);
        assert!(
            path.is_relative(),
            "{from} would be copied to the absolute path {to}"
        );
        assert!(
            !to.split('/').any(|part| part == ".."),
            "{from} would be copied out of the bundle, to {to}"
        );
        assert!(
            !seen.contains(&to.as_str()),
            "two payload entries both claim Contents/{to}"
        );
        seen.push(to);
    }
}
