//! Where the SPA comes from, decided *per request*
//! (docs/18-ui-upgrades.md#the-slot).
//!
//! Two candidate directories, in order: an explicit `--web-dist` (the dev
//! workflow) and the version slot `<data_dir>/web/current` that
//! `teleport ui upgrade` flips. Neither is checked at startup: a daemon
//! that booted before the first-ever `teleport ui upgrade` must pick the
//! slot up without a restart, which is the entire point of the slot
//! (docs/18-ui-upgrades.md#resolution-is-per-request-not-per-process).
//!
//! The cost is one `stat` per non-API request on a loopback server, on a
//! path that is about to `open` the file anyway.

use std::path::{Path, PathBuf};

/// The symlink inside the slot root that names the live version. Flipped by
/// `rename(2)` over itself, so an open that follows it either sees the old
/// target or the new one, never a partial state.
pub const CURRENT_LINK: &str = "current";

/// Where a served file came from. `resolve` returns the directory; the
/// variant is what `/health`'s `ui_version` reports on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// An explicit `--web-dist`. A dev tree has no release version and must
    /// not pretend to have one.
    Dist(PathBuf),
    /// `<data_dir>/web/current`, held as the *symlink* path rather than its
    /// target so every open re-follows it.
    Slot(PathBuf),
}

/// The two candidate roots, resolved on each call. Cheap to clone-free
/// share behind the `AppState` `Arc`.
#[derive(Debug, Default)]
pub struct WebAssets {
    explicit: Option<PathBuf>,
    slot_root: Option<PathBuf>,
}

impl WebAssets {
    /// `explicit` is `--web-dist`; `slot_root` is `<data_dir>/web`, which
    /// need not exist yet -- it is created by the first `teleport ui
    /// upgrade`, possibly long after this daemon started.
    pub fn new(explicit: Option<PathBuf>, slot_root: Option<PathBuf>) -> Self {
        Self {
            explicit,
            slot_root,
        }
    }

    /// The directory to serve from, or `None` to fall through to the
    /// embedded bundle (or a `404` in a build without one).
    pub fn resolve(&self) -> Option<PathBuf> {
        match self.source()? {
            Source::Dist(path) | Source::Slot(path) => Some(path),
        }
    }

    /// The release tag the slot currently points at, for `/health`.
    /// `None` when serving an explicit `--web-dist` or the embedded bundle,
    /// and `None` when `current` is a plain directory rather than a symlink
    /// -- there is no version to report in any of those cases, and guessing
    /// one would be worse than admitting it.
    pub fn ui_version(&self) -> Option<String> {
        let Source::Slot(link) = self.source()? else {
            return None;
        };
        let target = std::fs::read_link(link).ok()?;
        Some(target.file_name()?.to_string_lossy().into_owned())
    }

    /// Version directories kept beside the live one, newest name first.
    ///
    /// Only the miss path for `/assets/*` reads these: a tab that loaded the
    /// previous UI still asks for its content-hashed chunks, which exist
    /// only in the directory the flip moved off of
    /// (docs/18-ui-upgrades.md#stale-tabs-and-why-old-versions-are-retained).
    /// Empty unless the slot is what's being served -- a `--web-dist` tree
    /// has no siblings to fall through to.
    pub fn retained_versions(&self) -> Vec<PathBuf> {
        let Some(Source::Slot(link)) = self.source() else {
            return Vec::new();
        };
        let Some(root) = link.parent() else {
            return Vec::new();
        };
        let live = std::fs::read_link(&link)
            .ok()
            .and_then(|t| t.file_name().map(std::ffi::OsString::from));

        let mut dirs: Vec<PathBuf> = read_version_dirs(root)
            .into_iter()
            .filter(|p| p.file_name().map(std::ffi::OsString::from) != live)
            .collect();
        // Newest name first: a hashed asset present in two retained versions
        // is the same bytes either way, but a deterministic order keeps the
        // miss path's behavior reproducible in tests.
        dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        dirs
    }

    fn source(&self) -> Option<Source> {
        if let Some(dist) = &self.explicit {
            if dist.is_dir() {
                return Some(Source::Dist(dist.clone()));
            }
        }
        let link = self.slot_root.as_ref()?.join(CURRENT_LINK);
        // `is_dir` follows the symlink, which is what matters: a `current`
        // dangling at a pruned version is not servable.
        link.is_dir().then_some(Source::Slot(link))
    }
}

/// Version directories directly under `root`: real directories, skipping
/// the `current` symlink itself and anything dot-prefixed (a `.staging-*`
/// an interrupted upgrade left behind must never be served).
pub fn read_version_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name != CURRENT_LINK && !name.starts_with('.')
        })
        // `file_type` does not follow symlinks; a stray symlink beside
        // `current` is not a version directory.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-web-assets-{}-{name}",
            std::process::id()
        ));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "clearing a stale dir from a previous run; fine if absent"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Every caller is a `cfg(unix)` test (they all need `point_current_at`
    /// below), so on Windows this is dead code and `-D warnings` says so.
    /// Same gate, same reason as `point_current_at`: "needs symlinks", not a
    /// platform being dropped.
    #[cfg(unix)]
    fn version(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create version dir");
        dir
    }

    /// Only the `current` *symlink* is unix-only here; nothing in
    /// `WebAssets` itself is. The tests that need one are gated with it --
    /// `cfg(unix)`, not `target_os`, so this is "needs symlinks", not a
    /// platform being dropped (scripts/check-target-os-gates.sh's header).
    #[cfg(unix)]
    fn point_current_at(root: &Path, name: &str) {
        let staged = root.join(".staged-link");
        std::os::unix::fs::symlink(name, &staged).expect("symlink");
        std::fs::rename(&staged, root.join(CURRENT_LINK)).expect("rename over current");
    }

    #[test]
    #[cfg(unix)]
    fn an_explicit_dist_wins_over_the_slot() {
        let root = scratch("explicit-wins");
        let dist = version(&root, "dist");
        version(&root, "v1.0.0");
        point_current_at(&root, "v1.0.0");

        let assets = WebAssets::new(Some(dist.clone()), Some(root.clone()));
        assert_eq!(assets.resolve(), Some(dist));
        // A dev tree has no release version to report.
        assert_eq!(assets.ui_version(), None);
    }

    /// A `--web-dist` pointed at a directory that isn't there is a typo or a
    /// tree that hasn't been built yet, not an instruction to serve nothing.
    #[test]
    #[cfg(unix)]
    fn a_dist_that_does_not_resolve_falls_through_to_the_slot() {
        let root = scratch("dist-missing");
        version(&root, "v1.0.0");
        point_current_at(&root, "v1.0.0");

        let assets = WebAssets::new(Some(root.join("not-built")), Some(root.clone()));
        assert_eq!(assets.resolve(), Some(root.join(CURRENT_LINK)));
        assert_eq!(assets.ui_version(), Some("v1.0.0".to_string()));
    }

    /// `resolve` hands back the *symlink*, not its target, so every `open`
    /// re-follows it and a flip lands on the very next request.
    #[test]
    #[cfg(unix)]
    fn the_slot_resolves_to_the_symlink_not_its_target() {
        let root = scratch("symlink-not-target");
        version(&root, "v1.0.0");
        point_current_at(&root, "v1.0.0");

        let assets = WebAssets::new(None, Some(root.clone()));
        assert_eq!(assets.resolve(), Some(root.join(CURRENT_LINK)));
    }

    #[test]
    fn nothing_resolves_when_the_slot_is_empty() {
        let root = scratch("empty-slot");
        let assets = WebAssets::new(None, Some(root));
        assert_eq!(assets.resolve(), None);
        assert_eq!(assets.ui_version(), None);
        assert!(assets.retained_versions().is_empty());
    }

    /// A `current` left dangling by a prune is not servable, and saying so
    /// falls back to the embedded bundle instead of serving 404s forever.
    #[test]
    #[cfg(unix)]
    fn a_dangling_current_resolves_to_nothing() {
        let root = scratch("dangling");
        version(&root, "v1.0.0");
        point_current_at(&root, "v1.0.0");
        std::fs::remove_dir_all(root.join("v1.0.0")).expect("prune the live version");

        let assets = WebAssets::new(None, Some(root));
        assert_eq!(assets.resolve(), None);
    }

    #[test]
    #[cfg(unix)]
    fn retained_versions_exclude_the_live_one_and_anything_staged() {
        let root = scratch("retained");
        version(&root, "v1.0.0");
        version(&root, "v1.1.0");
        version(&root, ".staging-v1.2.0");
        point_current_at(&root, "v1.1.0");

        let assets = WebAssets::new(None, Some(root.clone()));
        assert_eq!(assets.retained_versions(), vec![root.join("v1.0.0")]);
    }

    #[test]
    #[cfg(unix)]
    fn retained_versions_are_newest_name_first() {
        let root = scratch("retained-order");
        version(&root, "v1.0.0");
        version(&root, "v1.0.1");
        version(&root, "v1.1.0");
        point_current_at(&root, "v1.1.0");

        let assets = WebAssets::new(None, Some(root.clone()));
        assert_eq!(
            assets.retained_versions(),
            vec![root.join("v1.0.1"), root.join("v1.0.0")]
        );
    }

    /// A dev tree has no sibling versions to fall through to, and must not
    /// go looking in whatever directory happens to contain it.
    #[test]
    #[cfg(unix)]
    fn a_dist_tree_has_no_retained_versions() {
        let root = scratch("dist-no-retained");
        let dist = version(&root, "dist");
        version(&root, "v1.0.0");
        point_current_at(&root, "v1.0.0");

        let assets = WebAssets::new(Some(dist), Some(root));
        assert!(assets.retained_versions().is_empty());
    }
}
