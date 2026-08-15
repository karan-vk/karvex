//! Filesystem half of the `tmux` shim install: one symlink, installed under
//! ownership rules strict enough that Karvex can never hijack somebody else's
//! `tmux`.
//!
//! Kept separate from the policy in [`super::plan`] so the "where does a shim
//! go" decision and the "how is a shim safely written" mechanism can each be
//! tested on their own. Both the `<data_dir>/shims` entry and the PATH mirror
//! go through the same function here — the guards are the load-bearing part
//! (see the port plan's R2/R8), so there is exactly one copy of them.

use std::io;
use std::path::Path;

use tracing::warn;

/// Karvex's own primary binary name.
pub(super) const KARVEX_BINARY_NAME: &str = "kvx";

/// True only for Karvex's own primary binary name, `kvx`, exactly.
///
/// This is the nextest guard (D6): cargo test binaries are named
/// `kvx-<hash>` / `karvex-<hash>` under `target/*/deps`, so an exact match
/// keeps them from ever installing a shim that could hijack a user's real
/// `tmux`. It is deliberately a pure function over just the stem so it is
/// unit-testable without spawning binaries.
pub(super) fn binary_owns_shim(stem: &str) -> bool {
    stem == KARVEX_BINARY_NAME
}

/// A broader, separate "is this pre-existing shim symlink one Karvex
/// created" test, used only to decide whether an existing link at the shim
/// path is safe to replace or re-point. Deliberately more permissive than
/// [`binary_owns_shim`]: a link whose recorded target lives under our own
/// data dir, or whose stem is `kvx`, or starts with `kvx-`/`karvex-`, is
/// ours; anything else (a real tmux, an unrelated tool) is left alone (D6).
fn existing_link_is_ours(recorded_target: &Path, data_dir: &Path) -> bool {
    if recorded_target.starts_with(data_dir) {
        return true;
    }
    recorded_target
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            binary_owns_shim(stem) || stem.starts_with("kvx-") || stem.starts_with("karvex-")
        })
}

/// Whether a path currently holds a `tmux` Karvex may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkOwner {
    /// Nothing there.
    Vacant,
    /// A Karvex-installed symlink, safe to re-point (including a dangling one
    /// left behind by a package-manager upgrade).
    Ours,
    /// A real file or a foreign symlink. Never touched.
    Foreign,
}

/// Classifies whatever sits at `link_path` without modifying it.
///
/// Uses `symlink_metadata`, not `metadata`, so a link whose target has since
/// vanished still reads as a link rather than as "nothing here".
pub(super) fn link_owner(link_path: &Path, data_dir: &Path) -> LinkOwner {
    let Ok(metadata) = std::fs::symlink_metadata(link_path) else {
        return LinkOwner::Vacant;
    };
    if !metadata.file_type().is_symlink() {
        return LinkOwner::Foreign;
    }
    match std::fs::read_link(link_path) {
        Ok(recorded_target) if existing_link_is_ours(&recorded_target, data_dir) => LinkOwner::Ours,
        _ => LinkOwner::Foreign,
    }
}

fn create_symlink(target: &Path, link_path: &Path) -> io::Result<bool> {
    match std::os::unix::fs::symlink(target, link_path) {
        Ok(()) => Ok(true),
        // A concurrent pane spawn may have already won the race to create
        // this exact link; that is success, not a conflict.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(true),
        Err(err) => Err(err),
    }
}

/// Installs, repoints, or leaves alone a symlink at `link_path` that should
/// point at `target`.
///
/// Returns `Ok(true)` when `link_path` now points at `target` (freshly
/// created, already correct, or repointed because it was ours to repoint —
/// including a dangling link left by a package-manager upgrade). Returns
/// `Ok(false)` when a real file or a foreign symlink already occupies
/// `link_path` and was deliberately left untouched. `Err` is an unexpected
/// I/O failure.
pub(super) fn install_shim_symlink(
    link_path: &Path,
    target: &Path,
    data_dir: &Path,
) -> io::Result<bool> {
    match link_owner(link_path, data_dir) {
        LinkOwner::Vacant => create_symlink(target, link_path),
        LinkOwner::Foreign => {
            warn!(
                path = %link_path.display(),
                "leaving a foreign tmux at the shim path alone"
            );
            Ok(false)
        }
        LinkOwner::Ours => {
            if std::fs::read_link(link_path).is_ok_and(|recorded| recorded == target) {
                return Ok(true);
            }
            std::fs::remove_file(link_path)?;
            create_symlink(target, link_path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::tmux_shim::scratch_dir;

    #[test]
    fn binary_owns_shim_accepts_kvx_and_rejects_test_binaries() {
        assert!(binary_owns_shim("kvx"));
        assert!(!binary_owns_shim("kvx-9f2a1c"));
        assert!(!binary_owns_shim("karvex-9f2a1c"));
        assert!(!binary_owns_shim("tmux"));
        assert!(!binary_owns_shim("bakr"));
    }

    #[test]
    fn install_shim_symlink_refuses_real_file() {
        let dir = scratch_dir("real-file");
        let link_path = dir.join("tmux");
        std::fs::write(&link_path, b"not a symlink").expect("write real file");
        let target = dir.join("kvx");
        std::fs::write(&target, b"binary").expect("write target");

        let installed = install_shim_symlink(&link_path, &target, &dir).expect("no io error");

        assert!(!installed);
        assert_eq!(link_owner(&link_path, &dir), LinkOwner::Foreign);
        assert!(!std::fs::symlink_metadata(&link_path)
            .expect("metadata")
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_shim_symlink_refuses_foreign_symlink() {
        let dir = scratch_dir("foreign-symlink");
        // The foreign target must live *outside* `data_dir` — otherwise the
        // "recorded target lives under our data dir" ownership clause would
        // (correctly) claim it, defeating the point of this test.
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let foreign_target = dir.join("real-tmux");
        std::fs::write(&foreign_target, b"real tmux").expect("write foreign target");
        let link_path = data_dir.join("tmux");
        std::os::unix::fs::symlink(&foreign_target, &link_path).expect("create foreign symlink");
        let our_target = data_dir.join("kvx");
        std::fs::write(&our_target, b"binary").expect("write our target");

        let installed =
            install_shim_symlink(&link_path, &our_target, &data_dir).expect("no io error");

        assert!(!installed);
        assert_eq!(link_owner(&link_path, &data_dir), LinkOwner::Foreign);
        assert_eq!(
            std::fs::read_link(&link_path).expect("read link"),
            foreign_target
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_shim_symlink_replaces_own_link_and_is_idempotent() {
        let dir = scratch_dir("replace-own");
        let old_target = dir.join("kvx-old-version");
        std::fs::write(&old_target, b"binary").expect("write old target");
        let link_path = dir.join("tmux");
        std::os::unix::fs::symlink(&old_target, &link_path).expect("create own symlink");
        let new_target = dir.join("kvx");
        std::fs::write(&new_target, b"binary").expect("write new target");

        let installed_first =
            install_shim_symlink(&link_path, &new_target, &dir).expect("no io error");
        assert!(installed_first);
        assert_eq!(
            std::fs::read_link(&link_path).expect("read link"),
            new_target
        );

        // Idempotent: calling again with the same target is a no-op success.
        let installed_second =
            install_shim_symlink(&link_path, &new_target, &dir).expect("no io error");
        assert!(installed_second);
        assert_eq!(
            std::fs::read_link(&link_path).expect("read link"),
            new_target
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_shim_symlink_repoints_dangling_own_link() {
        let dir = scratch_dir("dangling-own");
        let vanished_target = dir.join("kvx-vanished-store-path");
        // Deliberately never create `vanished_target`: this simulates a
        // Homebrew/Nix/mise upgrade that removed the recorded target while
        // leaving the link itself in place.
        let link_path = dir.join("tmux");
        std::os::unix::fs::symlink(&vanished_target, &link_path)
            .expect("create dangling own symlink");
        let new_target = dir.join("kvx");
        std::fs::write(&new_target, b"binary").expect("write new target");

        assert_eq!(link_owner(&link_path, &dir), LinkOwner::Ours);
        let installed = install_shim_symlink(&link_path, &new_target, &dir).expect("no io error");

        assert!(installed);
        assert_eq!(
            std::fs::read_link(&link_path).expect("read link"),
            new_target
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_shim_symlink_leaves_dangling_foreign_link_alone() {
        let dir = scratch_dir("dangling-foreign");
        // As above: the dangling target's path must live outside `data_dir`
        // so only its stem is under test, not the data-dir containment
        // clause.
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let vanished_target = dir.join("some-other-tool-9f2a1c");
        let link_path = data_dir.join("tmux");
        std::os::unix::fs::symlink(&vanished_target, &link_path)
            .expect("create dangling foreign symlink");
        let new_target = data_dir.join("kvx");
        std::fs::write(&new_target, b"binary").expect("write new target");

        let installed =
            install_shim_symlink(&link_path, &new_target, &data_dir).expect("no io error");

        assert!(!installed);
        assert_eq!(
            std::fs::read_link(&link_path).expect("read link"),
            vanished_target
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_owner_reports_a_vacant_path() {
        let dir = scratch_dir("vacant");
        assert_eq!(link_owner(&dir.join("tmux"), &dir), LinkOwner::Vacant);
        let _ = std::fs::remove_dir_all(dir);
    }
}
