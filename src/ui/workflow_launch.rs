//! The workflow launcher modal
//! (`docs/design/workflow-builder/06-phase2-plan.md` §4 D18).
//!
//! Split the same way the DAG overlay is, and for the same reason:
//!
//! - [`compute_workflow_launch_view`] runs in the view-computation pass — the
//!   one place allowed to mutate `AppState` — and stores every rectangle the
//!   modal will draw into.
//! - [`render_workflow_launch`] takes `&AppState` and only draws, deriving its
//!   rows from the stored [`WorkflowLaunchState::modal_rect`] through the same
//!   pure [`launch_sections`] the compute pass used, so the hit-test can never
//!   disagree with what was drawn.
//!
//! Three sections, in the existing modal language ([`modal_stack_areas`] +
//! [`action_button_row_rects`], never the `pub(super)` `centered_button_row`):
//! the workflow list, one line per declared arg, and the tier row seeded from
//! `WorkflowSummary.default_tier` — which `workflow.list` already returns, so
//! the row needs no new storage and no new query (§4 D17).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::text::truncate_end;
use super::widgets::{
    action_button_row_rects, centered_popup_rect, modal_stack_areas, panel_contrast_fg,
    render_action_button, render_panel_shell, ActionButtonSpec,
};
use crate::app::state::{
    AppState, Mode, Palette, WorkflowLaunchArg, WorkflowLaunchEntry, WorkflowLaunchFocus,
    WorkflowLaunchState,
};
use crate::workflow::tier::Tier;

pub(crate) const LAUNCH_MODAL_WIDTH: u16 = 72;
pub(crate) const LAUNCH_MODAL_HEIGHT: u16 = 20;

/// Header holds the title; footer holds the refusal/hint line; the action row
/// holds the two buttons.
const HEADER_HEIGHT: u16 = 1;
const FOOTER_HEIGHT: u16 = 1;
const ACTIONS_HEIGHT: u16 = 1;
const STACK_GAP: u16 = 1;
/// A form with more than this many visible arg lines starves the list; the
/// rest are still reachable, they simply have no row this frame.
const MAX_ARG_ROWS: u16 = 5;

/// The tier row, in the order §7.1's table reads.
pub(crate) const LAUNCH_TIERS: [Tier; 5] =
    [Tier::Auto, Tier::Max, Tier::High, Tier::Medium, Tier::Low];

/// What the pointer is over. Returned by [`workflow_launch_target_at`] and
/// consumed by the input layer, so clicking and keying share one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowLaunchTarget {
    Workflow(usize),
    Arg(usize),
    Tier(Tier),
    Run,
    Cancel,
}

// ── view computation (mutation pass) ────────────────────────────────────────

/// Refreshes the launcher's geometry for this frame.
///
/// `carried` is the previous frame's state: the list, the arg values the user
/// is typing, and the tier they picked are *their* in-progress input, and
/// `ViewState` is rebuilt wholesale every pass, so it is carried rather than
/// recomputed. Closing the modal drops it entirely, which is what stops any
/// other mode from hit-testing stale launcher geometry.
pub(super) fn compute_workflow_launch_view(
    app: &AppState,
    area: Rect,
    carried: WorkflowLaunchState,
) -> WorkflowLaunchState {
    if app.mode != Mode::WorkflowLaunch {
        return WorkflowLaunchState::default();
    }
    let mut view = carried;
    apply_launch_geometry(&mut view, area);
    view
}

/// Stores the rects the renderer draws and the hit-test reads. Pure: the only
/// inputs are the area and the counts already on the state.
pub(crate) fn apply_launch_geometry(view: &mut WorkflowLaunchState, area: Rect) {
    view.modal_rect = Rect::default();
    view.list_rect = Rect::default();
    view.workflow_rects = vec![Rect::default(); view.workflows.len()];
    view.arg_rects = vec![Rect::default(); view.args.len()];
    view.tier_rects = Vec::new();
    view.button_rects = Vec::new();

    let Some(popup) = centered_popup_rect(area, LAUNCH_MODAL_WIDTH, LAUNCH_MODAL_HEIGHT) else {
        return;
    };
    view.modal_rect = popup;
    let Some(sections) =
        launch_sections(popup, view.workflows.len(), view.selected, view.args.len())
    else {
        return;
    };
    view.list_rect = sections.list;
    view.workflow_rects = sections.workflows;
    view.arg_rects = sections.args;
    view.tier_rects = sections.tiers;
    view.button_rects = sections.buttons;
}

/// Every rectangle the modal is made of, including the ones the renderer needs
/// but the hit-test does not (section titles, the footer line).
pub(crate) struct LaunchSections {
    pub header: Rect,
    pub list_title: Rect,
    pub list: Rect,
    /// One rect per workflow. Rows outside the visible window get an empty
    /// rect, so indices stay aligned with `workflows` without a scroll offset
    /// on the state.
    pub workflows: Vec<Rect>,
    pub args_title: Rect,
    pub args: Vec<Rect>,
    pub tier_title: Rect,
    pub tiers: Vec<Rect>,
    pub footer: Rect,
    pub buttons: Vec<Rect>,
}

/// Splits the modal into its sections. `popup` is the bordered outer rect, so
/// this is the one place that knows the border costs a row and a column on
/// each side.
///
/// A short modal yields in this order: the arg lines shrink first, then the
/// list, and the tier row and the action buttons are the last things to go —
/// a form you cannot submit is worse than a form you have to scroll.
pub(crate) fn launch_sections(
    popup: Rect,
    workflow_count: usize,
    selected: usize,
    arg_count: usize,
) -> Option<LaunchSections> {
    if popup.width < 4 || popup.height < 6 {
        return None;
    }
    let inner = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let stack = modal_stack_areas(
        inner,
        HEADER_HEIGHT,
        FOOTER_HEIGHT,
        ACTIONS_HEIGHT,
        STACK_GAP,
    );
    let content = stack.content;
    if content.height == 0 {
        return None;
    }

    // Bottom-up: the tier row is fixed, the args take what is left over the
    // list's one guaranteed row, and the list takes the remainder.
    let mut remaining = content.height;
    let tier_height = 2.min(remaining);
    remaining = remaining.saturating_sub(tier_height);

    let wanted_arg_rows = (arg_count as u16).min(MAX_ARG_ROWS);
    let args_height = if wanted_arg_rows == 0 {
        0
    } else {
        // Keep at least one row for the list, plus its title.
        (wanted_arg_rows + 1).min(remaining.saturating_sub(2))
    };
    remaining = remaining.saturating_sub(args_height);

    let list_title = Rect::new(content.x, content.y, content.width, 1.min(remaining));
    let list = Rect::new(
        content.x,
        list_title.bottom(),
        content.width,
        remaining.saturating_sub(list_title.height),
    );
    let args_area = Rect::new(content.x, list.bottom(), content.width, args_height);
    let args_title = Rect::new(
        args_area.x,
        args_area.y,
        args_area.width,
        1.min(args_height),
    );
    let tier_area = Rect::new(content.x, args_area.bottom(), content.width, tier_height);
    let tier_title = Rect::new(
        tier_area.x,
        tier_area.y,
        tier_area.width,
        1.min(tier_height),
    );
    let tier_row = Rect::new(
        tier_area.x,
        tier_title.bottom(),
        tier_area.width,
        tier_area.height.saturating_sub(tier_title.height),
    );

    let workflows = windowed_rows(
        list,
        workflow_count,
        list_window_offset(workflow_count, selected, list.height),
    );
    let args = windowed_rows(
        Rect::new(
            args_area.x,
            args_title.bottom(),
            args_area.width,
            args_height.saturating_sub(args_title.height),
        ),
        arg_count,
        0,
    );

    let tiers = if tier_row.height == 0 {
        Vec::new()
    } else {
        action_button_row_rects(tier_row, &tier_button_specs(), 1, 0)
    };
    let buttons = match stack.actions {
        Some(actions) => action_button_row_rects(
            actions,
            &[
                ActionButtonSpec {
                    hint: Some("↵"),
                    label: "run",
                },
                ActionButtonSpec {
                    hint: Some("esc"),
                    label: "cancel",
                },
            ],
            2,
            0,
        ),
        None => Vec::new(),
    };

    Some(LaunchSections {
        header: stack.header,
        list_title,
        list,
        workflows,
        args_title,
        args,
        tier_title,
        tiers,
        footer: stack.footer.unwrap_or_default(),
        buttons,
    })
}

fn tier_button_specs() -> Vec<ActionButtonSpec<'static>> {
    LAUNCH_TIERS
        .iter()
        .map(|tier| ActionButtonSpec {
            hint: None,
            label: tier.as_str(),
        })
        .collect()
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

// ── hit-testing ─────────────────────────────────────────────────────────────

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Whether the pointer is anywhere inside the modal. A click outside it closes
/// the launcher, the same gesture every other modal answers to.
pub(crate) fn workflow_launch_contains(state: &WorkflowLaunchState, col: u16, row: u16) -> bool {
    rect_contains(state.modal_rect, col, row)
}

/// Reads exactly the rects the compute pass stored.
pub(crate) fn workflow_launch_target_at(
    state: &WorkflowLaunchState,
    col: u16,
    row: u16,
) -> Option<WorkflowLaunchTarget> {
    if let Some(index) = state
        .workflow_rects
        .iter()
        .position(|rect| rect_contains(*rect, col, row))
    {
        return Some(WorkflowLaunchTarget::Workflow(index));
    }
    if let Some(index) = state
        .arg_rects
        .iter()
        .position(|rect| rect_contains(*rect, col, row))
    {
        return Some(WorkflowLaunchTarget::Arg(index));
    }
    if let Some(index) = state
        .tier_rects
        .iter()
        .position(|rect| rect_contains(*rect, col, row))
    {
        return LAUNCH_TIERS
            .get(index)
            .copied()
            .map(WorkflowLaunchTarget::Tier);
    }
    match state
        .button_rects
        .iter()
        .position(|rect| rect_contains(*rect, col, row))
    {
        Some(0) => Some(WorkflowLaunchTarget::Run),
        Some(1) => Some(WorkflowLaunchTarget::Cancel),
        _ => None,
    }
}

// ── rendering (draw only) ───────────────────────────────────────────────────

pub(super) fn render_workflow_launch(app: &AppState, frame: &mut Frame, area: Rect) {
    let launch = &app.view.workflow_launch;
    let popup = launch.modal_rect;
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    let Some(sections) = launch_sections(
        popup,
        launch.workflows.len(),
        launch.selected,
        launch.args.len(),
    ) else {
        return;
    };

    let p = &app.palette;
    super::dim_background(frame, area);
    if render_panel_shell(frame, popup, p.accent, p.panel_bg).is_none() {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " run a workflow",
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   tab moves · ↵ runs", Style::default().fg(p.overlay1)),
        ])),
        sections.header,
    );

    render_section_title(frame, sections.list_title, "workflow", p);
    render_workflow_rows(launch, frame, &sections, p);

    if sections.args_title.height > 0 {
        render_section_title(frame, sections.args_title, "arguments", p);
    }
    render_arg_rows(launch, frame, &sections, p);

    if sections.tier_title.height > 0 {
        render_section_title(frame, sections.tier_title, "tier", p);
    }
    render_tier_row(launch, frame, &sections, p);
    render_footer(launch, frame, sections.footer, p);
    render_buttons(launch, frame, &sections, p);
}

fn render_section_title(frame: &mut Frame, area: Rect, title: &str, p: &Palette) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {title}"),
            Style::default().fg(p.overlay1).add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

fn render_workflow_rows(
    launch: &WorkflowLaunchState,
    frame: &mut Frame,
    sections: &LaunchSections,
    p: &Palette,
) {
    if launch.workflows.is_empty() {
        if sections.list.height > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " no workflows yet — create one with kvx workflow create",
                    Style::default().fg(p.overlay1),
                )),
                Rect::new(sections.list.x, sections.list.y, sections.list.width, 1),
            );
        }
        return;
    }

    let focused = launch.focus == WorkflowLaunchFocus::Workflows;
    for (index, (entry, rect)) in launch
        .workflows
        .iter()
        .zip(sections.workflows.iter())
        .enumerate()
    {
        if rect.height == 0 {
            continue;
        }
        let selected = index == launch.selected;
        frame.render_widget(
            Paragraph::new(workflow_row_line(entry, rect.width, selected, focused, p)),
            *rect,
        );
    }
}

fn workflow_row_line(
    entry: &WorkflowLaunchEntry,
    width: u16,
    selected: bool,
    focused: bool,
    p: &Palette,
) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let text = if entry.description.trim().is_empty() {
        entry.name.clone()
    } else {
        format!("{} — {}", entry.name, entry.description.trim())
    };
    let text = truncate_end(&text, width.saturating_sub(2) as usize);
    let style = match (selected, focused) {
        (true, true) => Style::default()
            .bg(p.surface0)
            .fg(p.text)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::default().fg(p.text),
        _ => Style::default().fg(p.subtext0),
    };
    Line::from(vec![Span::styled(format!("{marker}{text}"), style)])
}

fn render_arg_rows(
    launch: &WorkflowLaunchState,
    frame: &mut Frame,
    sections: &LaunchSections,
    p: &Palette,
) {
    for (index, (arg, rect)) in launch.args.iter().zip(sections.args.iter()).enumerate() {
        if rect.height == 0 {
            continue;
        }
        let focused = launch.focus == WorkflowLaunchFocus::Arg(index);
        frame.render_widget(
            Paragraph::new(arg_row_line(arg, rect.width, focused, p)),
            *rect,
        );
    }
}

fn arg_row_line(arg: &WorkflowLaunchArg, width: u16, focused: bool, p: &Palette) -> Line<'static> {
    let required = if arg.required { "*" } else { " " };
    let label = format!(" {required}{} ", arg.name);
    let value_width = (width as usize).saturating_sub(label.chars().count() + 2);
    let value = truncate_end(&arg.value, value_width);
    let caret = if focused { "▏" } else { "" };
    let label_style = if arg.required && arg.value.trim().is_empty() {
        Style::default().fg(p.peach)
    } else {
        Style::default().fg(p.overlay1)
    };
    let value_style = if focused {
        Style::default().bg(p.surface0).fg(p.text)
    } else {
        Style::default().fg(p.subtext0)
    };
    Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(format!("{value}{caret}"), value_style),
    ])
}

fn render_tier_row(
    launch: &WorkflowLaunchState,
    frame: &mut Frame,
    sections: &LaunchSections,
    p: &Palette,
) {
    let focused = launch.focus == WorkflowLaunchFocus::Tier;
    let active = launch.tier;
    for (tier, rect) in LAUNCH_TIERS.iter().zip(sections.tiers.iter()) {
        let is_active = active == Some(*tier);
        let style = match (is_active, focused) {
            (true, true) => Style::default()
                .bg(p.accent)
                .fg(panel_contrast_fg(p))
                .add_modifier(Modifier::BOLD),
            (true, false) => Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            (false, true) => Style::default().fg(p.subtext0),
            (false, false) => Style::default().fg(p.overlay1),
        };
        render_action_button(frame, *rect, None, tier.as_str(), style);
    }
}

fn render_footer(launch: &WorkflowLaunchState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let (text, style) = match (&launch.error, launch.submitting) {
        (Some(error), _) => (error.clone(), Style::default().fg(p.red)),
        (None, true) => ("starting…".to_string(), Style::default().fg(p.overlay1)),
        (None, false) => (
            "* marks a required argument".to_string(),
            Style::default().fg(p.overlay1),
        ),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(
                " {}",
                truncate_end(&text, area.width.saturating_sub(1) as usize)
            ),
            style,
        )),
        area,
    );
}

fn render_buttons(
    launch: &WorkflowLaunchState,
    frame: &mut Frame,
    sections: &LaunchSections,
    p: &Palette,
) {
    let labels = [(Some("↵"), "run"), (Some("esc"), "cancel")];
    for (index, (rect, (hint, label))) in sections.buttons.iter().zip(labels.iter()).enumerate() {
        let primary = index == 0;
        let focused = primary && launch.focus == WorkflowLaunchFocus::Confirm;
        let style = if primary {
            let base = Style::default().bg(p.accent).fg(panel_contrast_fg(p));
            if focused {
                base.add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                base.add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default().fg(p.overlay1)
        };
        render_action_button(frame, *rect, *hint, label, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(workflows: usize, args: usize) -> WorkflowLaunchState {
        WorkflowLaunchState {
            workflows: (0..workflows)
                .map(|index| WorkflowLaunchEntry {
                    workflow_id: format!("workflow:{index}"),
                    name: format!("workflow {index}"),
                    description: String::new(),
                    default_tier: Some(Tier::High),
                })
                .collect(),
            args: (0..args)
                .map(|index| WorkflowLaunchArg {
                    name: format!("arg{index}"),
                    description: String::new(),
                    required: true,
                    value: String::new(),
                })
                .collect(),
            tier: Some(Tier::High),
            ..WorkflowLaunchState::default()
        }
    }

    #[test]
    fn geometry_gives_one_rect_per_entry_and_five_tiers() {
        let mut view = state_with(3, 2);
        apply_launch_geometry(&mut view, Rect::new(0, 0, 120, 40));

        assert_eq!(view.workflow_rects.len(), 3);
        assert_eq!(view.arg_rects.len(), 2);
        assert_eq!(view.tier_rects.len(), LAUNCH_TIERS.len());
        assert_eq!(view.button_rects.len(), 2);
        assert!(view.modal_rect.width >= 4 && view.modal_rect.height >= 6);
        // Every stored row sits inside the modal.
        for rect in view
            .workflow_rects
            .iter()
            .chain(view.arg_rects.iter())
            .chain(view.tier_rects.iter())
            .chain(view.button_rects.iter())
        {
            assert!(rect.y >= view.modal_rect.y, "{rect:?}");
            assert!(rect.bottom() <= view.modal_rect.bottom(), "{rect:?}");
        }
    }

    #[test]
    fn a_tiny_area_stores_no_clickable_geometry() {
        let mut view = state_with(3, 2);
        apply_launch_geometry(&mut view, Rect::new(0, 0, 8, 4));

        assert_eq!(view.modal_rect, Rect::default());
        assert!(view.tier_rects.is_empty());
        assert!(view.button_rects.is_empty());
        assert!(view.workflow_rects.iter().all(|rect| rect.height == 0));
        assert!(workflow_launch_target_at(&view, 0, 0).is_none());
    }

    #[test]
    fn the_hit_test_agrees_with_the_stored_rects() {
        let mut view = state_with(3, 2);
        view.selected = 1;
        apply_launch_geometry(&mut view, Rect::new(0, 0, 120, 40));

        for (index, rect) in view.workflow_rects.clone().iter().enumerate() {
            assert_eq!(
                workflow_launch_target_at(&view, rect.x, rect.y),
                Some(WorkflowLaunchTarget::Workflow(index))
            );
        }
        for (index, rect) in view.arg_rects.clone().iter().enumerate() {
            assert_eq!(
                workflow_launch_target_at(&view, rect.x, rect.y),
                Some(WorkflowLaunchTarget::Arg(index))
            );
        }
        for (tier, rect) in LAUNCH_TIERS.iter().zip(view.tier_rects.clone().iter()) {
            assert_eq!(
                workflow_launch_target_at(&view, rect.x, rect.y),
                Some(WorkflowLaunchTarget::Tier(*tier))
            );
        }
        let buttons = view.button_rects.clone();
        assert_eq!(
            workflow_launch_target_at(&view, buttons[0].x, buttons[0].y),
            Some(WorkflowLaunchTarget::Run)
        );
        assert_eq!(
            workflow_launch_target_at(&view, buttons[1].x, buttons[1].y),
            Some(WorkflowLaunchTarget::Cancel)
        );
        assert!(workflow_launch_contains(
            &view,
            view.modal_rect.x,
            view.modal_rect.y
        ));
        assert!(!workflow_launch_contains(&view, 0, 0));
    }

    #[test]
    fn a_selection_below_the_window_scrolls_the_list_without_a_stored_offset() {
        let mut view = state_with(40, 0);
        view.selected = 39;
        apply_launch_geometry(&mut view, Rect::new(0, 0, 120, 40));

        let visible: Vec<usize> = view
            .workflow_rects
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
        assert_eq!(
            workflow_launch_target_at(&view, 0, 0),
            None,
            "rows scrolled out are not clickable"
        );
    }

    #[test]
    fn the_renderer_and_the_compute_pass_read_the_same_sections() {
        let mut view = state_with(3, 2);
        apply_launch_geometry(&mut view, Rect::new(0, 0, 120, 40));

        let sections = launch_sections(
            view.modal_rect,
            view.workflows.len(),
            view.selected,
            view.args.len(),
        )
        .expect("the modal fits");
        assert_eq!(sections.workflows, view.workflow_rects);
        assert_eq!(sections.args, view.arg_rects);
        assert_eq!(sections.tiers, view.tier_rects);
        assert_eq!(sections.buttons, view.button_rects);
        assert_eq!(sections.list, view.list_rect);
    }

    fn screen_of(launch: &WorkflowLaunchState, area: Rect) -> String {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowLaunch;
        app.view.workflow_launch = launch.clone();
        // `render` only ever sees `&AppState`; nothing below can mutate it.
        let app = &app;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_launch(app, frame, area))
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
    fn render_draws_the_list_the_arg_lines_and_the_tier_row() {
        let area = Rect::new(0, 0, 100, 30);
        let mut view = state_with(2, 1);
        view.workflows[0].description = "ships the thing".into();
        view.args[0].name = "goal".into();
        view.args[0].value = "add dark mode".into();
        view.focus = WorkflowLaunchFocus::Arg(0);
        apply_launch_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("run a workflow"), "{screen}");
        assert!(screen.contains("workflow 0"), "{screen}");
        assert!(screen.contains("ships the thing"), "{screen}");
        assert!(screen.contains("goal"), "{screen}");
        assert!(screen.contains("add dark mode"), "{screen}");
        for tier in LAUNCH_TIERS {
            assert!(screen.contains(tier.as_str()), "missing {tier}\n{screen}");
        }
        assert!(screen.contains("run"), "{screen}");
        assert!(screen.contains("cancel"), "{screen}");
        assert!(
            screen.contains("*goal"),
            "a required arg is marked\n{screen}"
        );
    }

    #[test]
    fn a_refusal_replaces_the_hint_line() {
        let area = Rect::new(0, 0, 100, 30);
        let mut view = state_with(1, 1);
        view.error = Some("goal is required".into());
        apply_launch_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("goal is required"), "{screen}");
        assert!(!screen.contains("marks a required argument"), "{screen}");
    }

    #[test]
    fn an_empty_list_says_so_rather_than_drawing_nothing() {
        let area = Rect::new(0, 0, 100, 30);
        let mut view = state_with(0, 0);
        apply_launch_geometry(&mut view, area);

        let screen = screen_of(&view, area);
        assert!(screen.contains("no workflows yet"), "{screen}");
    }

    #[test]
    fn a_terminal_too_small_for_the_modal_draws_nothing_and_does_not_panic() {
        for (width, height) in [(4, 3), (10, 5), (20, 8), (40, 10)] {
            let area = Rect::new(0, 0, width, height);
            let mut view = state_with(3, 2);
            apply_launch_geometry(&mut view, area);
            let _ = screen_of(&view, area);
        }
    }

    #[test]
    fn closing_the_modal_drops_the_geometry() {
        let mut app = AppState::test_new();
        let mut carried = state_with(3, 2);
        apply_launch_geometry(&mut carried, Rect::new(0, 0, 120, 40));

        app.mode = Mode::WorkflowLaunch;
        let open = compute_workflow_launch_view(&app, Rect::new(0, 0, 120, 40), carried.clone());
        assert_eq!(open.workflows.len(), 3, "the user's input is carried");
        assert!(open.modal_rect.width > 0);

        app.mode = Mode::Terminal;
        let closed = compute_workflow_launch_view(&app, Rect::new(0, 0, 120, 40), carried);
        assert_eq!(closed, WorkflowLaunchState::default());
    }
}
