//! `teleport ui` -- the UI half of upgrades, which must not restart the
//! daemon (docs/18-ui-upgrades.md).
//!
//! Everything here is filesystem work on `<data_dir>/web`: download, verify,
//! extract, and one `rename(2)` that flips the `current` symlink. The daemon
//! re-resolves that symlink on every request
//! (`daemon/src/web_assets.rs`), so the flip *is* the upgrade -- nothing is
//! signalled and no PTY is touched.
//!
//! The CLI does this rather than the daemon deliberately
//! (docs/18-ui-upgrades.md#why-the-cli-not-the-daemon): `teleportd` is a
//! long-lived process holding every PTY on the machine, and an outbound
//! update path -- HTTP client, TLS trust, archive extraction, checksum
//! policy -- does not belong inside it. This process is short-lived and
//! holds nothing.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Where releases come from -- the same repo `scripts/install.sh` installs
/// the binaries from.
const REPO: &str = "alehatsman/teleport";

/// How many version directories survive a flip, beyond the live one. What
/// it buys: a tab open across two upgrades still loads its hashed chunks.
/// What it costs: ~1.2 MB (docs/18-ui-upgrades.md#stale-tabs-and-why-old-versions-are-retained).
const RETAINED_VERSIONS: usize = 3;

/// Prefix for the directory an upgrade extracts into before renaming it into
/// place. Dot-prefixed so a half-extracted bundle is invisible to everything
/// that lists versions -- the daemon's and the CLI's listings both skip it.
const STAGING_PREFIX: &str = ".staging-";

/// `<data_dir>/web` -- the slot root. Every path this module touches is
/// under it.
#[derive(Debug)]
pub(crate) struct Slot {
    root: PathBuf,
}

impl Slot {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("web"),
        }
    }

    fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// The tag `current` names, whether or not that directory still exists
    /// -- a dangling link is exactly the state `ui status` has to be able
    /// to report.
    fn current_version(&self) -> Option<String> {
        let target = std::fs::read_link(self.current_link()).ok()?;
        Some(target.file_name()?.to_string_lossy().into_owned())
    }

    /// Installed version directories, most recently installed first.
    ///
    /// Ordered by mtime, not by name: version *names* don't sort
    /// (`v1.10.0` < `v1.9.0` lexically), and what retention and rollback
    /// actually mean is "the one before this one", which is install order.
    fn versions(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                // `current` is the symlink itself; dot-prefixed is a
                // `.staging-*` an interrupted upgrade left behind.
                name != "current" && !name.starts_with('.')
            })
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, e.path()))
            })
            .collect();
        dirs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        dirs.into_iter().map(|(_, path)| path).collect()
    }

    /// `.staging-*` directories an interrupted upgrade left behind
    /// ([#78](https://github.com/alehatsman/teleport/issues/78)). Harmless --
    /// dot-prefixed, so neither [`Slot::versions`] nor the daemon's
    /// `read_version_dirs` ever sees them, meaning they are never served and
    /// never counted against retention -- but ~1.2 MB of litter each, and
    /// nothing used to name them.
    fn stray_staging(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
        // Name order, not mtime: these are only ever listed or deleted, and a
        // stable order keeps `status` output diffable between runs.
        dirs.sort();
        dirs
    }

    /// The upgrade's one irreversible step, and a single syscall: symlink to
    /// a temp name, then `rename(2)` over `current`. Atomic on POSIX, so no
    /// in-flight request ever observes a missing or half-written link.
    fn flip_to(&self, version: &str) -> Result<()> {
        let staged = self.root.join(".current.staged");
        // A leftover from an interrupted flip; symlink() fails on an
        // existing path, so clear it first.
        remove_if_present(&staged)?;
        std::os::unix::fs::symlink(version, &staged)
            .with_context(|| format!("creating the staged symlink at {}", staged.display()))?;
        std::fs::rename(&staged, self.current_link())
            .with_context(|| format!("pointing {} at {version}", self.current_link().display()))
    }
}

/// `teleport ui status` -- "what is it actually serving?" with an answer
/// that isn't `ls -l` on a symlink.
pub(crate) fn status(data_dir: &Path) {
    let slot = Slot::new(data_dir);
    println!("slot: {}", slot.root.display());
    match slot.current_version() {
        Some(version) if slot.root.join(&version).is_dir() => println!("current: {version}"),
        // Reported, not silently ignored: the daemon falls back to its
        // embedded bundle here, so the UI still works and nothing else
        // would tell you the slot is broken.
        Some(version) => println!("current: {version} (DANGLING -- serving the embedded UI)"),
        None => println!("current: none (serving the embedded UI, or --web-dist)"),
    }
    let versions = slot.versions();
    if versions.is_empty() {
        println!("installed: none");
    } else {
        println!("installed (newest first):");
        for path in versions {
            println!(
                "  {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    // Only mentioned when there are any -- a permanent "stray: none" line
    // would be noise on every healthy run. #78 exists because "why is there
    // a .staging-v0.4.1 in my data dir" had no answer anywhere; this is it.
    let stray = slot.stray_staging();
    if !stray.is_empty() {
        println!("stray staging dirs (an interrupted upgrade; safe to delete):");
        for path in stray {
            println!(
                "  {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        println!("  -- the next `teleport ui upgrade` clears these");
    }
}

/// `teleport ui upgrade` -- download, verify, extract, flip, prune.
///
/// Steps 1-5 are all recoverable: any failure leaves `current` pointing
/// where it did and at worst a `.staging-*` directory to delete.
pub(crate) async fn upgrade(data_dir: &Path, version: Option<String>) -> Result<()> {
    let slot = Slot::new(data_dir);
    std::fs::create_dir_all(&slot.root)
        .with_context(|| format!("creating the slot root at {}", slot.root.display()))?;
    restrict(&slot.root)?;

    // Every stray, not just this tag's (#78). An upgrade is the only thing
    // that creates these, so it is the natural thing to clear them -- and a
    // retry after the crash that stranded one is exactly when a user is here.
    // Two concurrent upgrades would race, but that was never supported: one
    // owner, one daemon, one slot.
    for stray in slot.stray_staging() {
        println!("clearing stray {}", stray.display());
        remove_if_present(&stray)?;
    }

    let http = client()?;
    let tag = match version {
        Some(tag) => tag,
        None => latest_tag(&http).await?,
    };
    let installed = slot.root.join(&tag);

    if installed.is_dir() {
        // Already on disk: flip to it and stop. Re-extracting over a
        // directory the daemon may be serving right now would mean deleting
        // it first, which is the one thing this design never does.
        println!("{tag} is already installed");
    } else {
        let archive = format!("teleport-web-{tag}.tar.gz");
        let base = format!("https://github.com/{REPO}/releases/download/{tag}");

        println!("downloading {archive}...");
        let bytes = get_bytes(&http, &format!("{base}/{archive}"))
            .await
            .with_context(|| {
                format!("downloading {archive} -- is there a release for {tag} with a web bundle?")
            })?;
        let checksums = get_bytes(&http, &format!("{base}/checksums.txt"))
            .await
            .with_context(|| format!("downloading checksums.txt for {tag}"))?;

        // Verified before a single byte is written: the extracted tree ends
        // up in a directory the daemon serves to a browser.
        verify(&bytes, &String::from_utf8_lossy(&checksums), &archive)?;

        let staging = slot.root.join(format!("{STAGING_PREFIX}{tag}"));
        remove_if_present(&staging)?;
        extract(&bytes, &staging)?;
        let index = staging.join("index.html");
        if std::fs::metadata(&index).map_or(0, |m| m.len()) == 0 {
            remove_if_present(&staging)?;
            bail!("{archive} has no usable index.html -- refusing to install it");
        }
        std::fs::rename(&staging, &installed)
            .with_context(|| format!("moving the staged bundle into {}", installed.display()))?;
    }

    slot.flip_to(&tag)?;
    println!("UI is now {tag} -- no daemon restart, no session touched");
    prune(&slot, &tag)?;
    Ok(())
}

/// `teleport ui rollback` -- one `rename(2)`, no download. This is why a
/// flip retains the previous directory instead of replacing it in place.
pub(crate) fn rollback(data_dir: &Path) -> Result<()> {
    let slot = Slot::new(data_dir);
    let current = slot.current_version();
    let names: Vec<String> = slot
        .versions()
        .into_iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    // The one *behind* the current one in install order -- not merely
    // "some other version". Without that, a second rollback would walk
    // forward again and the two commands would just toggle.
    let previous = match current
        .as_ref()
        .and_then(|c| names.iter().position(|n| n == c))
    {
        Some(index) => names.get(index + 1),
        // `current` is missing or dangling: the newest installed version is
        // the best available answer.
        None => names.first(),
    }
    .cloned();
    let Some(previous) = previous else {
        // Idempotent at the oldest retained version rather than an error:
        // rolling back when there is nowhere to roll back to has already
        // achieved what it was asked for.
        println!("no earlier UI version is retained; staying on the current one");
        return Ok(());
    };
    slot.flip_to(&previous)?;
    println!("UI rolled back to {previous}");
    Ok(())
}

/// Keeps the live version plus [`RETAINED_VERSIONS`] behind it. Pruning
/// happens after the flip, so a failure here has already left the upgrade
/// itself complete.
fn prune(slot: &Slot, live: &str) -> Result<()> {
    let stale: Vec<PathBuf> = slot
        .versions()
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n != live))
        .skip(RETAINED_VERSIONS)
        .collect();
    for dir in stale {
        println!("pruning {}", dir.display());
        remove_if_present(&dir)?;
    }
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // GitHub's API rejects requests without one.
        .user_agent(concat!("teleport-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")
}

async fn latest_tag(http: &reqwest::Client) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body: serde_json::Value = serde_json::from_slice(&get_bytes(http, &url).await?)
        .context("parsing the GitHub releases response")?;
    body.get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("could not resolve the latest release tag -- pass --version vX.Y.Z")
}

async fn get_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} returned {status}");
    }
    Ok(response
        .bytes()
        .await
        .with_context(|| format!("reading the body of {url}"))?
        .to_vec())
}

/// sha256 against the release's own `checksums.txt`, in the `sha256sum`
/// format the release workflow writes: `<hex>  <filename>`.
pub(crate) fn verify(bytes: &[u8], checksums: &str, archive: &str) -> Result<()> {
    use sha2::Digest as _;

    let expected = checksums
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| name.trim() == archive)
        .map(|(hash, _)| hash.trim())
        .with_context(|| format!("{archive} is not listed in checksums.txt"))?;
    let actual = hex(&sha2::Sha256::digest(bytes));
    if actual != expected {
        bail!("checksum mismatch for {archive} (expected {expected}, got {actual})");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        // Writing to a String is infallible, but the Result is #[must_use].
        #[expect(
            clippy::let_underscore_must_use,
            reason = "fmt::Write on a String cannot fail"
        )]
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Extracts the bundle into `dest`, treating the archive as untrusted input
/// even after the checksum matches
/// (docs/18-ui-upgrades.md#extraction-safety).
///
/// Only regular files are accepted, and only at paths that stay inside
/// `dest`. Nothing in a Vite build is a symlink, a hardlink, a device or a
/// `..` path, so rejecting all of them costs nothing and removes the whole
/// class of "the tarball wrote outside the directory" bugs. The archive's
/// single top-level directory (the release tag) is stripped.
pub(crate) fn extract(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    restrict(dest)?;

    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut files = 0_usize;

    for entry in archive.entries().context("reading the archive")? {
        let mut entry = entry.context("reading an archive entry")?;
        let path = entry.path().context("reading an entry path")?.into_owned();
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            bail!(
                "{} is not a regular file ({entry_type:?}) -- refusing to extract it",
                path.display()
            );
        }
        let relative = strip_top_level(&path)
            .with_context(|| format!("{} has no path below the archive root", path.display()))?;
        let target = safe_join(dest, &relative)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            restrict(parent)?;
        }
        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .with_context(|| format!("reading {}", path.display()))?;
        std::fs::write(&target, &contents)
            .with_context(|| format!("writing {}", target.display()))?;
        // Owner-only, same as the rest of <data_dir> (docs/06-security.md).
        // The archive's own modes are ignored on purpose -- they come from
        // whatever machine built the release.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", target.display()))?;
        files += 1;
    }

    if files == 0 {
        bail!("the archive contained no files");
    }
    Ok(())
}

/// Drops the archive's single top-level directory (the release tag), so
/// `v1.2.3/assets/app.js` lands at `assets/app.js`.
fn strip_top_level(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    components.next()?;
    let rest = components.as_path();
    (!rest.as_os_str().is_empty()).then(|| rest.to_path_buf())
}

/// Joins `relative` under `dest`, rejecting anything that could escape:
/// `..`, an absolute path, or a Windows prefix.
fn safe_join(dest: &Path, relative: &Path) -> Result<PathBuf> {
    use std::path::Component;

    let mut out = dest.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "{} escapes the staging directory -- refusing to extract it",
                    relative.display()
                );
            }
        }
    }
    Ok(out)
}

fn restrict(dir: &Path) -> Result<()> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

fn remove_if_present(path: &Path) -> Result<()> {
    // `symlink_metadata`, not `metadata`: a dangling symlink must still be
    // removed, and following it would report it as absent.
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}

/// `teleport ui *` writes to the *local* filesystem, so it is a local-daemon
/// command even though `teleport` is otherwise remote-capable. Upgrading a
/// remote daemon's UI means running this CLI on that host; silently
/// upgrading the wrong machine's slot would be worse than refusing.
pub(crate) fn reject_remote(url: Option<&str>) -> Result<()> {
    let Some(url) = url else {
        return Ok(());
    };
    if is_loopback(url) {
        return Ok(());
    }
    bail!(
        "`teleport ui` writes to this machine's data directory, but --url points at {url} -- \
         run it on that host instead"
    )
}

fn is_loopback(url: &str) -> bool {
    let host = url
        .split("//")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("teleport-ui-test-{name}-{}", std::process::id()));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "clearing a stale dir from a previous run; fine if absent"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #78: an interrupted upgrade strands a `.staging-*` directory. It must
    /// stay invisible to everything that lists versions -- otherwise a
    /// half-extracted bundle could be flipped to or counted against retention
    /// -- while still being findable by the one thing that cleans it up.
    #[cfg(unix)]
    #[test]
    fn stray_staging_dirs_are_listed_but_never_counted_as_versions() {
        let dir = scratch("stray-staging");
        let slot = Slot::new(&dir);
        std::fs::create_dir_all(&slot.root).unwrap();
        std::fs::create_dir_all(slot.root.join("v1.0.0")).unwrap();
        std::fs::create_dir_all(slot.root.join(format!("{STAGING_PREFIX}v1.1.0"))).unwrap();
        std::fs::create_dir_all(slot.root.join(format!("{STAGING_PREFIX}v1.2.0"))).unwrap();

        let versions: Vec<String> = slot
            .versions()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            versions,
            vec!["v1.0.0"],
            "staging dirs leaked into versions"
        );

        let stray: Vec<String> = slot
            .stray_staging()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            stray,
            vec![
                format!("{STAGING_PREFIX}v1.1.0"),
                format!("{STAGING_PREFIX}v1.2.0")
            ],
            "every stray should be found, in a stable order"
        );

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `current` symlink and a plain file must not be mistaken for
    /// strays: deleting either would break the slot outright.
    #[cfg(unix)]
    #[test]
    fn the_current_link_and_stray_files_are_not_stray_staging_dirs() {
        let dir = scratch("stray-staging-negatives");
        let slot = Slot::new(&dir);
        std::fs::create_dir_all(&slot.root).unwrap();
        std::fs::create_dir_all(slot.root.join("v1.0.0")).unwrap();
        std::os::unix::fs::symlink("v1.0.0", slot.root.join("current")).unwrap();
        // A file, not a directory, that happens to share the prefix.
        std::fs::write(slot.root.join(format!("{STAGING_PREFIX}notadir")), b"x").unwrap();

        assert!(
            slot.stray_staging().is_empty(),
            "only directories are strays"
        );

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds a gzipped tar with the given `<path, contents>` entries, all
    /// regular files, under a `v1.0.0/` top level.
    fn tarball(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, path, contents.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_matching_checksum_verifies() {
        let bytes = b"bundle";
        let checksums = format!(
            "{}  teleport-web-v1.0.0.tar.gz\n",
            hex(&<sha2::Sha256 as sha2::Digest>::digest(bytes))
        );
        verify(bytes, &checksums, "teleport-web-v1.0.0.tar.gz").unwrap();
    }

    #[test]
    fn a_mismatched_checksum_is_an_error() {
        let checksums = format!("{}  teleport-web-v1.0.0.tar.gz\n", "0".repeat(64));
        let err = verify(b"bundle", &checksums, "teleport-web-v1.0.0.tar.gz").unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn an_archive_not_listed_in_checksums_is_an_error() {
        let err = verify(
            b"bundle",
            "abc  something-else.tar.gz\n",
            "teleport-web-v1.0.0.tar.gz",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not listed"));
    }

    #[test]
    fn extraction_strips_the_top_level_and_restricts_modes() {
        let dir = scratch("extract");
        let dest = dir.join("staging");
        extract(
            &tarball(&[
                ("v1.0.0/index.html", "<html>ui</html>"),
                ("v1.0.0/assets/app.js", "console.log(1)"),
            ]),
            &dest,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("index.html")).unwrap(),
            "<html>ui</html>"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("assets/app.js")).unwrap(),
            "console.log(1)"
        );
        let mode = std::fs::metadata(dest.join("index.html"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_entry_that_escapes_the_staging_directory_is_rejected() {
        let dir = scratch("escape");
        let dest = dir.join("staging");
        let err = extract(&escaping_tarball(), &dest).unwrap_err();
        assert!(
            err.to_string().contains("escapes the staging directory"),
            "unexpected error: {err}"
        );
        assert!(!dir.join("pwned").exists());
        assert!(!dest.parent().unwrap().join("pwned").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `tar::Builder` refuses to *write* a `..` path, so the header's name
    /// field is filled in directly -- the point of the test is an archive
    /// built by something that doesn't share those scruples.
    fn escaping_tarball() -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        {
            let name = b"v1.0.0/../../pwned";
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..name.len()].copy_from_slice(name);
        }
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_symlink_entry_is_rejected() {
        let dir = scratch("symlink-entry");
        let dest = dir.join("staging");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, "v1.0.0/link", &b""[..])
            .unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();

        let err = extract(&bytes, &dest).unwrap_err();
        assert!(
            err.to_string().contains("not a regular file"),
            "unexpected error: {err}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_archive_is_rejected() {
        let dir = scratch("empty-archive");
        let err = extract(&tarball(&[]), &dir.join("staging")).unwrap_err();
        assert!(err.to_string().contains("no files"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn install(slot: &Slot, version: &str) {
        let dir = slot.root.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), version).unwrap();
        // versions() orders by mtime; sleeping is the only way to make that
        // ordering deterministic on a filesystem with coarse timestamps.
        std::thread::sleep(std::time::Duration::from_millis(15));
        slot.flip_to(version).unwrap();
    }

    #[test]
    fn retention_keeps_the_newest_three_behind_the_live_one() {
        let dir = scratch("retention");
        let slot = Slot::new(&dir);
        std::fs::create_dir_all(&slot.root).unwrap();
        for version in ["v1.0.0", "v1.0.1", "v1.0.2", "v1.0.3", "v1.0.4"] {
            install(&slot, version);
        }
        prune(&slot, "v1.0.4").unwrap();

        let kept: Vec<String> = slot
            .versions()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, vec!["v1.0.4", "v1.0.3", "v1.0.2", "v1.0.1"]);
        assert_eq!(slot.current_version().as_deref(), Some("v1.0.4"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_returns_to_the_previous_version_and_then_stops() {
        let dir = scratch("rollback");
        let slot = Slot::new(&dir);
        std::fs::create_dir_all(&slot.root).unwrap();
        install(&slot, "v1.0.0");
        install(&slot, "v1.0.1");

        rollback(&dir).unwrap();
        assert_eq!(slot.current_version().as_deref(), Some("v1.0.0"));

        // Idempotent at the oldest retained version: nowhere left to go is
        // not a failure.
        rollback(&dir).unwrap();
        assert_eq!(slot.current_version().as_deref(), Some("v1.0.0"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_non_loopback_url_is_refused() {
        reject_remote(None).unwrap();
        reject_remote(Some("http://127.0.0.1:7337")).unwrap();
        reject_remote(Some("http://localhost:7337/")).unwrap();
        let err = reject_remote(Some("https://mainpc.tail1234.ts.net")).unwrap_err();
        assert!(err.to_string().contains("run it on that host"));
    }
}
