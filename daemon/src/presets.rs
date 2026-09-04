//! Agent presets -- `<data_dir>/presets.toml` and `GET /api/v1/presets`
//! (docs/04-api-protocol.md#get-apiv1presets, docs/11-mvp-plan.md#m8--agent-presets).
//! A preset supplies executable, argv defaults and presentation metadata; no
//! scheduler, agent protocol or provider SDK is needed to spawn the first
//! Codex/Claude CLI.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One entry from `presets.toml`, and the shape `GET /api/v1/presets`
/// returns verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub label: String,
    /// May be the literal `"$SHELL"` (Unix) placeholder -- see
    /// [`Preset::resolved_command`]. Stored as written; expansion happens at
    /// use time, not load time, so a login shell change takes effect on the
    /// next session without editing `presets.toml`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub icon: String,
}

impl Preset {
    /// Expands the `"$SHELL"` placeholder against the daemon's own
    /// environment. Anything else is returned as-is -- a preset author who
    /// wants a literal `$SHELL` string as their command has no way to escape
    /// this, but no built-in or realistic custom preset needs one.
    pub fn resolved_command(&self) -> String {
        if self.command == "$SHELL" {
            #[cfg(unix)]
            {
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
            }
            #[cfg(windows)]
            {
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
            }
        } else {
            self.command.clone()
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PresetsFile {
    #[serde(default)]
    presets: Vec<Preset>,
}

fn default_presets() -> Vec<Preset> {
    vec![
        Preset {
            id: "codex".to_string(),
            label: "Codex".to_string(),
            command: "codex".to_string(),
            args: vec![],
            icon: "codex".to_string(),
        },
        Preset {
            id: "claude".to_string(),
            label: "Claude Code".to_string(),
            command: "claude".to_string(),
            args: vec![],
            icon: "claude".to_string(),
        },
        Preset {
            id: "shell".to_string(),
            label: "Shell".to_string(),
            command: "$SHELL".to_string(),
            args: vec!["-l".to_string()],
            icon: "terminal".to_string(),
        },
    ]
}

/// Loads `<data_dir>/presets.toml`, writing the built-in defaults there on
/// first run -- the same first-run-generates-a-file shape as `device.json`
/// and `token` (docs/05-persistence.md#layout). A malformed existing file is
/// a startup error, not silently replaced: overwriting a user's edits on a
/// typo would be worse than refusing to start.
pub fn load_or_create(data_dir: &Path) -> Result<Vec<Preset>> {
    let path = data_dir.join("presets.toml");

    match fs::read_to_string(&path) {
        Ok(contents) => {
            let file: PresetsFile =
                toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
            validate_presets(&file.presets)
                .with_context(|| format!("validating {}", path.display()))?;
            Ok(file.presets)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let presets = default_presets();
            let file = PresetsFile {
                presets: presets.clone(),
            };
            let serialized =
                toml::to_string_pretty(&file).context("serializing default presets")?;
            fs::write(&path, serialized).with_context(|| format!("writing {}", path.display()))?;
            Ok(presets)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Catches a hand-edited `presets.toml` that parses as valid TOML but makes
/// no sense as presets -- a duplicate id (silently shadowed by whichever
/// entry `Vec::iter().find` reaches first at session-create time) or an
/// empty id/command (M4 review: previously only TOML syntax was checked, so
/// these surfaced later as a confusing `422` instead of a clear startup
/// error).
fn validate_presets(presets: &[Preset]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for p in presets {
        anyhow::ensure!(!p.id.is_empty(), "a preset has an empty id");
        anyhow::ensure!(
            !p.command.is_empty(),
            "preset {:?} has an empty command",
            p.id
        );
        anyhow::ensure!(
            seen.insert(p.id.as_str()),
            "duplicate preset id: {:?}",
            p.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-presets-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_run_writes_and_returns_the_built_in_defaults() {
        let dir = scratch_dir("first-run");
        let presets = load_or_create(&dir).expect("load_or_create");
        assert_eq!(presets.len(), 3);
        assert!(dir.join("presets.toml").is_file());

        // Loading again must return the same set from the file, not
        // regenerate -- proves the write round-trips through TOML cleanly.
        let reloaded = load_or_create(&dir).expect("reload");
        assert_eq!(reloaded.len(), 3);
        assert_eq!(reloaded[0].id, presets[0].id);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_file_is_a_clean_error_not_a_silent_overwrite() {
        let dir = scratch_dir("malformed");
        fs::write(dir.join("presets.toml"), "not valid toml {{{").unwrap();
        assert!(load_or_create(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// M4 review: valid-TOML-but-nonsense presets (a duplicate id) used to
    /// load silently, and `create_session`'s `find`-by-id would pick
    /// whichever entry came first with no warning.
    #[test]
    fn a_duplicate_preset_id_is_a_clean_error() {
        let dir = scratch_dir("duplicate-id");
        fs::write(
            dir.join("presets.toml"),
            r#"
            [[presets]]
            id = "shell"
            label = "Shell"
            command = "/bin/sh"
            icon = "terminal"

            [[presets]]
            id = "shell"
            label = "Shell Again"
            command = "/bin/bash"
            icon = "terminal"
            "#,
        )
        .unwrap();
        assert!(load_or_create(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// M4 review: same gap, for an empty command.
    #[test]
    fn an_empty_preset_command_is_a_clean_error() {
        let dir = scratch_dir("empty-command");
        fs::write(
            dir.join("presets.toml"),
            r#"
            [[presets]]
            id = "broken"
            label = "Broken"
            command = ""
            icon = "terminal"
            "#,
        )
        .unwrap();
        assert!(load_or_create(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    // Unix-only: `$SHELL` expansion is a Unix shell convention, and the
    // Windows leg of this test would build a `Preset` it never uses --
    // caught as an `unused_variables` clippy error (`-D warnings`) once
    // `cargo test` stopped failing earlier in the Windows CI job and let
    // `cargo clippy --all-targets` actually run, 2026-09-04.
    #[cfg(unix)]
    #[test]
    fn shell_preset_expands_the_env_var() {
        let preset = Preset {
            id: "shell".into(),
            label: "Shell".into(),
            command: "$SHELL".into(),
            args: vec![],
            icon: "terminal".into(),
        };
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(preset.resolved_command(), "/bin/zsh");
    }
}
