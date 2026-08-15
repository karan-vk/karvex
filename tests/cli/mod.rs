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
mod workflow;
mod workspace;
