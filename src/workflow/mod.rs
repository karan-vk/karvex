//! Workflow subsystem: kvdag definitions, run execution, and persistence.
//!
//! Layering follows `docs/design/workflow-builder/04-kvdag-and-execution.md`
//! §1. `model`, `tier`, and `layout` are pure: no `App`, no PTYs, no
//! ratatui, no SurrealDB, and unit-testable the way `AppState::test_new()` is.
//! `binding` is the only module that talks to the runtime, and `store` is the
//! only one that talks to SurrealDB.
//!
//! Every module but `store` compiles unconditionally. `store` sits behind the
//! `workflow` cargo feature, which is on by default: there is one karvex
//! binary per platform and the workflow subsystem is always in it. The gate
//! exists so `--no-default-features` can drop SurrealDB for the MSVC
//! cross-lint and slim source builds.

// The module tree lands one step ahead of the code that consumes it, so items
// are unused until their workstream lands. Remove once the subsystem is fully
// wired into the server.
#![allow(dead_code)]

pub mod binding;
pub mod compile_findings;
pub mod definition;
pub mod layout;
pub mod lead_prompt;
pub mod model;
pub mod projection;
pub mod review;
pub mod review_prompt;
#[cfg(feature = "workflow")]
pub mod store;
pub mod tier;
pub mod watchdog;

/// Cheap grep guard for the pure/binding/store split this module's doc comment
/// states: none of the pure files may reference the runtime (`App`, ratatui)
/// or the store crate (SurrealDB). `06-phase2-plan.md`/`07-phase3-plan.md`
/// call this "the pure-layer grep test"; it lapsed when the engine-era tree
/// that carried it was deleted (`ab068de7`), so `phase4-retarget-plan.md` P1
/// restores it before its own additions (`watchdog`, `review`,
/// `review_prompt`, `compile_findings`) join the layer it guards.
///
/// Adding a new pure file to this module means adding it to `PURE_FILES` too
/// — there is no way to enumerate `src/workflow/*.rs` at compile time, so this
/// list is the enforcement point, not a convenience.
#[cfg(test)]
mod pure_layer_grep {
    const PURE_FILES: &[(&str, &str)] = &[
        ("model.rs", include_str!("model.rs")),
        ("tier.rs", include_str!("tier.rs")),
        ("layout.rs", include_str!("layout.rs")),
        ("lead_prompt.rs", include_str!("lead_prompt.rs")),
        ("projection.rs", include_str!("projection.rs")),
        ("definition.rs", include_str!("definition.rs")),
        ("watchdog.rs", include_str!("watchdog.rs")),
        ("review.rs", include_str!("review.rs")),
        ("review_prompt.rs", include_str!("review_prompt.rs")),
        ("compile_findings.rs", include_str!("compile_findings.rs")),
    ];

    /// Substrings that mean a pure file has acquired a runtime or store
    /// dependency. Checked against non-comment lines only (`code_lines`
    /// below), so this module's own doc comment — which names both crates by
    /// word — never trips it.
    const FORBIDDEN: &[&str] = &[
        "ratatui",
        "surrealdb",
        "crate::app::",
        "crate::pane::",
        "TerminalRuntime",
        "PtyHandle",
        "tokio::process",
        "std::process::Command",
    ];

    /// Drops `//`, `///`, and `//!` comment lines, so a needle appearing only
    /// in prose (documenting *why* a dependency is forbidden, say) never
    /// counts as the file having one.
    fn code_lines(source: &str) -> impl Iterator<Item = &str> {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
    }

    #[test]
    fn pure_files_have_no_runtime_or_store_dependency() {
        for (name, source) in PURE_FILES {
            for line in code_lines(source) {
                for needle in FORBIDDEN {
                    assert!(
                        !line.contains(needle),
                        "src/workflow/{name} references \"{needle}\" outside a comment \
                         (line: {line:?}), which the pure layer (this module's doc comment) \
                         forbids"
                    );
                }
            }
        }
    }
}
