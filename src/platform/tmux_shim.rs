//! Unix installation of the `tmux` shim symlinks that give Claude Code (and
//! anything else that dispatches on tmux presence) a `tmux` binary to find on
//! PATH inside a Karvex pane. See `docs/design/claude-teammates/01-port-plan.md`
//! D1/D2/D6 for the design this implements, and R2/R8 for why the guards here
//! are load-bearing: a wrong move hijacks a user's real `tmux` or shadows an
//! unrelated binary for every process the user runs.
//!
//! Two placements, one mechanism ([`link::install_shim_symlink`]):
//!
//! * `<data_dir>/shims/tmux` — always installed. Karvex prepends this
//!   directory to every managed pane's `PATH`.
//! * a *mirror* beside Karvex's own binary on `PATH` — installed only when
//!   some foreign `tmux` would otherwise win the lookup, because a pane's
//!   shell re-orders `PATH` during its own startup and routinely demotes the
//!   prepended directory (`path_helper`, `brew shellenv`, `fish_add_path`,
//!   mise/asdf/nix hooks). [`plan`] owns that decision and explains it.

mod link;
mod plan;

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use plan::{ShimFacts, TmuxEntry};

/// The one name Karvex ever writes into a shim directory. The `<data_dir>/shims`
/// directory is prepended to every managed pane's `PATH`, so it must never
/// become a general-purpose bin directory (R8's single-entry invariant).
const TMUX_BINARY_NAME: &str = "tmux";

pub(super) fn ensure_tmux_shim_dir_platform() -> Option<PathBuf> {
    ensure_tmux_shim_dir_in(&crate::session::data_dir())
}

fn ensure_tmux_shim_dir_in(data_dir: &Path) -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let stem = current_exe.file_stem().and_then(|stem| stem.to_str())?;
    if !link::binary_owns_shim(stem) {
        return None;
    }

    let shims_dir = install_shims_dir(data_dir, &current_exe)?;
    ensure_shim_reachable_on_path(data_dir, &current_exe);
    Some(shims_dir)
}

/// Creates `<data_dir>/shims` (if needed) and installs the `tmux` shim inside
/// it pointing at `target`. Returns the shims directory on success.
fn install_shims_dir(data_dir: &Path, target: &Path) -> Option<PathBuf> {
    let shims_dir = data_dir.join("shims");
    if let Err(err) = std::fs::create_dir_all(&shims_dir) {
        warn!(
            error = %err,
            path = %shims_dir.display(),
            "failed to create the tmux shim directory"
        );
        return None;
    }

    install_shim(&shims_dir, target, data_dir).then_some(shims_dir)
}

/// Installs `<dir>/tmux` pointing at `target`. Returns whether `tmux` in that
/// directory now resolves to Karvex.
fn install_shim(dir: &Path, target: &Path, data_dir: &Path) -> bool {
    let link_path = dir.join(TMUX_BINARY_NAME);
    match link::install_shim_symlink(&link_path, target, data_dir) {
        Ok(installed) => installed,
        Err(err) => {
            warn!(
                error = %err,
                path = %link_path.display(),
                "failed to install the tmux shim symlink"
            );
            false
        }
    }
}

/// Keeps `tmux` resolving to Karvex even after a pane's shell has re-ordered
/// `PATH` past the prepended shims directory. [`plan::plan_mirror`] decides
/// what that takes; this only carries it out.
fn ensure_shim_reachable_on_path(data_dir: &Path, target: &Path) {
    let path_entries: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    let facts = InstalledShims {
        data_dir,
        running_exe: target,
    };
    let plan = plan::plan_mirror(&facts, &path_entries);

    if let Some(refresh_dir) = &plan.refresh {
        install_shim(refresh_dir, target, data_dir);
    }

    if let Some(mirror_dir) = &plan.mirror_dir {
        if install_shim(mirror_dir, target, data_dir) {
            info!(
                path = %mirror_dir.join(TMUX_BINARY_NAME).display(),
                "installed a tmux shim beside the kvx binary so agent teammates reach Karvex"
            );
        }
    }

    if let Some(shadowed_by) = &plan.shadowed_by {
        warn_shadowed(shadowed_by);
    }
}

/// One warning per process: the shim install runs on every managed pane
/// launch, and repeating this for every pane would drown the log.
fn warn_shadowed(shadowed_by: &Path) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    warn!(
        tmux = %shadowed_by.join(TMUX_BINARY_NAME).display(),
        "another tmux precedes the directory kvx is installed in, so agent teammates may \
         spawn outside Karvex panes; put that install directory earlier on PATH to fix this"
    );
}

/// [`ShimFacts`] adapter: answers the planner's questions against real
/// directories, reusing the same ownership rules the installer writes under.
struct InstalledShims<'a> {
    data_dir: &'a Path,
    running_exe: &'a Path,
}

impl ShimFacts for InstalledShims<'_> {
    fn tmux_entry(&self, dir: &Path) -> TmuxEntry {
        let path = dir.join(TMUX_BINARY_NAME);
        match link::link_owner(&path, self.data_dir) {
            link::LinkOwner::Vacant => TmuxEntry::Absent,
            link::LinkOwner::Ours => TmuxEntry::Ours,
            // A directory named `tmux` is not a tmux to get ahead of.
            link::LinkOwner::Foreign if path.is_dir() => TmuxEntry::Absent,
            link::LinkOwner::Foreign => TmuxEntry::Foreign,
        }
    }

    /// Compared after canonicalising both sides, so the usual installs — a
    /// `~/.local/bin/kvx` symlink into a versioned directory, a Homebrew
    /// `bin` entry pointing into the Cellar, a Nix profile link into the
    /// store — all still recognise themselves.
    fn holds_running_exe(&self, dir: &Path) -> bool {
        let candidate = dir.join(link::KARVEX_BINARY_NAME);
        match (candidate.canonicalize(), self.running_exe.canonicalize()) {
            (Ok(candidate), Ok(running_exe)) => candidate == running_exe,
            _ => false,
        }
    }
}

/// Shared by this module's tests and [`link`]'s: a unique, throwaway
/// directory per test, so nothing here ever races or reuses state.
#[cfg(test)]
pub(super) fn scratch_dir(label: &str) -> PathBuf {
    static UNIQUE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = UNIQUE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "karvex-tmux-shim-test-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_binary(path: &Path) {
        std::fs::write(path, b"binary").expect("write binary");
    }

    #[test]
    fn shims_dir_contains_only_tmux() {
        let dir = scratch_dir("single-entry");
        let target = dir.join("kvx");
        write_binary(&target);

        let shims_dir = install_shims_dir(&dir, &target).expect("install succeeds");

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

    /// Drives `ensure_shim_reachable_on_path` against a fake `PATH`.
    fn with_path<R>(path: &[&Path], body: impl FnOnce() -> R) -> R {
        let _lock = crate::integration::integration_env_lock();
        let previous_path = std::env::var_os("PATH");
        std::env::set_var(
            "PATH",
            std::env::join_paths(path.iter()).expect("join fake PATH"),
        );

        let outcome = body();

        match previous_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        outcome
    }

    /// A fake machine: a data dir, a directory holding a real tmux, and an
    /// install directory where `kvx` is reachable by name — the state that
    /// makes a running binary an *installed* Karvex.
    struct Machine {
        root: PathBuf,
        data_dir: PathBuf,
        install_bin: PathBuf,
        foreign_bin: PathBuf,
        target: PathBuf,
    }

    impl Machine {
        fn new(label: &str) -> Self {
            let root = scratch_dir(label);
            let data_dir = root.join("data");
            let install_bin = root.join("install");
            let foreign_bin = root.join("foreign");
            for path in [&data_dir, &install_bin, &foreign_bin] {
                std::fs::create_dir_all(path).expect("create dir");
            }
            let target = data_dir.join("kvx");
            write_binary(&target);
            std::os::unix::fs::symlink(&target, install_bin.join("kvx"))
                .expect("install kvx under its own name");
            Self {
                root,
                data_dir,
                install_bin,
                foreign_bin,
                target,
            }
        }

        fn install_a_real_tmux(&self) {
            write_binary(&self.foreign_bin.join("tmux"));
        }

        fn ensure_with_path(&self, path: &[&Path]) {
            with_path(path, || {
                ensure_shim_reachable_on_path(&self.data_dir, &self.target)
            });
        }

        fn mirror(&self) -> PathBuf {
            self.install_bin.join("tmux")
        }
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_foreign_tmux_behind_the_install_dir_gets_a_mirror() {
        // The macOS/Homebrew shape: the pane's shell demotes the prepended
        // shims directory, so a real tmux would win unless Karvex also sits
        // beside its own binary, which is already ahead of it.
        let machine = Machine::new("mirror-needed");
        machine.install_a_real_tmux();

        machine.ensure_with_path(&[&machine.install_bin, &machine.foreign_bin]);

        assert_eq!(
            std::fs::read_link(machine.mirror()).expect("mirror link"),
            machine.target
        );
    }

    #[test]
    fn no_foreign_tmux_means_no_mirror_is_written() {
        // Blast radius: with nothing to lose the lookup to, Karvex must not
        // put a `tmux` on the user's PATH at all.
        let machine = Machine::new("mirror-unneeded");

        machine.ensure_with_path(&[&machine.install_bin, &machine.foreign_bin]);

        assert!(!machine.mirror().exists());
    }

    #[test]
    fn a_mirror_behind_the_foreign_tmux_is_not_written() {
        let machine = Machine::new("mirror-too-late");
        machine.install_a_real_tmux();

        machine.ensure_with_path(&[&machine.foreign_bin, &machine.install_bin]);

        assert!(
            !machine.mirror().exists(),
            "shadowing a user's PATH buys nothing when the real tmux still wins"
        );
    }

    #[test]
    fn a_binary_that_is_not_installed_on_path_writes_nothing_outside_its_data_dir() {
        // The regression this guards: the test suite spawns the real `kvx`
        // binary out of `target/debug`, with the developer's own `$PATH`
        // inherited. Without this rule a test run would install — or
        // repoint — a `tmux` in a directory on that PATH.
        let machine = Machine::new("uninstalled-binary");
        machine.install_a_real_tmux();
        std::os::unix::fs::symlink(machine.data_dir.join("kvx-stale"), machine.mirror())
            .expect("pre-existing mirror from a real install");

        // The install directory is deliberately left off PATH: nothing here
        // can run `kvx` by name.
        machine.ensure_with_path(&[&machine.foreign_bin]);

        assert_eq!(
            std::fs::read_link(machine.mirror()).expect("mirror link"),
            machine.data_dir.join("kvx-stale"),
            "an uninstalled binary must not even repoint an existing mirror"
        );
    }

    #[test]
    fn an_existing_mirror_is_repointed_even_when_no_mirror_is_needed() {
        // A mirror outlives the condition that created it and is on PATH for
        // everything the user runs, so a link left dangling by an upgrade has
        // to be repointed rather than ignored.
        let machine = Machine::new("mirror-refresh");
        std::os::unix::fs::symlink(machine.data_dir.join("kvx-vanished"), machine.mirror())
            .expect("stale mirror");

        machine.ensure_with_path(&[&machine.install_bin]);

        assert_eq!(
            std::fs::read_link(machine.mirror()).expect("mirror link"),
            machine.target
        );
    }

    #[test]
    fn a_foreign_tmux_beside_the_binary_is_never_replaced() {
        let machine = Machine::new("mirror-foreign-candidate");
        machine.install_a_real_tmux();
        write_binary(&machine.mirror());

        machine.ensure_with_path(&[&machine.install_bin, &machine.foreign_bin]);

        assert!(
            std::fs::symlink_metadata(machine.mirror())
                .expect("still there")
                .file_type()
                .is_file(),
            "a real tmux beside the kvx binary stays exactly as it was"
        );
    }

    #[test]
    fn installed_shims_classifies_by_ownership() {
        let dir = scratch_dir("facts");
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let ours_dir = dir.join("ours");
        let foreign_dir = dir.join("foreign");
        let empty_dir = dir.join("empty");
        for path in [&ours_dir, &foreign_dir, &empty_dir] {
            std::fs::create_dir_all(path).expect("create dir");
        }
        let target = data_dir.join("kvx");
        write_binary(&target);
        std::os::unix::fs::symlink(&target, ours_dir.join("tmux")).expect("our link");
        write_binary(&foreign_dir.join("tmux"));

        let facts = InstalledShims {
            data_dir: &data_dir,
            running_exe: &target,
        };

        assert_eq!(facts.tmux_entry(&ours_dir), TmuxEntry::Ours);
        assert_eq!(facts.tmux_entry(&foreign_dir), TmuxEntry::Foreign);
        assert_eq!(facts.tmux_entry(&empty_dir), TmuxEntry::Absent);
        let _ = std::fs::remove_dir_all(dir);
    }
}
