//! Pure planning half of the `tmux` shim install (no I/O, no globals).
//!
//! Karvex prepends `<data_dir>/shims` to every managed pane's `PATH`, but the
//! pane's own shell startup gets the last word: `path_helper` (macOS
//! `/etc/zprofile`), `brew shellenv`, `fish_add_path`, mise/asdf/nix hooks and
//! plain `export PATH="...:$PATH"` all re-order `PATH` *after* Karvex has
//! handed it over. Measured on a stock fish setup, a shim directory Karvex
//! passed as `PATH[0]` came out of shell startup at index 9. That is harmless
//! until some *other* `tmux` sits in one of the directories that jumped ahead
//! of it — then Claude Code's Agent Teams backend runs that real tmux against
//! Karvex's socket, the call fails, and Claude silently falls back to
//! in-process teammates instead of spawning Karvex panes.
//!
//! So the prepend alone cannot carry the feature. When (and only when) a
//! foreign `tmux` is on `PATH` at all, Karvex also installs the shim *beside
//! its own binary* — the `PATH` directory that makes `kvx` runnable by name,
//! which the user chose when they installed Karvex and which therefore needs
//! no guessing. This module decides that, and nothing else: every filesystem
//! question it needs is asked through [`ShimFacts`], so the policy is
//! unit-testable without touching a disk.

use std::path::{Path, PathBuf};

/// What a `tmux` entry in some `PATH` directory belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TmuxEntry {
    /// No `tmux` in this directory.
    Absent,
    /// A `tmux` Karvex installed, so it resolves to the shim already.
    Ours,
    /// Somebody else's `tmux` — a real one. Never touched, only worked around.
    Foreign,
}

/// The filesystem questions [`plan_mirror`] needs answered. Implemented for
/// real directories by the installer; implemented by a fixture map in tests.
pub(super) trait ShimFacts {
    fn tmux_entry(&self, dir: &Path) -> TmuxEntry;

    /// Whether this directory makes the *running* Karvex binary reachable by
    /// name — a `kvx` entry here resolves to the executable this process is.
    fn holds_running_exe(&self, dir: &Path) -> bool;
}

/// What the installer should do beyond `<data_dir>/shims`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct MirrorPlan {
    /// A mirror Karvex installed earlier that must keep pointing at the
    /// current binary, whether or not a new one is called for.
    pub(super) refresh: Option<PathBuf>,
    /// Directory to install a mirror into, ahead of the foreign `tmux`.
    pub(super) mirror_dir: Option<PathBuf>,
    /// A foreign `tmux` that wins the lookup and that Karvex cannot get ahead
    /// of. Teammates will not reach Karvex; worth one warning.
    pub(super) shadowed_by: Option<PathBuf>,
}

/// Decides whether a mirror is needed and where it goes.
///
/// `path_entries` is `PATH` in order, as the Karvex server itself sees it —
/// a post-shell-startup `PATH`, and therefore the best available prediction of
/// the order a pane's shell will end up with.
pub(super) fn plan_mirror(facts: &impl ShimFacts, path_entries: &[PathBuf]) -> MirrorPlan {
    let entries = deduplicate(path_entries);

    // A `tmux` outside Karvex's own data directory is machine-visible state:
    // it sits on the user's `PATH` for everything they run, not just for
    // panes. Karvex only writes one next to its own binary, and only when this
    // `PATH` can already reach that binary by name — so a build in
    // `target/debug`, a binary run out of a download directory, or a server
    // the test suite spawned writes nothing outside its own data directory
    // (the same instinct as D6's exact-stem rule, one level up).
    let Some(installed_at) = entries.iter().position(|dir| facts.holds_running_exe(dir)) else {
        return MirrorPlan::default();
    };
    let install_dir = &entries[installed_at];
    // Karvex's own binary living in a package manager's prefix does not make
    // that prefix Karvex's to write into.
    let may_write = !is_package_manager_dir(install_dir);

    // A mirror outlives the condition that created it and keeps answering
    // `tmux` for the whole machine, so an existing one is repointed at the
    // current binary before anything else is decided.
    let refresh = (may_write && facts.tmux_entry(install_dir) == TmuxEntry::Ours)
        .then(|| install_dir.clone());

    // Nothing on PATH can shadow the shim, so the pane's PATH prepend is
    // sufficient on its own and Karvex writes no new `tmux` anywhere.
    let Some(foreign_index) = entries
        .iter()
        .position(|dir| facts.tmux_entry(dir) == TmuxEntry::Foreign)
    else {
        return MirrorPlan {
            refresh,
            ..MirrorPlan::default()
        };
    };

    // Behind the foreign tmux a mirror would shadow the user's `PATH` and
    // still lose the lookup, so it is not written at all.
    let mirror_dir = (may_write && installed_at < foreign_index).then(|| install_dir.clone());
    let shadowed_by = mirror_dir.is_none().then(|| entries[foreign_index].clone());

    MirrorPlan {
        refresh,
        mirror_dir,
        shadowed_by,
    }
}

/// Directories a package manager owns. Karvex never writes a `tmux` into one,
/// even when it is the directory Karvex's own binary was installed into: a
/// stray `tmux` inside a Homebrew or Nix prefix is that tool's to manage, and
/// shadowing `brew install tmux` there is exactly the hijack the shim's
/// ownership rules exist to prevent.
fn is_package_manager_dir(dir: &Path) -> bool {
    const MANAGED_PREFIXES: &[&str] = &[
        "/nix/store",
        "/usr",
        "/bin",
        "/sbin",
        "/opt/homebrew",
        "/opt/local",
        "/home/linuxbrew",
        "/snap",
        "/var/lib/flatpak",
        "/Library",
        "/System",
    ];
    let Some(dir) = dir.to_str() else {
        return true;
    };
    MANAGED_PREFIXES
        .iter()
        .any(|prefix| dir == *prefix || dir.starts_with(&format!("{prefix}/")))
        || dir.contains("/Cellar/")
        || dir.contains("/.linuxbrew/")
        || dir.contains("/.nix-profile/")
}

fn deduplicate(entries: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.as_os_str().is_empty() || seen.contains(entry) {
            continue;
        }
        seen.push(entry.clone());
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const INSTALL_DIR: &str = "/home/u/.local/bin";

    struct Fixture {
        tmux: HashMap<PathBuf, TmuxEntry>,
        /// The `PATH` directory `kvx` is reachable from, i.e. where Karvex is
        /// installed. `None` models a binary nobody can run by name: a
        /// `target/debug` build under the test suite, a downloaded release.
        install_dir: Option<PathBuf>,
    }

    impl Fixture {
        fn new(entries: &[(&str, TmuxEntry)]) -> Self {
            Self {
                tmux: entries
                    .iter()
                    .map(|(dir, entry)| (PathBuf::from(dir), *entry))
                    .collect(),
                install_dir: Some(PathBuf::from(INSTALL_DIR)),
            }
        }

        fn installed_in(mut self, dir: Option<&str>) -> Self {
            self.install_dir = dir.map(PathBuf::from);
            self
        }
    }

    impl ShimFacts for Fixture {
        fn tmux_entry(&self, dir: &Path) -> TmuxEntry {
            self.tmux.get(dir).copied().unwrap_or(TmuxEntry::Absent)
        }

        fn holds_running_exe(&self, dir: &Path) -> bool {
            self.install_dir.as_deref() == Some(dir)
        }
    }

    fn paths(entries: &[&str]) -> Vec<PathBuf> {
        entries.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn no_foreign_tmux_needs_no_mirror() {
        let facts = Fixture::new(&[("/home/u/.config/karvex/shims", TmuxEntry::Ours)]);
        let plan = plan_mirror(
            &facts,
            &paths(&[INSTALL_DIR, "/home/u/.config/karvex/shims", "/usr/bin"]),
        );
        assert_eq!(plan, MirrorPlan::default());
    }

    #[test]
    fn a_foreign_tmux_behind_the_install_dir_gets_a_mirror() {
        // The macOS shape this exists for: brew's tmux sits in a directory
        // `brew shellenv` prepends, and the directory Karvex was installed
        // into (`~/.local/bin`, which `.zshrc` prepends last) is ahead of it.
        let facts = Fixture::new(&[("/opt/homebrew/bin", TmuxEntry::Foreign)]);
        let plan = plan_mirror(
            &facts,
            &paths(&[INSTALL_DIR, "/opt/homebrew/bin", "/usr/bin"]),
        );
        assert_eq!(plan.mirror_dir, Some(PathBuf::from(INSTALL_DIR)));
        assert!(plan.shadowed_by.is_none());
    }

    #[test]
    fn an_install_dir_behind_the_foreign_tmux_is_not_written_to() {
        // Installing there would shadow the user's PATH and still lose.
        let facts = Fixture::new(&[("/usr/bin", TmuxEntry::Foreign)]);
        let plan = plan_mirror(&facts, &paths(&["/usr/bin", INSTALL_DIR]));
        assert_eq!(plan.mirror_dir, None);
        assert_eq!(plan.shadowed_by, Some(PathBuf::from("/usr/bin")));
    }

    #[test]
    fn a_foreign_tmux_in_the_install_dir_is_never_replaced() {
        let facts = Fixture::new(&[(INSTALL_DIR, TmuxEntry::Foreign)]);
        let plan = plan_mirror(&facts, &paths(&[INSTALL_DIR, "/usr/bin"]));
        assert_eq!(plan.mirror_dir, None);
        assert_eq!(plan.refresh, None);
        assert_eq!(plan.shadowed_by, Some(PathBuf::from(INSTALL_DIR)));
    }

    #[test]
    fn an_uninstalled_binary_never_writes_outside_its_data_dir() {
        // `target/debug/kvx` under `cargo nextest`, a downloaded release
        // binary, `cargo run`: none of them are reachable by name from this
        // PATH, so none of them may touch a directory on it — not even to
        // repoint a mirror a real install left behind.
        let facts = Fixture::new(&[
            (INSTALL_DIR, TmuxEntry::Ours),
            ("/usr/bin", TmuxEntry::Foreign),
        ])
        .installed_in(None);
        let plan = plan_mirror(&facts, &paths(&[INSTALL_DIR, "/usr/bin"]));
        assert_eq!(plan, MirrorPlan::default());
    }

    #[test]
    fn a_mirror_only_ever_lands_next_to_the_binary_that_owns_it() {
        // The test suite spawns the binary under test from its own bin
        // directory, with the developer's real `PATH` appended. Karvex must
        // mirror into that directory and nowhere else — a `~/.local/bin` that
        // happens to be further down the same `PATH` is not Karvex's to write
        // to just because the developer put Karvex there once.
        let facts = Fixture::new(&[("/usr/bin", TmuxEntry::Foreign)])
            .installed_in(Some("/tmp/test-base/bin"));
        let plan = plan_mirror(
            &facts,
            &paths(&["/tmp/test-base/bin", INSTALL_DIR, "/usr/bin"]),
        );
        assert_eq!(plan.mirror_dir, Some(PathBuf::from("/tmp/test-base/bin")));
    }

    #[test]
    fn an_existing_mirror_is_refreshed_even_with_nothing_to_shadow() {
        // A mirror answers `tmux` for everything on the machine, so a link
        // left dangling by an upgrade has to be repointed rather than ignored.
        let facts = Fixture::new(&[(INSTALL_DIR, TmuxEntry::Ours)]);
        let plan = plan_mirror(&facts, &paths(&[INSTALL_DIR, "/usr/bin"]));
        assert_eq!(plan.refresh, Some(PathBuf::from(INSTALL_DIR)));
        assert_eq!(plan.mirror_dir, None);
    }

    #[test]
    fn our_own_mirror_is_not_mistaken_for_a_shadow() {
        // Second run: the mirror installed on the first run must read as ours,
        // otherwise Karvex would treat itself as the tmux to get ahead of.
        let facts = Fixture::new(&[
            (INSTALL_DIR, TmuxEntry::Ours),
            ("/usr/bin", TmuxEntry::Foreign),
        ]);
        let plan = plan_mirror(&facts, &paths(&[INSTALL_DIR, "/usr/bin"]));
        assert_eq!(plan.mirror_dir, Some(PathBuf::from(INSTALL_DIR)));
        assert_eq!(plan.refresh, Some(PathBuf::from(INSTALL_DIR)));
        assert!(plan.shadowed_by.is_none());
    }

    #[test]
    fn duplicate_path_entries_do_not_change_the_decision() {
        let facts = Fixture::new(&[("/usr/bin", TmuxEntry::Foreign)]);
        let plan = plan_mirror(
            &facts,
            &paths(&[INSTALL_DIR, INSTALL_DIR, "", "/usr/bin", INSTALL_DIR]),
        );
        assert_eq!(plan.mirror_dir, Some(PathBuf::from(INSTALL_DIR)));
    }

    #[test]
    fn a_package_manager_prefix_is_never_written_into() {
        // A Homebrew- or Nix-installed Karvex does not get to put a `tmux`
        // next to itself; that prefix belongs to the package manager.
        for managed in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/nix/store/abc-karvex-1.0/bin",
            "/home/u/.nix-profile/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/opt/homebrew/Cellar/karvex/1.0/bin",
        ] {
            let facts =
                Fixture::new(&[("/usr/bin", TmuxEntry::Foreign)]).installed_in(Some(managed));
            let plan = plan_mirror(&facts, &paths(&[managed, "/usr/bin"]));
            assert_eq!(plan.mirror_dir, None, "{managed} must not be written into");
            assert_eq!(
                plan.shadowed_by,
                Some(PathBuf::from("/usr/bin")),
                "{managed} cannot be worked around, so the user is told"
            );
        }
    }
}
