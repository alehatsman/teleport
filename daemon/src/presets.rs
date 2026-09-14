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
    /// Stable identifier, referenced by `POST /api/v1/sessions`'s `preset`
    /// field.
    pub id: String,
    /// Display label for the UI.
    pub label: String,
    /// May be the literal `"$SHELL"` (Unix) placeholder -- see
    /// [`Preset::resolved_command`]. Stored as written; expansion happens at
    /// use time, not load time, so a login shell change takes effect on the
    /// next session without editing `presets.toml`.
    pub command: String,
    /// `command`'s argv, not including `command` itself.
    #[serde(default)]
    pub args: Vec<String>,
    /// Icon name for the UI; not interpreted here.
    pub icon: String,
    /// How this agent is told to resume a previous conversation, e.g.
    /// `["--resume"]`. Empty means "this agent has no resume story", and the
    /// UI offers no Resume action for it -- which is the honest default for
    /// an arbitrary command.
    ///
    /// This is the seam that keeps harness knowledge out of the daemon and
    /// out of the wire: teleport does not know that `claude` resumes and a
    /// plain shell does not, it reads that from `presets.toml`
    /// (docs/11-mvp-plan.md#m8--agent-presets anticipated exactly this
    /// field). A session id, when one is known, is appended to these args by
    /// the caller; teleport never parses or validates it -- it is the
    /// agent's own opaque identifier.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Run `command` through the user's login shell instead of exec'ing it
    /// directly, so it inherits the environment their dotfiles build
    /// (docs/03-pty-layer.md#spawn, docs/04-api-protocol.md#get-apiv1presets).
    /// `#[serde(default)]` -- a `presets.toml` written before this field
    /// existed keeps the old direct-exec behavior rather than silently
    /// changing how every already-installed daemon spawns its agents.
    #[serde(default)]
    pub login_shell: bool,
}

impl Preset {
    /// Expands the `"$SHELL"` placeholder against the daemon's own
    /// environment. Anything else is returned as-is -- a preset author who
    /// wants a literal `$SHELL` string as their command has no way to escape
    /// this, but no built-in or realistic custom preset needs one.
    pub fn resolved_command(&self) -> String {
        if self.command == "$SHELL" {
            crate::pty::login_shell_path()
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
        // The two agent presets exec a named binary with no shell in
        // between, so they -- and only they -- need `login_shell` to see the
        // user's real PATH/EDITOR/LANG (docs/03-pty-layer.md#spawn).
        Preset {
            id: "codex".to_string(),
            label: "Codex".to_string(),
            command: "codex".to_string(),
            args: vec![],
            icon: "codex".to_string(),
            login_shell: true,
            // Deliberately empty rather than guessed: codex's resume flag
            // has not been verified against a real binary here, and a wrong
            // flag turns a Resume button into a failed spawn. One line in
            // `presets.toml` adds it once someone checks.
            resume_args: vec![],
        },
        Preset {
            id: "claude".to_string(),
            label: "Claude Code".to_string(),
            command: "claude".to_string(),
            args: vec![],
            icon: "claude".to_string(),
            login_shell: true,
            // Bare `--resume` opens Claude Code's own picker for the folder.
            // Deliberately not `--continue`, which resumes the most recent
            // conversation in the directory without asking -- restoring four
            // agents that worked in one repo would point all four at one
            // conversation.
            resume_args: vec!["--resume".to_string()],
        },
        // Already `$SHELL -l`: wrapping a login shell in a login shell buys
        // nothing but a second startup.
        Preset {
            id: "shell".to_string(),
            label: "Shell".to_string(),
            command: "$SHELL".to_string(),
            args: vec!["-l".to_string()],
            icon: "terminal".to_string(),
            login_shell: false,
            resume_args: vec![],
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
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_file_is_a_clean_error_not_a_silent_overwrite() {
        let dir = scratch_dir("malformed");
        fs::write(dir.join("presets.toml"), "not valid toml {{{").unwrap();
        load_or_create(&dir).unwrap_err();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
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
        load_or_create(&dir).unwrap_err();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `presets.toml` written before `login_shell` existed must keep the
    /// old direct-exec behavior rather than silently changing how an
    /// already-installed daemon spawns its agents -- the field is
    /// `#[serde(default)]` for exactly this
    /// (docs/04-api-protocol.md#get-apiv1presets).
    #[test]
    fn an_older_presets_file_without_login_shell_defaults_to_off() {
        let dir = scratch_dir("no-login-shell-field");
        fs::write(
            dir.join("presets.toml"),
            r#"
            [[presets]]
            id = "claude"
            label = "Claude Code"
            command = "claude"
            icon = "claude"
            "#,
        )
        .unwrap();
        let presets = load_or_create(&dir).expect("load");
        assert!(!presets[0].login_shell);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    /// The agent presets exec a named binary with no shell in between, so
    /// they are the ones that need the login shell; `shell` is already
    /// `$SHELL -l` and wrapping it would buy nothing but a second startup.
    #[test]
    fn only_the_agent_defaults_ask_for_a_login_shell() {
        let by_id = |id: &str| -> bool {
            default_presets()
                .into_iter()
                .find(|p| p.id == id)
                .expect("built-in preset")
                .login_shell
        };
        assert!(by_id("claude"));
        assert!(by_id("codex"));
        assert!(!by_id("shell"));
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
        load_or_create(&dir).unwrap_err();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
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
            login_shell: false,
            resume_args: vec![],
        };
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(preset.resolved_command(), "/bin/zsh");
    }
}
