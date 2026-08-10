//! Runtime binding: `RunEffect` → `App` calls, and `App` facts →
//! `EngineInput`.
//!
//! This is the only workflow module that may reference the runtime. Its parts
//! are split so spawn work and observation work stay independent: `spawn`
//! builds argv/env and node directories and creates panes, `observe` turns
//! detection, hook, and pane-exit facts back into engine inputs, and
//! `interrogate` revives a *finished* node's session in a pane — which is
//! deliberately not `spawn`'s job, because an interrogation is not a run node
//! (`07-phase3-plan.md` §4 D8).
//!
//! This file is landed complete, with both `mod` lines and stub files, so
//! parallel work never has to edit it.

pub mod interrogate;
pub mod observe;
pub mod spawn;
