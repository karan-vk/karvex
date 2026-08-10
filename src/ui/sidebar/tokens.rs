use ratatui::style::Color;

use super::AgentPanelEntry;
use crate::config::{
    AgentSidebarToken, AgentsSidebarConfig, SidebarTokenStyle, SpaceSidebarToken,
    SpacesSidebarConfig,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedToken {
    pub kind: ResolvedTokenKind,
    pub style: SidebarTokenStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResolvedTokenKind {
    StateIcon,
    StateText(String),
    Workspace(String),
    Tab(String),
    Pane(String),
    Agent(String),
    Session(String),
    TerminalTitle(String),
    Branch(String),
    GitStatus { ahead: usize, behind: usize },
    Custom(String),
}

impl ResolvedToken {
    fn new(kind: ResolvedTokenKind, style: SidebarTokenStyle) -> Self {
        Self { kind, style }
    }

    #[cfg(test)]
    pub(super) fn unstyled(kind: ResolvedTokenKind) -> Self {
        Self::new(kind, SidebarTokenStyle::default())
    }
}

pub(super) fn agent_rows(
    config: &AgentsSidebarConfig,
    entry: &AgentPanelEntry,
    state_text: &str,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows_for_agent(entry.agent)
        .iter()
        .filter_map(|row| {
            let resolved = row
                .iter()
                .filter_map(|configured| {
                    let (token, style) = configured.parts();
                    let kind = match token {
                        AgentSidebarToken::StateIcon => Some(ResolvedTokenKind::StateIcon),
                        AgentSidebarToken::StateText => {
                            Some(ResolvedTokenKind::StateText(state_text.to_string()))
                        }
                        AgentSidebarToken::Workspace => {
                            Some(ResolvedTokenKind::Workspace(entry.primary_label.clone()))
                        }
                        AgentSidebarToken::Tab => {
                            entry.primary_tab_label.clone().map(ResolvedTokenKind::Tab)
                        }
                        AgentSidebarToken::Pane => {
                            entry.pane_label.clone().map(ResolvedTokenKind::Pane)
                        }
                        AgentSidebarToken::Agent => {
                            entry.agent_label.clone().map(ResolvedTokenKind::Agent)
                        }
                        // Panes without an agent session resolve to nothing, so
                        // the element is simply absent from the row rather than
                        // taking up width with a placeholder.
                        AgentSidebarToken::Session => entry
                            .agent_session_name
                            .clone()
                            .map(ResolvedTokenKind::Session),
                        AgentSidebarToken::TerminalTitle => entry
                            .terminal_title
                            .clone()
                            .map(ResolvedTokenKind::TerminalTitle),
                        AgentSidebarToken::TerminalTitleStripped => entry
                            .terminal_title_stripped
                            .clone()
                            .map(ResolvedTokenKind::TerminalTitle),
                        AgentSidebarToken::Custom(name) => entry
                            .tokens
                            .get(name)
                            .cloned()
                            .map(ResolvedTokenKind::Custom),
                        AgentSidebarToken::Styled { .. } => None,
                    }?;
                    Some(ResolvedToken::new(kind, style))
                })
                .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

pub(super) struct SpaceTokenContext<'a> {
    pub workspace: &'a str,
    pub branch: Option<&'a str>,
    pub state_text: &'a str,
    pub ahead_behind: Option<(usize, usize)>,
    pub tokens: &'a std::collections::HashMap<String, String>,
    pub suppress_git_details: bool,
}

pub(super) fn space_rows(
    config: &SpacesSidebarConfig,
    context: SpaceTokenContext<'_>,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows
        .iter()
        .filter_map(|row| {
            let resolved = row
                .iter()
                .filter_map(|configured| {
                    let (token, style) = configured.parts();
                    let kind = match token {
                        SpaceSidebarToken::StateIcon => Some(ResolvedTokenKind::StateIcon),
                        SpaceSidebarToken::StateText => {
                            Some(ResolvedTokenKind::StateText(context.state_text.to_string()))
                        }
                        SpaceSidebarToken::Workspace => {
                            Some(ResolvedTokenKind::Workspace(context.workspace.to_string()))
                        }
                        SpaceSidebarToken::Branch if !context.suppress_git_details => context
                            .branch
                            .map(|branch| ResolvedTokenKind::Branch(branch.to_string())),
                        SpaceSidebarToken::Branch => None,
                        SpaceSidebarToken::GitStatus if !context.suppress_git_details => context
                            .ahead_behind
                            .filter(|(ahead, behind)| *ahead > 0 || *behind > 0)
                            .map(|(ahead, behind)| ResolvedTokenKind::GitStatus { ahead, behind }),
                        SpaceSidebarToken::GitStatus => None,
                        SpaceSidebarToken::Custom(name) => context
                            .tokens
                            .get(name)
                            .cloned()
                            .map(ResolvedTokenKind::Custom),
                        SpaceSidebarToken::Styled { .. } => None,
                    }?;
                    Some(ResolvedToken::new(kind, style))
                })
                .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

pub(super) fn separator(previous: &ResolvedToken, current: &ResolvedToken) -> &'static str {
    if matches!(previous.kind, ResolvedTokenKind::StateIcon)
        || matches!(current.kind, ResolvedTokenKind::GitStatus { .. })
    {
        " "
    } else {
        " · "
    }
}

/// Teammate accent colour (D8): a style hint derived from the
/// `agent_accent` metadata token the tmux-compat shim writes
/// (`src/cli/tmux_compat.rs`), never rendered as literal text. An unset or
/// unrecognised value falls back to `None`, so the caller keeps the row's
/// default style rather than propagating garbage into the UI.
///
/// These are fixed identity swatches, deliberately independent of the active
/// theme's `Palette`: the accent's job is to let a user tell teammates apart
/// by the colour Claude itself assigned them, not to carry status meaning
/// (unlike `Palette::red`/`green`/`yellow`, which already mean
/// blocked/idle/working elsewhere in this UI).
pub(super) fn agent_accent_color(value: &str) -> Option<Color> {
    Some(match value {
        "red" => Color::Rgb(0xe0, 0x6c, 0x75),
        "blue" => Color::Rgb(0x61, 0xaf, 0xef),
        "green" => Color::Rgb(0x98, 0xc3, 0x79),
        "yellow" => Color::Rgb(0xe5, 0xc0, 0x7b),
        "magenta" => Color::Rgb(0xc6, 0x78, 0xdd),
        "cyan" => Color::Rgb(0x56, 0xb6, 0xc2),
        // colour208
        "orange" => Color::Rgb(0xd1, 0x9a, 0x66),
        // colour205
        "pink" => Color::Rgb(0xff, 0x79, 0xc6),
        _ => return None,
    })
}

/// Reads the `agent_accent` token straight off the panel entry and resolves
/// it to a colour. See [`agent_accent_color`].
pub(super) fn entry_agent_accent_color(entry: &AgentPanelEntry) -> Option<Color> {
    entry
        .tokens
        .get(crate::cli::tmux_compat::AGENT_ACCENT_TOKEN_KEY)
        .and_then(|value| agent_accent_color(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentSidebarToken, SpaceSidebarToken};
    use crate::detect::AgentState;

    #[test]
    fn agent_accent_color_maps_every_claude_colour_and_falls_back_for_unknown() {
        for colour in ["red", "blue", "green", "yellow", "magenta", "cyan"] {
            assert!(agent_accent_color(colour).is_some(), "missing {colour}");
        }
        // Post-normalisation names (colour208 -> orange, colour205 -> pink);
        // the shim never hands the sidebar a raw `colourNNN` value.
        assert!(agent_accent_color("orange").is_some());
        assert!(agent_accent_color("pink").is_some());
        assert_eq!(agent_accent_color("colour208"), None);
        assert_eq!(agent_accent_color(""), None);
        assert_eq!(agent_accent_color("mauve"), None);
    }

    #[test]
    fn entry_agent_accent_color_reads_the_agent_accent_token() {
        let mut with_accent = entry();
        with_accent
            .tokens
            .insert("agent_accent".into(), "cyan".into());
        assert_eq!(
            entry_agent_accent_color(&with_accent),
            agent_accent_color("cyan")
        );

        let without_accent = entry();
        assert_eq!(entry_agent_accent_color(&without_accent), None);

        let mut unknown_value = entry();
        unknown_value
            .tokens
            .insert("agent_accent".into(), "chartreuse".into());
        assert_eq!(entry_agent_accent_color(&unknown_value), None);
    }

    fn entry() -> AgentPanelEntry {
        AgentPanelEntry {
            ws_idx: 0,
            tab_idx: 0,
            pane_id: crate::layout::PaneId::from_raw(1),
            primary_label: "repo".into(),
            primary_tab_label: None,
            pane_label: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_label: Some("pi".into()),
            agent_kind_label: Some("pi".into()),
            agent_session_name: None,
            agent: Some(crate::detect::Agent::Pi),
            state: AgentState::Working,
            seen: true,
            last_agent_state_change_seq: None,
            state_labels: std::collections::HashMap::new(),
            tokens: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn missing_custom_tokens_elide_rows_and_separators() {
        let entry = entry();
        let config = AgentsSidebarConfig {
            rows: vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Custom("missing".into()),
                ],
                vec![AgentSidebarToken::Custom("missing".into())],
                vec![AgentSidebarToken::Agent],
            ],
            ..Default::default()
        };

        let rows = agent_rows(&config, &entry, "working");

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::StateIcon)]
        );
        assert_eq!(
            rows[1],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "pi".into()
            ))]
        );
    }

    #[test]
    fn state_text_and_arbitrary_values_are_independent_tokens() {
        let mut entry = entry();
        entry
            .tokens
            .insert("summary".into(), "reviewing auth".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::StateText,
                AgentSidebarToken::Custom("summary".into()),
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "deep in the mines"),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::StateText("deep in the mines".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Custom("reviewing auth".into())),
            ]]
        );
    }

    #[test]
    fn terminal_title_builtins_are_distinct_from_custom_tokens() {
        let mut entry = entry();
        entry.terminal_title = Some("⠋ raw title".into());
        entry.terminal_title_stripped = Some("raw title".into());
        entry
            .tokens
            .insert("terminal_title".into(), "custom title".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::TerminalTitle,
                AgentSidebarToken::TerminalTitleStripped,
                AgentSidebarToken::Custom("terminal_title".into()),
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "working"),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("⠋ raw title".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("raw title".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Custom("custom title".into())),
            ]]
        );
    }

    #[test]
    fn the_session_token_renders_whatever_the_entry_resolved() {
        let mut entry = entry();
        entry.agent_session_name = Some("toilet-presence-sensor".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![AgentSidebarToken::Agent, AgentSidebarToken::Session]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "working"),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::Agent("pi".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Session(
                    "toilet-presence-sensor".into()
                )),
            ]]
        );
    }

    #[test]
    fn a_short_id_fallback_reaches_the_session_token_unchanged() {
        // The aggregate layer already decided name-or-short-id; the token layer
        // only renders what it was handed.
        let mut entry = entry();
        entry.agent_session_name = Some("f593fc46".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![AgentSidebarToken::Session]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &entry, "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Session(
                "f593fc46".into()
            ))]]
        );
    }

    #[test]
    fn a_pane_without_a_session_drops_the_token_and_can_empty_the_row() {
        let entry = entry();
        assert_eq!(entry.agent_session_name, None);
        let config = AgentsSidebarConfig {
            rows: vec![
                vec![AgentSidebarToken::Agent, AgentSidebarToken::Session],
                vec![AgentSidebarToken::Session],
            ],
            ..Default::default()
        };

        // Row 1 keeps just the agent; row 2 disappears entirely rather than
        // rendering an empty line.
        assert_eq!(
            agent_rows(&config, &entry, "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "pi".into()
            ))]]
        );
    }

    #[test]
    fn default_rows_pair_the_agent_with_its_session() {
        let mut entry = entry();
        entry.agent_session_name = Some("sensor-pcb".into());

        let rows = agent_rows(&AgentsSidebarConfig::default(), &entry, "working");

        assert_eq!(
            rows.last(),
            Some(&vec![
                ResolvedToken::unstyled(ResolvedTokenKind::Agent("pi".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Session("sensor-pcb".into())),
            ])
        );
    }

    #[test]
    fn known_agent_override_replaces_default_rows() {
        let mut config = AgentsSidebarConfig {
            rows: vec![vec![AgentSidebarToken::Workspace]],
            ..Default::default()
        };
        config
            .rows_by_agent
            .insert("pi".into(), vec![vec![AgentSidebarToken::Agent]]);
        let mut pi = entry();
        pi.agent_label = Some("renamed pi".into());

        assert_eq!(
            agent_rows(&config, &pi, "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "renamed pi".into()
            ))]]
        );

        pi.agent = None;
        assert_eq!(
            agent_rows(&config, &pi, "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Workspace(
                "repo".into()
            ))]]
        );
    }

    #[test]
    fn grouped_children_suppress_all_builtin_git_details() {
        let config = SpacesSidebarConfig::default();

        assert_eq!(
            space_rows(
                &config,
                SpaceTokenContext {
                    workspace: "feature",
                    branch: Some("worktree/feature"),
                    state_text: "idle",
                    ahead_behind: Some((2, 1)),
                    tokens: &std::collections::HashMap::new(),
                    suppress_git_details: true,
                },
            ),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::StateIcon),
                ResolvedToken::unstyled(ResolvedTokenKind::Workspace("feature".into())),
            ]]
        );
    }

    #[test]
    fn workspace_custom_token_can_replace_git_specific_details() {
        let tokens = std::collections::HashMap::from([("jj_status".into(), "2 changes".into())]);
        let config = SpacesSidebarConfig {
            rows: vec![vec![SpaceSidebarToken::Custom("jj_status".into())]],
            ..Default::default()
        };

        assert_eq!(
            space_rows(
                &config,
                SpaceTokenContext {
                    workspace: "repo",
                    branch: None,
                    state_text: "idle",
                    ahead_behind: None,
                    tokens: &tokens,
                    suppress_git_details: false,
                },
            ),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Custom(
                "2 changes".into()
            ))]]
        );
    }
}
