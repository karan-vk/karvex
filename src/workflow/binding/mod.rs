//! Runtime binding: the only workflow module that may reference the runtime.
//!
//! `lead` builds the argv, env, and run directory for a run's team lead;
//! `spawn` holds the pane-geometry and run-directory primitives both the lead
//! spawn and the API's run-context writer share.

pub mod lead;
pub mod spawn;
