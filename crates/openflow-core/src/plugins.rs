use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub hooks: Vec<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HookPayload {
    pub raw_text: Option<String>,
    pub formatted_text: Option<String>,
    pub provider: Option<String>,
    pub language: Option<String>,
}

pub struct PluginManager {
    plugins_dir: PathBuf,
}

impl PluginManager {
    // The move into the library crate makes this constructor public API, which
    // is the only reason clippy now asks for a `Default`. Adding one would be a
    // new API, not a move.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let plugins_dir = dirs_next::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".openflow")
            .join("plugins");

        let _ = std::fs::create_dir_all(&plugins_dir);

        Self { plugins_dir }
    }

    /// Where plugins live. A host needs this to reveal the folder and to read a
    /// manifest out of a directory the user picked; the field itself stays
    /// private so the path is still owned here.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        let mut plugins = Vec::new();

        let entries = match std::fs::read_dir(&self.plugins_dir) {
            Ok(e) => e,
            Err(_) => return plugins,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }

            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let manifest: PluginManifest = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let enabled_path = path.join(".enabled");
            plugins.push(PluginInfo {
                manifest,
                enabled: enabled_path.exists(),
                path: path.to_string_lossy().to_string(),
            });
        }

        plugins
    }

    pub fn enable_plugin(&self, id: &str) -> Result<(), String> {
        validate_plugin_id(id)?;
        let plugin_dir = self.plugins_dir.join(id);
        if !plugin_dir.exists() {
            return Err(format!("Plugin '{}' not found", id));
        }
        std::fs::write(plugin_dir.join(".enabled"), "")
            .map_err(|e| format!("Failed to enable: {}", e))
    }

    pub fn disable_plugin(&self, id: &str) -> Result<(), String> {
        validate_plugin_id(id)?;
        let plugin_dir = self.plugins_dir.join(id);
        let enabled_path = plugin_dir.join(".enabled");
        if enabled_path.exists() {
            std::fs::remove_file(enabled_path).map_err(|e| format!("Failed to disable: {}", e))?;
        }
        Ok(())
    }

    pub fn get_enabled_hooks(&self, hook_name: &str) -> Vec<PluginInfo> {
        self.list_plugins()
            .into_iter()
            .filter(|p| p.enabled && p.manifest.hooks.contains(&hook_name.to_string()))
            .collect()
    }

    pub fn install_plugin(&self, manifest_json: &str) -> Result<PluginInfo, String> {
        let manifest: PluginManifest =
            serde_json::from_str(manifest_json).map_err(|e| format!("Invalid manifest: {}", e))?;
        validate_manifest(&manifest)?;

        let plugin_dir = self.plugins_dir.join(&manifest.id);
        std::fs::create_dir_all(&plugin_dir)
            .map_err(|e| format!("Failed to create plugin dir: {}", e))?;

        // Before the new code, not after. Installing over a plugin the user had
        // enabled would otherwise leave the old `.enabled` marker standing over
        // it, and the next hook would run it -- a subprocess with the user's
        // privileges -- without anyone having enabled it. Enabling is the
        // user's decision to take again, about the thing they just installed.
        //
        // Order matters for the failure as much as for the success. Writing
        // first and then failing to remove the marker returns an error while
        // leaving new code enabled on disk, and leaves a window in which a hook
        // starting on another thread reads the new manifest as still permitted.
        // This way a failure leaves a plugin disabled, which is the side that
        // costs the user a click rather than a decision they never made.
        self.disable_plugin(&manifest.id)?;

        std::fs::write(plugin_dir.join("manifest.json"), manifest_json)
            .map_err(|e| format!("Failed to write manifest: {}", e))?;

        Ok(PluginInfo {
            manifest,
            enabled: false,
            path: plugin_dir.to_string_lossy().to_string(),
        })
    }

    /// Runs enabled plugin hooks serially. Each executable receives one JSON
    /// payload on stdin and must return the updated payload on stdout.
    pub fn run_hook(&self, hook_name: &str, initial: HookPayload) -> Result<HookPayload, String> {
        let mut payload = initial;
        for plugin in self.get_enabled_hooks(hook_name) {
            let Some(entrypoint) = plugin.manifest.entrypoint.as_deref() else {
                continue;
            };
            let plugin_dir = PathBuf::from(&plugin.path).canonicalize().map_err(|e| {
                format!(
                    "Plugin '{}' directory is unavailable: {}",
                    plugin.manifest.id, e
                )
            })?;
            let executable = resolve_entrypoint(&plugin_dir, entrypoint)?;
            let mut child = Command::new(&executable)
                .arg(hook_name)
                .current_dir(&plugin_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("Plugin '{}' could not start: {}", plugin.manifest.id, e))?;
            let input = serde_json::to_vec(&payload)
                .map_err(|e| format!("Plugin payload failed: {}", e))?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("Plugin '{}' stdin unavailable", plugin.manifest.id))?;
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| format!("Plugin '{}' stdout unavailable", plugin.manifest.id))?;
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| format!("Plugin '{}' stderr unavailable", plugin.manifest.id))?;

            // Drain every pipe concurrently. Writing stdin or waiting for the
            // child before draining stdout can deadlock when a plugin produces
            // more than the operating system pipe buffer.
            let input_thread = std::thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            });
            let output_thread = std::thread::spawn(move || read_bounded(&mut stdout, 1_048_576));
            let error_thread = std::thread::spawn(move || read_bounded(&mut stderr, 65_536));

            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("Plugin '{}' timed out", plugin.manifest.id));
                    }
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("Plugin '{}' failed: {}", plugin.manifest.id, e));
                    }
                }
            };
            while !(input_thread.is_finished()
                && output_thread.is_finished()
                && error_thread.is_finished())
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !input_thread.is_finished()
                || !output_thread.is_finished()
                || !error_thread.is_finished()
            {
                return Err(format!(
                    "Plugin '{}' did not close its input/output pipes",
                    plugin.manifest.id
                ));
            }
            let input_result = input_thread
                .join()
                .map_err(|_| format!("Plugin '{}' input worker failed", plugin.manifest.id))?;
            let (stdout, stdout_truncated) = output_thread
                .join()
                .map_err(|_| format!("Plugin '{}' output worker failed", plugin.manifest.id))?
                .map_err(|e| format!("Plugin '{}' output failed: {}", plugin.manifest.id, e))?;
            let (stderr, _) = error_thread
                .join()
                .map_err(|_| format!("Plugin '{}' error worker failed", plugin.manifest.id))?
                .map_err(|e| {
                    format!("Plugin '{}' error output failed: {}", plugin.manifest.id, e)
                })?;
            if !status.success() {
                let stderr = String::from_utf8_lossy(&stderr);
                let input_context = input_result
                    .err()
                    .map(|error| format!(" (input: {})", error))
                    .unwrap_or_default();
                return Err(format!(
                    "Plugin '{}' failed: {}{}",
                    plugin.manifest.id,
                    truncate(&stderr, 300),
                    input_context
                ));
            }
            if stdout_truncated {
                return Err(format!(
                    "Plugin '{}' returned too much data",
                    plugin.manifest.id
                ));
            }
            payload = serde_json::from_slice(&stdout).map_err(|e| {
                format!(
                    "Plugin '{}' returned invalid JSON: {}",
                    plugin.manifest.id, e
                )
            })?;
        }
        Ok(payload)
    }
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), String> {
    validate_plugin_id(&manifest.id)?;
    if manifest.name.trim().is_empty() || manifest.name.len() > 100 {
        return Err("Plugin name is invalid".to_string());
    }
    if manifest.version.trim().is_empty() || manifest.version.len() > 40 {
        return Err("Plugin version is invalid".to_string());
    }
    if manifest.hooks.len() > 32
        || manifest
            .hooks
            .iter()
            .any(|hook| !matches!(hook.as_str(), "after_transcribe" | "after_format"))
    {
        return Err("Plugin declares an unsupported hook".to_string());
    }
    if let Some(entrypoint) = manifest.entrypoint.as_deref() {
        validate_relative_path(entrypoint)?;
    }
    Ok(())
}

fn validate_plugin_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Plugin id may only contain letters, numbers, '-' and '_'".to_string());
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(
            "Plugin entrypoint must be a relative path inside its plugin directory".to_string(),
        );
    }
    Ok(())
}

fn resolve_entrypoint(plugin_dir: &Path, entrypoint: &str) -> Result<PathBuf, String> {
    validate_relative_path(entrypoint)?;
    let executable = plugin_dir
        .join(entrypoint)
        .canonicalize()
        .map_err(|e| format!("Plugin entrypoint is unavailable: {}", e))?;
    if !executable.starts_with(plugin_dir) || !executable.is_file() {
        return Err("Plugin entrypoint escapes its plugin directory".to_string());
    }
    Ok(executable)
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn read_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut scratch = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut scratch)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let copy = remaining.min(read);
        kept.extend_from_slice(&scratch[..copy]);
        truncated |= copy < read;
    }
    Ok((kept, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_ids_and_entrypoints() {
        assert!(validate_plugin_id("../escape").is_err());
        assert!(validate_relative_path("../run.sh").is_err());
        assert!(validate_plugin_id("clean-plugin_2").is_ok());
    }

    /// Reinstalling over a plugin the user had enabled must not hand the new
    /// code the old permission. The marker is what the hook runner reads, so
    /// leaving it in place would run a freshly installed executable at the next
    /// dictation with nobody having said yes to it.
    #[test]
    fn installing_over_an_enabled_plugin_lands_disabled() {
        let root =
            std::env::temp_dir().join(format!("openflow-plugin-test-{}", uuid::Uuid::new_v4()));
        let manager = PluginManager {
            plugins_dir: root.clone(),
        };
        let manifest_json = r#"{
            "id": "reinstalled",
            "name": "Reinstalled",
            "version": "2.0.0",
            "description": "Replaces a version the user had enabled",
            "hooks": ["after_transcribe"],
            "entrypoint": "run.sh"
        }"#;

        let first = manager
            .install_plugin(manifest_json)
            .expect("first install");
        assert!(!first.enabled);
        manager
            .enable_plugin("reinstalled")
            .expect("the user enables it");
        assert!(manager.list_plugins()[0].enabled);

        let reinstalled = manager
            .install_plugin(manifest_json)
            .expect("install over the enabled copy");

        assert!(
            !reinstalled.enabled,
            "the returned value has to be true of what is on disk"
        );
        assert!(
            !root.join("reinstalled").join(".enabled").exists(),
            "the old permission cannot survive the code it was granted to"
        );
        assert!(!manager.list_plugins()[0].enabled);
        assert!(
            manager.get_enabled_hooks("after_transcribe").is_empty(),
            "the newly written plugin must not run at the next hook"
        );

        std::fs::remove_dir_all(root).expect("remove plugin fixture");
    }

    /// Installing is two steps, and the order decides what a failure leaves
    /// behind. If the marker cannot be removed, the new executable's manifest
    /// must not already be on disk under the old permission -- otherwise the
    /// call reports failure while the thing it was refusing to enable is
    /// enabled.
    #[test]
    #[cfg(unix)]
    fn an_install_that_cannot_disable_does_not_land_the_new_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("openflow-plugin-order-{}", uuid::Uuid::new_v4()));
        let manager = PluginManager {
            plugins_dir: root.clone(),
        };
        let manifest_of = |version: &str| {
            format!(
                r#"{{"id":"pinned","name":"Pinned","version":"{}","description":"d","hooks":["after_transcribe"],"entrypoint":"run.sh"}}"#,
                version
            )
        };
        manager
            .install_plugin(&manifest_of("2.0.0"))
            .expect("first install");
        manager
            .enable_plugin("pinned")
            .expect("the user enables it");

        let plugin_dir = root.join("pinned");
        let original = std::fs::metadata(&plugin_dir).expect("stat").permissions();
        std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o555))
            .expect("lock the folder");
        // A process that ignores directory permissions -- root -- would make
        // this test assert nothing, so check the premise rather than assume it.
        let unremovable = std::fs::remove_file(plugin_dir.join(".enabled")).is_err();

        let outcome = manager.install_plugin(&manifest_of("3.0.0"));
        let on_disk = std::fs::read_to_string(plugin_dir.join("manifest.json")).expect("manifest");

        std::fs::set_permissions(&plugin_dir, original).expect("unlock the folder");
        std::fs::remove_dir_all(&root).expect("remove plugin fixture");

        if !unremovable {
            return;
        }
        assert!(outcome.is_err(), "the install could not be made safe");
        assert!(
            on_disk.contains("2.0.0"),
            "new code must not land while the old permission still stands: {}",
            on_disk
        );
    }

    #[test]
    fn bounded_reader_drains_but_caps_retained_output() {
        let mut input = std::io::Cursor::new(vec![7_u8; 32]);
        let (kept, truncated) = read_bounded(&mut input, 8).expect("bounded read");
        assert_eq!(kept, vec![7_u8; 8]);
        assert!(truncated);
    }

    #[cfg(unix)]
    #[test]
    fn hook_drains_output_larger_than_an_os_pipe_buffer() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("openflow-plugin-test-{}", uuid::Uuid::new_v4()));
        let plugin_dir = root.join("large-output");
        std::fs::create_dir_all(&plugin_dir).expect("create plugin fixture");
        let manifest = PluginManifest {
            id: "large-output".to_string(),
            name: "Large output".to_string(),
            version: "1.0.0".to_string(),
            description: "Pipe draining fixture".to_string(),
            author: None,
            hooks: vec!["after_transcribe".to_string()],
            entrypoint: Some("run.sh".to_string()),
        };
        std::fs::write(
            plugin_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        std::fs::write(plugin_dir.join(".enabled"), []).expect("enable fixture");
        let script = "#!/bin/sh\ncat >/dev/null\nprintf '{\"raw_text\":\"'\nhead -c 131072 /dev/zero | tr '\\000' x\nprintf '\",\"formatted_text\":null,\"provider\":null,\"language\":null}'\n";
        let script_path = plugin_dir.join("run.sh");
        std::fs::write(&script_path, script).expect("write fixture executable");
        let mut permissions = std::fs::metadata(&script_path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions).expect("make fixture executable");

        let manager = PluginManager {
            plugins_dir: root.clone(),
        };
        let result = manager
            .run_hook(
                "after_transcribe",
                HookPayload {
                    raw_text: Some("input".to_string()),
                    formatted_text: None,
                    provider: None,
                    language: None,
                },
            )
            .expect("large output hook completes without deadlock");
        assert_eq!(result.raw_text.expect("raw text").len(), 131_072);
        std::fs::remove_dir_all(root).expect("remove plugin fixture");
    }

    #[cfg(unix)]
    #[test]
    fn successful_hook_may_exit_without_consuming_all_input() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("openflow-plugin-test-{}", uuid::Uuid::new_v4()));
        let plugin_dir = root.join("no-input");
        std::fs::create_dir_all(&plugin_dir).expect("create plugin fixture");
        let manifest = PluginManifest {
            id: "no-input".to_string(),
            name: "No input".to_string(),
            version: "1.0.0".to_string(),
            description: "Early exit fixture".to_string(),
            author: None,
            hooks: vec!["after_transcribe".to_string()],
            entrypoint: Some("run.sh".to_string()),
        };
        std::fs::write(
            plugin_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        std::fs::write(plugin_dir.join(".enabled"), []).expect("enable fixture");
        let script = "#!/bin/sh\nprintf '{\"raw_text\":\"ok\",\"formatted_text\":null,\"provider\":null,\"language\":null}'\n";
        let script_path = plugin_dir.join("run.sh");
        std::fs::write(&script_path, script).expect("write fixture executable");
        let mut permissions = std::fs::metadata(&script_path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script_path, permissions).expect("make fixture executable");

        let manager = PluginManager {
            plugins_dir: root.clone(),
        };
        let result = manager
            .run_hook(
                "after_transcribe",
                HookPayload {
                    raw_text: Some("x".repeat(256 * 1024)),
                    formatted_text: None,
                    provider: None,
                    language: None,
                },
            )
            .expect("successful hook is not failed by a closed stdin pipe");
        assert_eq!(result.raw_text.as_deref(), Some("ok"));
        std::fs::remove_dir_all(root).expect("remove plugin fixture");
    }
}
