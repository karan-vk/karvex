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
// `mod workflow` used to sit here. Every one of its six cases drove a
// `runner = "command"` node calling `kvx workflow node complete`/`node
// expand` on itself — the node contract, which went with the engine
// (`09-agent-teams-rework.md` §2). The surviving halves are covered
// elsewhere: the CLI's renderers and argument parsing by the 97 unit tests in
// `src/cli/workflow.rs`, and the real binary against a real server by
// `tests/workflow_lead_headless.rs`, which drives `kvx workflow run finish`.
mod workspace;
