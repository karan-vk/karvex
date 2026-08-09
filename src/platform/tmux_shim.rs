//! Unix-only installation of the `tmux` shim symlink that gives Claude Code
//! (and anything else that dispatches on tmux presence) a `tmux` binary to
//! find on PATH inside a Karvex pane. See
//! `docs/design/claude-teammates/01-port-plan.md` D1/D2/D6 for the design
//! this implements, and R2/R8 for why the guards here are load-bearing: a
//! wrong move hijacks a user's real `tmux` or shadows an unrelated binary
//! for every process the user runs in the pane.
//!
//! macOS also gets a `~/.local/bin/tmux` mirror (R3): `path_helper`/`brew
//! shellenv`/rc files can re-order PATH after our prepend, so a Homebrew
//! `tmux` would otherwise win.

use std::io;
use std::path::{Path, PathBuf};

use tracing::warn;

/// True only for Karvex's own primary binary name, `kvx`, exactly.
///
/// This is the nextest guard (D6): cargo test binaries are named
/// `kvx-<hash>` / `karvex-<hash>` under `target/*/deps`, so an exact match
/// keeps them from ever installing a shim that could hijack a user's real
/// `tmux`. It is deliberately a pure function over just the stem so it is
/// unit-testable without spawning binaries.
fn binary_owns_shim(stem: &str) -> bool {
    stem == "kvx"
}

/// A broader, separate "is this pre-existing shim symlink one Karvex
/// created" test, used only to decide whether an existing link at the shim
/// path is safe to replace or re-point. Deliberately more permissive than
/// `binary_owns_shim`: a link whose recorded target lives under our own
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
/// including a dangling link left by a package-manager upgrade, inspected
/// via `symlink_metadata` rather than `metadata` so a vanished target does
/// not read as "nothing here"). Returns `Ok(false)` when a real file or a
/// foreign symlink already occupies `link_path` and was deliberately left
/// untouched. `Err` is an unexpected I/O failure.
fn install_shim_symlink(link_path: &Path, target: &Path, data_dir: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(link_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() {
                warn!(
                    path = %link_path.display(),
                    "refusing to replace a non-symlink at the tmux shim path"
                );
                return Ok(false);
            }
            let recorded_target = std::fs::read_link(link_path)?;
            if recorded_target == target {
                return Ok(true);
            }
            if !existing_link_is_ours(&recorded_target, data_dir) {
                warn!(
                    path = %link_path.display(),
                    recorded_target = %recorded_target.display(),
                    "leaving a foreign tmux shim symlink alone"
                );
                return Ok(false);
            }
            std::fs::remove_file(link_path)?;
            create_symlink(target, link_path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_symlink(target, link_path),
        Err(err) => Err(err),
    }
}

/// Creates `<data_dir>/shims` (if needed) and installs the `tmux` shim
/// inside it pointing at `target`, then best-effort mirrors it into
/// `~/.local/bin` on macOS. Returns the shims directory on success.
///
/// Deliberately writes only the single `tmux` entry into the shims
/// directory (R8's single-entry invariant): every managed pane gets this
/// directory prepended to `PATH` ahead of everything else, so it must never
/// become a general-purpose bin dir.
fn install_tmux_shim_dir(data_dir: &Path, target: &Path) -> Option<PathBuf> {
    let shims_dir = data_dir.join("shims");
    if let Err(err) = std::fs::create_dir_all(&shims_dir) {
        warn!(
            error = %err,
            path = %shims_dir.display(),
            "failed to create the tmux shim directory"
        );
        return None;
    }

    let link_path = shims_dir.join("tmux");
    let installed = match install_shim_symlink(&link_path, target, data_dir) {
        Ok(installed) => installed,
        Err(err) => {
            warn!(
                error = %err,
                path = %link_path.display(),
                "failed to install the tmux shim symlink"
            );
            false
        }
    };
    if !installed {
        return None;
    }

    #[cfg(target_os = "macos")]
    install_macos_mirror(target, data_dir);

    Some(shims_dir)
}

/// Mirrors the shim into `~/.local/bin/tmux` (R3). Never creates
/// `~/.local/bin` itself (D6) — that directory outliving an uninstall of
/// Karvex is exactly why W8 documents an explicit removal command instead.
#[cfg(target_os = "macos")]
fn install_macos_mirror(target: &Path, data_dir: &Path) {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return;
    };
    let local_bin = PathBuf::from(home).join(".local").join("bin");
    if !local_bin.is_dir() {
        return;
    }

    let link_path = local_bin.join("tmux");
    if let Err(err) = install_shim_symlink(&link_path, target, data_dir) {
        warn!(
            error = %err,
            path = %link_path.display(),
            "failed to install the macOS ~/.local/bin tmux mirror"
        );
    }
}

fn ensure_tmux_shim_dir_in(data_dir: &Path) -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let stem = current_exe.file_stem().and_then(|stem| stem.to_str())?;
    if !binary_owns_shim(stem) {
        return None;
    }
    install_tmux_shim_dir(data_dir, &current_exe)
}

pub(super) fn ensure_tmux_shim_dir_platform() -> Option<PathBuf> {
    ensure_tmux_shim_dir_in(&crate::session::data_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    static UNIQUE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn scratch_dir(label: &str) -> PathBuf {
        let unique = UNIQUE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "karvex-tmux-shim-test-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

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
    fn shims_dir_contains_only_tmux() {
        let dir = scratch_dir("single-entry");
        let target = dir.join("kvx");
        std::fs::write(&target, b"binary").expect("write target");

        let shims_dir = install_tmux_shim_dir(&dir, &target).expect("install succeeds");

        let entries: Vec<_> = std::fs::read_dir(&shims_dir)
            .expect("read shims dir")
            .map(|entry| entry.expect("dir entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("tmux")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_tmux_shim_dir_platform_refuses_non_kvx_test_binary() {
        // Under `cargo test`/`cargo nextest`, `current_exe()`'s stem is
        // never exactly `kvx` (it is `kvx-<hash>` / `<crate>-<hash>`), so
        // this must refuse before ever touching the filesystem — the same
        // guarantee W7's `nextest_binary_never_installs_a_tmux_shim` checks
        // end to end through the real built binary (D6).
        assert!(ensure_tmux_shim_dir_platform().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_mirror_is_not_created_when_local_bin_is_absent() {
        let _lock = crate::integration::integration_env_lock();
        let dir = scratch_dir("macos-mirror-absent");
        let fake_home = dir.join("home");
        std::fs::create_dir_all(&fake_home).expect("create fake home");
        let target = dir.join("kvx");
        std::fs::write(&target, b"binary").expect("write target");

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &fake_home);

        install_macos_mirror(&target, &dir);

        let local_bin = fake_home.join(".local").join("bin");
        assert!(!local_bin.exists(), "must never create ~/.local/bin");

        match previous_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_mirror_is_created_when_local_bin_exists() {
        let _lock = crate::integration::integration_env_lock();
        let dir = scratch_dir("macos-mirror-present");
        let fake_home = dir.join("home");
        let local_bin = fake_home.join(".local").join("bin");
        std::fs::create_dir_all(&local_bin).expect("create fake local bin");
        let target = dir.join("kvx");
        std::fs::write(&target, b"binary").expect("write target");

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &fake_home);

        install_macos_mirror(&target, &dir);

        assert_eq!(
            std::fs::read_link(local_bin.join("tmux")).expect("mirror link"),
            target
        );

        match previous_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
