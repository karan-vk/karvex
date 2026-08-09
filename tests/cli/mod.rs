mod agent_options;
mod agent_transport;
mod agent_wait;
mod agents;
mod harness;
mod hooks;
mod panes;
mod plugins;
mod protocol;
mod protocol_guard;
mod sessions;
mod surface;
// The tmux-compat shim (`src/cli/tmux_compat.rs`, `src/platform/tmux_shim.rs`)
// is unix-only (docs/design/claude-teammates/01-port-plan.md D10); Windows
// exports nothing and has no `tmux` symlink to build this suite around.
#[cfg(unix)]
mod tmux_compat;
// The workflow subsystem — and with it `workflow.*` handlers — is gated
// behind the `workflow` feature (on by default; off for the MSVC cross-lint
// leg and slim source builds, `Cargo.toml`). A server built without it
// answers every `workflow.*` request with `workflow_unavailable`, so this
// suite would only ever exercise its own failure path there.
#[cfg(feature = "workflow")]
mod workflow;
mod workspace;
