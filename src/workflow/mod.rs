//! Workflow subsystem: kvdag definitions, run execution, and persistence.
//!
//! Layering follows `docs/design/workflow-builder/04-kvdag-and-execution.md`
//! §1. `model`, `tier`, `engine`, and `layout` are pure: no `App`, no PTYs, no
//! ratatui, no database, and unit-testable the way `AppState::test_new()` is.
//! `binding` is the only module that talks to the runtime, and `store` is the
//! only one that talks to the database.
//!
//! Every module here is compiled unconditionally. There is one karvex binary
//! per platform and the workflow subsystem is always in it.

// The module tree lands one step ahead of the code that consumes it, so items
// are unused until their workstream lands. Remove once the subsystem is fully
// wired into the server.
#![allow(dead_code)]

pub mod binding;
pub mod definition;
pub mod engine;
pub mod layout;
pub mod model;
pub mod store;
pub mod tier;
