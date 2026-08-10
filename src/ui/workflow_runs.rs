//! The run browser overlay — a list-and-detail view over past and pruned
//! runs (`docs/design/workflow-builder/07-phase3-plan.md` §WS-F).
//!
//! Landed as an inert stub in Phase 3 step 1b (§WS-G) so `ui.rs` never needs
//! to be touched again: it already dispatches into
//! [`compute_workflow_runs_view`] and [`render_workflow_runs`]. Split the same
//! way the launcher and DAG overlay are: the compute pass stores every
//! rectangle the renderer will draw, and the renderer only reads them back —
//! the hit-test can never disagree with what was drawn.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::text::truncate_end;
use super::widgets::{centered_popup_rect, modal_stack_areas, render_panel_shell};
use crate::app::state::{
    AppState, Mode, Palette, WorkflowRunsConfirmRestore, WorkflowRunsEntry,
    WorkflowRunsPrunedEntry, WorkflowRunsRunEntry, WorkflowRunsState,
};
use crate::workflow::model::RunStatus;

pub(crate) const RUNS_MODAL_WIDTH: u16 = 96;
pub(crate) const RUNS_MODAL_HEIGHT: u16 = 26;

const HEADER_HEIGHT: u16 = 1;
const FOOTER_HEIGHT: u16 = 1;
const STACK_GAP: u16 = 1;
/// Detail strip rows, within the plan's 4-6 row band.
const DETAIL_HEIGHT: u16 = 5;

// ── view computation (mutation pass) ────────────────────────────────────────

/// Refreshes the run browser's geometry for this frame. `carried` is the
/// previous frame's state (entries, selection, scroll, the restore-confirm
/// sub-state); `ViewState` is rebuilt wholesale every pass, so it is carried
/// rather than recomputed — same rule as
/// [`super::workflow_launch::compute_workflow_launch_view`].
///
/// Drops stale geometry the moment the mode goes inactive, exactly like the
/// launcher: a closed overlay must never leave clickable rects behind for
/// whatever mode comes next.
pub(super) fn compute_workflow_runs_view(
    app: &AppState,
    area: Rect,
    carried: WorkflowRunsState,
) -> WorkflowRunsState {
    if app.mode != Mode::WorkflowRuns {
        return WorkflowRunsState::default();
    }
    let mut view = carried;
    apply_runs_geometry(&mut view, area);
    view
}

/// Stores the rects the renderer draws and the hit-test reads. Pure: the only
/// inputs are the area and the state already on the view.
pub(crate) fn apply_runs_geometry(view: &mut WorkflowRunsState, area: Rect) {
    view.modal_rect = Rect::default();
    view.list_rect = Rect::default();
    view.detail_rect = Rect::default();
    view.footer_rect = Rect::default();
    view.row_rects = vec![Rect::default(); view.entries.len()];

    let Some(popup) = centered_popup_rect(area, RUNS_MODAL_WIDTH, RUNS_MODAL_HEIGHT) else {
        return;
    };
    view.modal_rect = popup;
    let Some(sections) = runs_sections(popup, view.entries.len(), view.selected) else {
        return;
    };
    view.list_rect = sections.list;
    view.detail_rect = sections.detail;
    view.footer_rect = sections.footer;
    view.row_rects = sections.rows;
}

/// Every rectangle the modal is made of.
pub(crate) struct RunsSections {
    pub header: Rect,
    pub list: Rect,
    /// One rect per entry. Rows outside the visible window get an empty
    /// rect, so indices stay aligned with `entries` without a scroll offset
    /// on the state.
    pub rows: Vec<Rect>,
    pub detail: Rect,
    pub footer: Rect,
}

/// Splits the modal into its sections. `popup` is the bordered outer rect, so
/// this is the one place that knows the border costs a row and a column on
/// each side.
///
/// A short modal yields in this order: the detail strip shrinks first (down
/// to nothing), then the list — a list you cannot read is worse than a list
/// with no detail under it.
pub(crate) fn runs_sections(
    popup: Rect,
    entry_count: usize,
    selected: usize,
) -> Option<RunsSections> {
    if popup.width < 4 || popup.height < 6 {
        return None;
    }
    let inner = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let stack = modal_stack_areas(inner, HEADER_HEIGHT, FOOTER_HEIGHT, 0, STACK_GAP);
    let content = stack.content;
    if content.height == 0 {
        return None;
    }

    let detail_height = DETAIL_HEIGHT.min(content.height.saturating_sub(1));
    let list = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(detail_height),
    );
    let detail = Rect::new(content.x, list.bottom(), content.width, detail_height);

    let rows = windowed_rows(
        list,
        entry_count,
        list_window_offset(entry_count, selected, list.height),
    );

    Some(RunsSections {
        header: stack.header,
        list,
        rows,
        detail,
        footer: stack.footer.unwrap_or_default(),
    })
}

/// Scrolls the list just enough to keep the selection on screen. Derived from
/// the selection rather than stored, so there is no scroll offset that can
/// drift out of step with it.
fn list_window_offset(count: usize, selected: usize, height: u16) -> usize {
    let height = height as usize;
    if height == 0 || count == 0 {
        return 0;
    }
    let selected = selected.min(count.saturating_sub(1));
    if selected >= height {
        selected + 1 - height
    } else {
        0
    }
}

/// One rect per entry, empty for the entries this frame has no row for. An
/// empty rect can never be hit, so nothing invisible stays clickable.
fn windowed_rows(area: Rect, count: usize, offset: usize) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); count];
    if area.height == 0 {
        return rects;
    }
    for row in 0..area.height {
        let Some(index) = offset.checked_add(row as usize) else {
            break;
        };
        let Some(slot) = rects.get_mut(index) else {
            break;
        };
        *slot = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
    }
    rects
}

// ── status vocabulary ───────────────────────────────────────────────────────
//
// Mirrors `ui::workflow_dag`'s run-status glyph/colour choices (private
// there, so this is a deliberate small copy rather than a cross-module
// dependency on another workstream's frozen file).

fn run_status_glyph(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "·",
        RunStatus::Running => "●",
        RunStatus::Paused => "!",
        RunStatus::Succeeded => "✓",
        RunStatus::Failed => "✗",
        RunStatus::Cancelled => "×",
    }
}

fn run_status_color(status: RunStatus, p: &Palette) -> Color {
    match status {
        RunStatus::Running => p.yellow,
        RunStatus::Succeeded => p.green,
        RunStatus::Failed => p.red,
        RunStatus::Paused => p.peach,
        RunStatus::Pending => p.subtext0,
        RunStatus::Cancelled => p.overlay0,
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// A compact relative timestamp for a narrow list row — "3h ago" reads better
/// there than a full calendar date.
fn relative_time(now_unix_ms: u64, then_unix_ms: u64) -> String {
    let elapsed_secs = now_unix_ms.saturating_sub(then_unix_ms) / 1000;
    if elapsed_secs < 60 {
        "just now".to_string()
    } else if elapsed_secs < 3_600 {
        format!("{}m ago", elapsed_secs / 60)
    } else if elapsed_secs < 86_400 {
        format!("{}h ago", elapsed_secs / 3_600)
    } else {
        format!("{}d ago", elapsed_secs / 86_400)
    }
}

// ── rendering (draw only) ───────────────────────────────────────────────────

pub(super) fn render_workflow_runs(app: &AppState, frame: &mut Frame, area: Rect) {
    let runs = &app.view.workflow_runs;
    let popup = runs.modal_rect;
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    let Some(sections) = runs_sections(popup, runs.entries.len(), runs.selected) else {
        return;
    };

    let p = &app.palette;
    super::dim_background(frame, area);
    if render_panel_shell(frame, popup, p.accent, p.panel_bg).is_none() {
        return;
    }

    frame.render_widget(
        Paragraph::new(Span::styled(
            " run history",
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        sections.header,
    );

    render_rows(runs, frame, &sections, p);
    render_detail(runs, frame, sections.detail, p);
    render_footer(runs, frame, sections.footer, p);

    if let Some(confirm) = &runs.confirm_restore {
        render_restore_confirm(confirm, frame, popup, p);
    }
}

fn render_rows(runs: &WorkflowRunsState, frame: &mut Frame, sections: &RunsSections, p: &Palette) {
    if runs.entries.is_empty() {
        if sections.list.height > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " no runs yet — start one from the workflow launcher",
                    Style::default().fg(p.overlay1),
                )),
                Rect::new(sections.list.x, sections.list.y, sections.list.width, 1),
            );
        }
        return;
    }

    let now = current_unix_ms();
    for (index, (entry, rect)) in runs.entries.iter().zip(sections.rows.iter()).enumerate() {
        if rect.height == 0 {
            continue;
        }
        let selected = index == runs.selected;
        frame.render_widget(
            Paragraph::new(row_line(entry, rect.width, selected, now, p)),
            *rect,
        );
    }
}

fn row_line(
    entry: &WorkflowRunsEntry,
    width: u16,
    selected: bool,
    now_unix_ms: u64,
    p: &Palette,
) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };

    let text = match entry {
        WorkflowRunsEntry::Run(run) => {
            let glyph = run_status_glyph(run.status);
            let tier = run
                .tier
                .map(|tier| tier.as_str().to_string())
                .unwrap_or_else(|| "-".to_string());
            let summary = if run.summary_outcome.is_some() {
                "summary"
            } else {
                ""
            };
            format!(
                "{marker}{glyph} {} · {tier} · {} · {}/{} {summary}",
                run.workflow_name,
                relative_time(now_unix_ms, run.started_at_unix_ms),
                run.nodes_done,
                run.nodes_total,
            )
        }
        WorkflowRunsEntry::PrunedSummary(pruned) => {
            format!(
                "{marker}· {} · pruned · {}",
                pruned.workflow_name, pruned.summary_outcome
            )
        }
    };
    let text = truncate_end(&text, width as usize);
    let fg = if selected {
        p.text
    } else {
        match entry {
            WorkflowRunsEntry::Run(run) => run_status_color(run.status, p),
            WorkflowRunsEntry::PrunedSummary(_) => p.overlay0,
        }
    };
    let mut style = Style::default().fg(fg);
    if selected {
        style = style.bg(p.surface0);
    }
    Line::from(vec![Span::styled(text, style)])
}

fn render_detail(runs: &WorkflowRunsState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let Some(entry) = runs.entries.get(runs.selected) else {
        return;
    };
    let lines: Vec<Line<'static>> = match entry {
        WorkflowRunsEntry::Run(run) => run_detail_lines(run, p),
        WorkflowRunsEntry::PrunedSummary(pruned) => pruned_detail_lines(pruned, p),
    };
    for (index, line) in lines.into_iter().take(area.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y.saturating_add(index as u16), area.width, 1),
        );
    }
}

fn run_detail_lines(run: &WorkflowRunsRunEntry, p: &Palette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(p.overlay1);
    let text = Style::default().fg(p.text);
    let mut lines = Vec::new();
    let args = if run.args.is_empty() {
        "(none)".to_string()
    } else {
        run.args.join(", ")
    };
    lines.push(Line::from(vec![
        Span::styled(" args: ", dim),
        Span::styled(args, text),
    ]));
    if !run.limits.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" limits: ", dim),
            Span::styled(run.limits.clone(), text),
        ]));
    }
    if let Some(blocker) = &run.blocker {
        lines.push(Line::from(vec![Span::styled(
            format!(" {blocker}"),
            Style::default().fg(p.red),
        )]));
    }
    // A lead run says who ran it (§3.4). Gated on `team_name` rather than on a
    // non-empty member list: an engine-era run has neither, and its detail
    // strip must stay byte-identical to what it was before the rework.
    if let Some(team) = &run.team_name {
        lines.push(Line::from(vec![
            Span::styled(" team: ", dim),
            Span::styled(team.clone(), text),
        ]));
        // "not observed yet" rather than nothing: karvex learns the members
        // from the team config the lead writes as it starts, so an empty list
        // on a live run means "too early", not "nobody worked on this".
        let members = if run.members.is_empty() {
            "(not observed yet)".to_string()
        } else {
            run.members.join(", ")
        };
        lines.push(Line::from(vec![
            Span::styled(" members: ", dim),
            Span::styled(members, text),
        ]));
    }
    match (&run.summary_outcome, &run.summary_first_highlight) {
        (Some(outcome), Some(highlight)) => lines.push(Line::from(vec![
            Span::styled(" summary: ", dim),
            Span::styled(format!("{outcome} — {highlight}"), text),
        ])),
        (Some(outcome), None) => lines.push(Line::from(vec![
            Span::styled(" summary: ", dim),
            Span::styled(outcome.clone(), text),
        ])),
        (None, _) => lines.push(Line::from(Span::styled(" no summary written", dim))),
    }
    lines
}

fn pruned_detail_lines(pruned: &WorkflowRunsPrunedEntry, p: &Palette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(p.overlay1);
    let text = Style::default().fg(p.text);
    vec![
        Line::from(vec![
            Span::styled(" summary: ", dim),
            Span::styled(
                format!("{} — {}", pruned.summary_outcome, pruned.summary_text),
                text,
            ),
        ]),
        Line::from(Span::styled(
            " history pruned — restore and interrogation unavailable",
            Style::default().fg(p.peach),
        )),
    ]
}

fn render_footer(runs: &WorkflowRunsState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let text = if runs.entries.is_empty() {
        " esc closes"
    } else {
        " ↵ open · r restore · esc close"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(p.overlay1))),
        area,
    );
}

fn render_restore_confirm(
    confirm: &WorkflowRunsConfirmRestore,
    frame: &mut Frame,
    parent: Rect,
    p: &Palette,
) {
    let Some(popup) = centered_popup_rect(parent, 60.min(parent.width), 6.min(parent.height))
    else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, p.peach, p.panel_bg) else {
        return;
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" restore all of {}?", confirm.workflow_name),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " every restorable node starts a new run without re-executing",
            Style::default().fg(p.overlay1),
        )),
        Line::from(Span::styled(
            " ↵ confirm · esc cancel",
            Style::default().fg(p.overlay1),
        )),
    ];
    for (index, line) in lines.into_iter().enumerate() {
        if (index as u16) >= inner.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(
                inner.x,
                inner.y.saturating_add(index as u16),
                inner.width,
                1,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tier::Tier;

    fn run_entry(id: &str, status: RunStatus) -> WorkflowRunsEntry {
        WorkflowRunsEntry::Run(WorkflowRunsRunEntry {
            run_id: id.to_string(),
            workflow_id: "workflow:1".to_string(),
            workflow_name: "ship-it".to_string(),
            tier: Some(Tier::High),
            status,
            started_at_unix_ms: 1_000,
            nodes_done: 2,
            nodes_total: 3,
            args: vec!["goal=ship".to_string()],
            limits: String::new(),
            blocker: None,
            summary_outcome: None,
            summary_first_highlight: None,
            team_name: None,
            members: Vec::new(),
        })
    }

    /// The same row, executed by a Claude Code team lead
    /// (`09-agent-teams-rework.md` §3.4).
    fn lead_entry(members: Vec<&str>) -> WorkflowRunsRunEntry {
        let WorkflowRunsEntry::Run(mut run) = run_entry("run:lead", RunStatus::Running) else {
            panic!("expected a run row");
        };
        run.team_name = Some("session-213aa9bf".to_string());
        run.members = members.into_iter().map(str::to_string).collect();
        run
    }

    /// §3.4: a lead run's detail names the team and who was on it; an
    /// engine-era run's detail is byte-identical to what it always was.
    #[test]
    fn a_lead_runs_detail_names_its_team_and_members() {
        let p = Palette::catppuccin();
        let lead = lead_entry(vec![
            "research · sonnet · w1:p3",
            "team-lead · opus · in-process",
        ]);
        let text = detail_text(&run_detail_lines(&lead, &p));
        assert!(text.contains("team: session-213aa9bf"), "{text}");
        assert!(text.contains("research · sonnet · w1:p3"), "{text}");
        assert!(text.contains("team-lead · opus · in-process"), "{text}");

        // A live lead whose team config has not been read yet says so rather
        // than implying nobody worked on the run.
        let early = detail_text(&run_detail_lines(&lead_entry(Vec::new()), &p));
        assert!(early.contains("members: (not observed yet)"), "{early}");

        let WorkflowRunsEntry::Run(engine) = run_entry("run:1", RunStatus::Succeeded) else {
            panic!("expected a run row");
        };
        let engine_text = detail_text(&run_detail_lines(&engine, &p));
        assert!(!engine_text.contains("team"), "{engine_text}");
        assert!(!engine_text.contains("members"), "{engine_text}");
    }

    fn detail_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pruned_entry(id: &str) -> WorkflowRunsEntry {
        WorkflowRunsEntry::PrunedSummary(WorkflowRunsPrunedEntry {
            run_id: id.to_string(),
            workflow_id: "workflow:1".to_string(),
            workflow_name: "ship-it".to_string(),
            summary_outcome: "succeeded".to_string(),
            summary_text: "shipped the thing".to_string(),
        })
    }

    fn state_with(entries: Vec<WorkflowRunsEntry>) -> WorkflowRunsState {
        WorkflowRunsState {
            entries,
            ..WorkflowRunsState::default()
        }
    }

    #[test]
    fn geometry_gives_one_rect_per_entry() {
        let mut view = state_with(vec![
            run_entry("run:1", RunStatus::Succeeded),
            run_entry("run:2", RunStatus::Running),
            pruned_entry("run:3"),
        ]);
        apply_runs_geometry(&mut view, Rect::new(0, 0, 120, 40));

        assert_eq!(view.row_rects.len(), 3);
        assert!(view.modal_rect.width >= 4 && view.modal_rect.height >= 6);
        assert!(view.detail_rect.height > 0);
        for rect in &view.row_rects {
            assert!(rect.y >= view.modal_rect.y, "{rect:?}");
            assert!(rect.bottom() <= view.modal_rect.bottom(), "{rect:?}");
        }
    }

    #[test]
    fn a_tiny_area_stores_no_clickable_geometry() {
        let mut view = state_with(vec![run_entry("run:1", RunStatus::Succeeded)]);
        apply_runs_geometry(&mut view, Rect::new(0, 0, 8, 4));

        assert_eq!(view.modal_rect, Rect::default());
        assert!(view.row_rects.iter().all(|rect| rect.height == 0));
    }

    #[test]
    fn a_selection_below_the_window_scrolls_the_list_without_a_stored_offset() {
        let entries: Vec<WorkflowRunsEntry> = (0..40)
            .map(|index| run_entry(&format!("run:{index}"), RunStatus::Succeeded))
            .collect();
        let mut view = state_with(entries);
        view.selected = 39;
        apply_runs_geometry(&mut view, Rect::new(0, 0, 120, 40));

        let visible: Vec<usize> = view
            .row_rects
            .iter()
            .enumerate()
            .filter(|(_, rect)| rect.height > 0)
            .map(|(index, _)| index)
            .collect();
        assert!(!visible.is_empty());
        assert_eq!(
            visible.last().copied(),
            Some(39),
            "the selected row is always on screen"
        );
    }

    #[test]
    fn the_renderer_and_the_compute_pass_read_the_same_sections() {
        let mut view = state_with(vec![run_entry("run:1", RunStatus::Succeeded)]);
        apply_runs_geometry(&mut view, Rect::new(0, 0, 120, 40));

        let sections = runs_sections(view.modal_rect, view.entries.len(), view.selected)
            .expect("the modal fits");
        assert_eq!(sections.rows, view.row_rects);
        assert_eq!(sections.list, view.list_rect);
        assert_eq!(sections.detail, view.detail_rect);
    }

    fn screen_of(runs: &WorkflowRunsState, area: Rect) -> String {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowRuns;
        app.view.workflow_runs = runs.clone();
        let app = &app;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_runs(app, frame, area))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_draws_the_list_and_the_detail_strip() {
        let area = Rect::new(0, 0, 110, 30);
        let mut run = run_entry("run:1", RunStatus::Succeeded);
        if let WorkflowRunsEntry::Run(entry) = &mut run {
            entry.summary_outcome = Some("shipped".to_string());
            entry.summary_first_highlight = Some("added dark mode".to_string());
        }
        let mut view = state_with(vec![run]);
        apply_runs_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("run history"), "{screen}");
        assert!(screen.contains("ship-it"), "{screen}");
        assert!(screen.contains("goal=ship"), "{screen}");
        assert!(screen.contains("shipped"), "{screen}");
        assert!(screen.contains("added dark mode"), "{screen}");
    }

    #[test]
    fn a_pruned_row_shows_the_fixed_unavailable_line() {
        let area = Rect::new(0, 0, 110, 30);
        let mut view = state_with(vec![pruned_entry("run:1")]);
        apply_runs_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("pruned"), "{screen}");
        assert!(
            screen.contains("restore and interrogation unavailable"),
            "{screen}"
        );
    }

    #[test]
    fn an_empty_list_says_so_rather_than_drawing_nothing() {
        let area = Rect::new(0, 0, 110, 30);
        let mut view = state_with(Vec::new());
        apply_runs_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("no runs yet"), "{screen}");
    }

    #[test]
    fn the_restore_confirm_dialog_draws_over_the_list() {
        let area = Rect::new(0, 0, 110, 30);
        let mut view = state_with(vec![run_entry("run:1", RunStatus::Succeeded)]);
        apply_runs_geometry(&mut view, area);
        view.confirm_restore = Some(WorkflowRunsConfirmRestore {
            run_id: "run:1".to_string(),
            workflow_name: "ship-it".to_string(),
        });

        let screen = screen_of(&view, area);
        assert!(screen.contains("restore all of ship-it"), "{screen}");
    }

    #[test]
    fn a_terminal_too_small_for_the_modal_draws_nothing_and_does_not_panic() {
        for (width, height) in [(4, 3), (10, 5), (20, 8), (40, 10)] {
            let area = Rect::new(0, 0, width, height);
            let mut view = state_with(vec![run_entry("run:1", RunStatus::Succeeded)]);
            apply_runs_geometry(&mut view, area);
            let _ = screen_of(&view, area);
        }
    }

    #[test]
    fn closing_the_modal_drops_the_geometry() {
        let mut app = AppState::test_new();
        let mut carried = state_with(vec![run_entry("run:1", RunStatus::Succeeded)]);
        apply_runs_geometry(&mut carried, Rect::new(0, 0, 120, 40));

        app.mode = Mode::WorkflowRuns;
        let open = compute_workflow_runs_view(&app, Rect::new(0, 0, 120, 40), carried.clone());
        assert_eq!(open.entries.len(), 1, "the entries are carried");
        assert!(open.modal_rect.width > 0);

        app.mode = Mode::Terminal;
        let closed = compute_workflow_runs_view(&app, Rect::new(0, 0, 120, 40), carried);
        assert_eq!(closed, WorkflowRunsState::default());
    }
}
