//! Runtime binding: the only workflow module that may reference the runtime.
//!
//! `lead` builds the argv, env, and run directory for a run's team lead;
//! `identity` turns the sessions' own `SessionStart` self-reports into a
//! binding; `messaging` encodes what karvex writes into those sessions'
//! documented inbox sockets; `spawn` holds the pane-geometry and run-directory
//! primitives both the lead spawn and the API's run-context writer share.

pub mod identity;
pub mod lead;
pub mod messaging;
pub mod spawn;
