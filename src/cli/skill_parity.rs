//! Parity pin between `skills/karvex/SKILL.md`'s workflow-operating prose and
//! the real `kvx workflow` surface (`.local/prd/phase4-retarget-plan.md` §5
//! packet P15).
//!
//! The skill is a node/operator *operating* document, not a CLI reference —
//! it is deliberately not exhaustive. What it must never do is name a verb
//! path, a flag, or a `KARVEX_WORKFLOW_*` env var that the CLI does not
//! actually expose, the way `workflows.mdx` once documented a per-node
//! contract (`workflow.node.report`, a minted token, a validated result
//! schema) years after the engine that served it was deleted. This module is
//! the guard against that specific rot, checked two ways for every entry
//! below:
//!
//! 1. **Forward** — the verb path resolves against
//!    [`crate::cli::workflow::VERB_PATHS`] (the manual parser's own
//!    hand-maintained list, itself checked against the clap tree by
//!    `spec.rs`'s parity test) and every named flag exists as a `--long`
//!    argument on the matching [`crate::cli::spec::command()`] subcommand.
//!    Renaming or dropping a verb or a flag in the real CLI fails this
//!    direction.
//! 2. **Backward** — the literal command and flag strings this module
//!    expects to teach are asserted present in the shipped `SKILL.md` text.
//!    Deleting or garbling the paragraph that teaches them fails this
//!    direction, so a prose-only edit cannot silently drop coverage either.
//!
//! `KARVEX_WORKFLOW_*` env var names get the same two-way treatment against
//! the constants in [`crate::workflow::binding::spawn`], and the watchdog's
//! `[karvex · watchdog]` frame is pinned directly against
//! [`crate::workflow::watchdog::WATCHDOG_FRAME`] — the single authority
//! `watchdog.rs` itself claims for that text.
//!
//! Only compiled for `cfg(test)` (the `mod skill_parity;` line in
//! `src/cli.rs` is itself `#[cfg(test)]`), same as `VERB_PATHS`: its sole
//! purpose is this parity test, and `just windows-lint` builds without
//! `--all-targets`.
//!
//! A negative fixture (`a_fabricated_verb_fails`) proves the forward
//! direction actually fails on drift rather than vacuously passing.

use clap::Command;

/// The shipped skill text, embedded at compile time so a check against it
/// can never read a stale copy off disk.
const SKILL_MD: &str = include_str!("../../skills/karvex/SKILL.md");

/// One `kvx workflow …` verb the skill names, and the `--flag`s it uses when
/// it does.
struct DocumentedVerb {
    /// Path into `VERB_PATHS` / the clap tree, e.g. `["run", "finish"]`.
    path: &'static [&'static str],
    /// Long flag names (without the leading `--`) the skill text uses for
    /// this verb. Not every flag the verb accepts — only the ones the skill
    /// actually teaches.
    flags: &'static [&'static str],
}

/// Every `kvx workflow …` verb `SKILL.md`'s workflow section names.
///
/// Deliberately a subset of `VERB_PATHS`: the skill is an operating guide,
/// not a reference, so it does not (and should not) mention every verb the
/// CLI has.
const DOCUMENTED_VERBS: &[DocumentedVerb] = &[
    DocumentedVerb {
        path: &["list"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["show"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["create"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["run", "start"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["run", "show"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["run", "list"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["run", "cancel"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["run", "finish"],
        flags: &["summary-file"],
    },
    DocumentedVerb {
        path: &["run", "message"],
        flags: &["to", "text", "text-file"],
    },
    DocumentedVerb {
        path: &["summary", "list"],
        flags: &[],
    },
    DocumentedVerb {
        path: &["summary", "show"],
        flags: &[],
    },
];

/// The six `kvx workflow node …` verbs whose wire method the removed engine
/// alone could serve (`app/api/workflows.rs`'s `node_verb_retired`). `node
/// show` is deliberately excluded — it still answers with real projected
/// data and is not part of this retirement.
const RETIRED_NODE_VERBS: &[&[&str]] = &[
    &["node", "steer"],
    &["node", "interrupt"],
    &["node", "restart"],
    &["node", "complete"],
    &["node", "expand"],
    &["node", "interrogate"],
];

/// `KARVEX_WORKFLOW_*` env vars the skill names, paired with the constant
/// that must produce their literal value.
struct DocumentedEnvVar {
    value: &'static str,
    source: &'static str,
}

const DOCUMENTED_ENV_VARS: &[DocumentedEnvVar] = &[DocumentedEnvVar {
    value: crate::workflow::binding::spawn::RUN_ID_ENV_VAR,
    source: "binding::spawn::RUN_ID_ENV_VAR",
}];

/// Walks `path` through the clap tree built by [`crate::cli::spec::command`],
/// starting at `kvx workflow`. Panics with the failing segment on drift —
/// this is a test-only module, so a panic is the whole point.
fn resolve<'a>(cmd: &'a Command, path: &[&str]) -> &'a Command {
    let mut current = cmd
        .get_subcommands()
        .find(|subcommand| subcommand.get_name() == "workflow")
        .expect("kvx workflow must exist in the clap tree");
    for segment in path {
        current = current
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == *segment)
            .unwrap_or_else(|| {
                panic!(
                    "skill text names `kvx workflow {}`, which does not resolve in the clap tree \
                     at segment {segment:?}",
                    path_words(path)
                )
            });
    }
    current
}

fn has_long_flag(cmd: &Command, flag: &str) -> bool {
    cmd.get_arguments().any(|arg| arg.get_long() == Some(flag))
}

/// `["run", "finish"]` -> `"run finish"`, matching how the skill's fenced
/// code blocks write a verb path.
fn path_words(path: &[&str]) -> String {
    path.join(" ")
}

/// Forward direction: every verb the skill names is a real verb, in both the
/// manual parser's own list and the clap tree, and every flag the skill uses
/// for it is a real `--flag` on that subcommand.
#[test]
fn every_documented_verb_and_flag_is_real() {
    let cmd = crate::cli::spec::command();
    for verb in DOCUMENTED_VERBS {
        assert!(
            crate::cli::workflow::VERB_PATHS.contains(&verb.path),
            "skill names `kvx workflow {}`, which is not in VERB_PATHS \
             (src/cli/workflow.rs) — it either never existed or was renamed",
            path_words(verb.path)
        );
        let resolved = resolve(&cmd, verb.path);
        for flag in verb.flags {
            assert!(
                has_long_flag(resolved, flag),
                "skill names `--{flag}` on `kvx workflow {}`, which has no such flag in \
                 src/cli/spec.rs",
                path_words(verb.path)
            );
        }
    }
}

/// The six retired `node` verbs are still real, still-parsing verb paths —
/// the skill teaches that the server refuses them, not that `kvx` itself
/// rejects the command line.
#[test]
fn retired_node_verbs_still_resolve_as_verb_paths() {
    let cmd = crate::cli::spec::command();
    for path in RETIRED_NODE_VERBS {
        assert!(
            crate::cli::workflow::VERB_PATHS.contains(path),
            "retired verb `kvx workflow {}` named in the skill is not in VERB_PATHS any \
             more — the skill's retirement teaching is stale",
            path_words(path)
        );
        resolve(&cmd, path);
    }
    // `node show` is the one node verb the retirement does NOT cover — pin
    // that it still resolves too, so the skill's "node show still works"
    // contrast stays true.
    resolve(&cmd, &["node", "show"]);
}

/// Backward direction: the verb and flag strings this module expects
/// `SKILL.md` to teach are actually present in it. Catches the prose-only
/// regression — deleting the paragraph that teaches a verb, without
/// deleting the verb from the CLI — that a forward-only check would miss
/// entirely.
#[test]
fn skill_text_names_every_documented_verb_and_flag() {
    for verb in DOCUMENTED_VERBS {
        let command_line = format!("kvx workflow {}", path_words(verb.path));
        assert!(
            SKILL_MD.contains(command_line.as_str()),
            "SKILL.md no longer mentions `{command_line}`, which this parity module expects \
             it to teach"
        );
        for flag in verb.flags {
            let needle = format!("--{flag}");
            assert!(
                SKILL_MD.contains(needle.as_str()),
                "SKILL.md no longer mentions `{needle}` (for `{command_line}`)"
            );
        }
    }
}

/// The skill teaches all six retired node verbs by name, and `node show` as
/// the one survivor.
#[test]
fn skill_text_names_every_retired_node_verb() {
    for path in RETIRED_NODE_VERBS {
        let verb: &str = path.last().copied().expect("retired path has a leaf verb");
        assert!(
            SKILL_MD.contains(verb),
            "SKILL.md no longer names the retired node verb `{verb}`"
        );
    }
    assert!(
        SKILL_MD.contains("node show"),
        "SKILL.md no longer contrasts the retired node verbs with `node show`, the one that \
         still works"
    );
}

/// Every `KARVEX_WORKFLOW_*` name the skill uses matches a real constant,
/// both ways.
#[test]
fn skill_text_env_vars_match_real_constants() {
    for env_var in DOCUMENTED_ENV_VARS {
        assert!(
            SKILL_MD.contains(env_var.value),
            "SKILL.md no longer mentions `{}` ({})",
            env_var.value,
            env_var.source
        );
    }
    // Backward: every `KARVEX_WORKFLOW_*`-shaped token actually present in
    // the skill text must be one of the constants this module knows about —
    // an env var invented in prose, or renamed in source without updating
    // the skill, fails here.
    let known: Vec<&str> = DOCUMENTED_ENV_VARS.iter().map(|e| e.value).collect();
    for token in SKILL_MD.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if token.starts_with("KARVEX_WORKFLOW_") {
            assert!(
                known.contains(&token),
                "SKILL.md names `{token}`, which is not a documented `KARVEX_WORKFLOW_*` \
                 constant in this parity module"
            );
        }
    }
}

/// The watchdog's `[karvex · watchdog]` frame is single-authored in
/// `watchdog.rs` (its module doc says so explicitly); the skill must quote
/// it exactly, not retype it.
#[test]
fn skill_text_quotes_the_real_watchdog_frame() {
    assert!(
        SKILL_MD.contains(crate::workflow::watchdog::WATCHDOG_FRAME),
        "SKILL.md does not quote the real WATCHDOG_FRAME from workflow::watchdog"
    );
}

/// Negative fixture (§5 P15 contract): a fabricated verb that does not exist
/// must fail the forward check. Proves `every_documented_verb_and_flag_is_real`
/// is not vacuous, and that renaming a verb fails this module exactly as
/// removing a mention of it would.
#[test]
#[should_panic(expected = "does not resolve in the clap tree")]
fn a_fabricated_verb_fails() {
    let cmd = crate::cli::spec::command();
    resolve(&cmd, &["run", "teleport"]);
}

/// Same negative fixture, for a flag that does not exist on a real verb.
#[test]
fn a_fabricated_flag_on_a_real_verb_is_absent() {
    let cmd = crate::cli::spec::command();
    let resolved = resolve(&cmd, &["run", "finish"]);
    assert!(
        !has_long_flag(resolved, "teleport-to"),
        "fixture flag unexpectedly exists — pick a different fabricated name"
    );
}
