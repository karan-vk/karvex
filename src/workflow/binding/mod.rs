//! Runtime binding: `RunEffect` → `App` calls, and `App` facts →
//! `EngineInput`.
//!
//! This is the only workflow module that may reference the runtime. Its two
//! halves are split so spawn work and observation work stay independent:
//! `spawn` builds argv/env and node directories and creates panes, `observe`
//! turns detection, hook, and pane-exit facts back into engine inputs.
//!
//! This file is landed complete, with both `mod` lines and stub files, so
//! parallel work never has to edit it.

pub mod observe;
pub mod spawn;
