//! Input for the workflow launcher modal
//! (`docs/design/workflow-builder/06-phase2-plan.md` §4 D18).
//!
//! Split the way every other overlay's input is: the pure half
//! ([`apply_workflow_launch_key`], [`launch_run_params`]) moves focus, edits
//! the arg lines and picks a tier on `&mut AppState` alone, and returns an
//! [`LaunchIntent`] for the half that needs the runtime. The effectful half
//! lives on `App` and speaks the same in-process API path `workflow.list`,
//! `workflow.get` and `workflow.run` do, so the launcher can never start a run
//! the CLI could not have started.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::modal::{leave_modal, modal_action_from_key, ModalAction, WORKFLOW_LAUNCH_ACTIONS};
use crate::api::schema::{
    EmptyParams, ErrorResponse, Method, ResponseResult, SuccessResponse, WorkflowRunParams,
    WorkflowTarget, WorkflowTier,
};
use crate::app::state::{
    AppState, Mode, WorkflowLaunchArg, WorkflowLaunchEntry, WorkflowLaunchFocus,
    WorkflowLaunchState,
};
use crate::app::App;
use crate::ui::{workflow_launch_contains, workflow_launch_target_at, WorkflowLaunchTarget};
use crate::workflow::model::{NoticeLevel, UserNotice};
use crate::workflow::tier::Tier;

/// What the pure key/mouse handler decided the runtime half has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchIntent {
    /// Handled entirely in state.
    None,
    /// `Esc`, the cancel button, or a click outside the modal.
    Close,
    /// The selected workflow changed, so its declared args have to be read
    /// again.
    SelectionChanged,
    /// `Enter` or the run button.
    Confirm,
}

/// Why a confirm was refused. Both variants name the field the user has to fix,
/// which is what lets the handler move focus there instead of only complaining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LaunchRefusal {
    NoWorkflow,
    MissingArg { index: usize, name: String },
}

impl LaunchRefusal {
    pub(super) fn message(&self) -> String {
        match self {
            Self::NoWorkflow => {
                "no workflow selected — create one with kvx workflow create".to_string()
            }
            Self::MissingArg { name, .. } => format!("{name} is required"),
        }
    }
}

// ── pure half ───────────────────────────────────────────────────────────────

/// The focus ring: the list, then one stop per arg line, then the tier row,
/// then the run button.
fn focus_ring(args: usize) -> Vec<WorkflowLaunchFocus> {
    let mut ring = vec![WorkflowLaunchFocus::Workflows];
    ring.extend((0..args).map(WorkflowLaunchFocus::Arg));
    ring.push(WorkflowLaunchFocus::Tier);
    ring.push(WorkflowLaunchFocus::Confirm);
    ring
}

fn move_focus(launch: &mut WorkflowLaunchState, delta: isize) {
    let ring = focus_ring(launch.args.len());
    let current = ring
        .iter()
        .position(|focus| *focus == launch.focus)
        .unwrap_or(0) as isize;
    let len = ring.len() as isize;
    let next = (current + delta).rem_euclid(len) as usize;
    if let Some(focus) = ring.get(next) {
        launch.focus = *focus;
    }
}

fn move_tier(launch: &mut WorkflowLaunchState, delta: isize) {
    let tiers = crate::ui::LAUNCH_TIERS;
    let current = launch
        .tier
        .and_then(|tier| tiers.iter().position(|candidate| *candidate == tier))
        .unwrap_or(0) as isize;
    let next = (current + delta).rem_euclid(tiers.len() as isize) as usize;
    launch.tier = tiers.get(next).copied();
    launch.error = None;
}

fn move_selection(launch: &mut WorkflowLaunchState, delta: isize) -> LaunchIntent {
    if launch.workflows.is_empty() {
        return LaunchIntent::None;
    }
    let len = launch.workflows.len() as isize;
    let current = (launch.selected.min(launch.workflows.len() - 1)) as isize;
    let next = (current + delta).rem_euclid(len) as usize;
    if next == launch.selected {
        return LaunchIntent::None;
    }
    launch.selected = next;
    launch.error = None;
    LaunchIntent::SelectionChanged
}

/// Inserts typed or pasted text into the focused arg line. Append-only, like
/// the DAG's steer line — one text-entry model for the whole workflow surface.
pub(crate) fn insert_workflow_launch_text(state: &mut AppState, text: &str) -> bool {
    let launch = &mut state.view.workflow_launch;
    let WorkflowLaunchFocus::Arg(index) = launch.focus else {
        return false;
    };
    let Some(arg) = launch.args.get_mut(index) else {
        return false;
    };
    arg.value
        .extend(text.chars().filter(|character| !character.is_control()));
    launch.error = None;
    true
}

/// Everything the launcher can do without the runtime.
pub(super) fn apply_workflow_launch_key(state: &mut AppState, key: KeyEvent) -> LaunchIntent {
    // `Enter` and `Esc` come from the same modal action table every other
    // overlay reads, so the launcher's two terminal gestures are not a
    // one-off.
    if let Some(action) = modal_action_from_key(&key, WORKFLOW_LAUNCH_ACTIONS) {
        return match action {
            ModalAction::Confirm => LaunchIntent::Confirm,
            _ => LaunchIntent::Close,
        };
    }

    let launch = &mut state.view.workflow_launch;
    match key.code {
        KeyCode::Tab => {
            move_focus(launch, 1);
            LaunchIntent::None
        }
        KeyCode::BackTab => {
            move_focus(launch, -1);
            LaunchIntent::None
        }
        KeyCode::Down => {
            if launch.focus == WorkflowLaunchFocus::Workflows {
                move_selection(launch, 1)
            } else {
                move_focus(launch, 1);
                LaunchIntent::None
            }
        }
        KeyCode::Up => {
            if launch.focus == WorkflowLaunchFocus::Workflows {
                move_selection(launch, -1)
            } else {
                move_focus(launch, -1);
                LaunchIntent::None
            }
        }
        KeyCode::Left if launch.focus == WorkflowLaunchFocus::Tier => {
            move_tier(launch, -1);
            LaunchIntent::None
        }
        KeyCode::Right if launch.focus == WorkflowLaunchFocus::Tier => {
            move_tier(launch, 1);
            LaunchIntent::None
        }
        KeyCode::Backspace => {
            if let WorkflowLaunchFocus::Arg(index) = launch.focus {
                if let Some(arg) = launch.args.get_mut(index) {
                    arg.value.pop();
                    launch.error = None;
                }
            }
            LaunchIntent::None
        }
        // `j`/`k` navigate the workflow list, matching the DAG view's own
        // bindings — but only while the list itself has focus. When focus is
        // an `Arg`, these must still type `j`/`k` into the field, which is
        // why the guard is on `launch.focus`, not a global mode check.
        KeyCode::Char('j')
            if launch.focus == WorkflowLaunchFocus::Workflows
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            move_selection(launch, 1)
        }
        KeyCode::Char('k')
            if launch.focus == WorkflowLaunchFocus::Workflows
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            move_selection(launch, -1)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let WorkflowLaunchFocus::Arg(index) = launch.focus {
                if let Some(arg) = launch.args.get_mut(index) {
                    arg.value.clear();
                    launch.error = None;
                }
            }
            LaunchIntent::None
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_workflow_launch_text(state, &character.to_string());
            LaunchIntent::None
        }
        _ => LaunchIntent::None,
    }
}

/// The one gate on confirm: a workflow has to be selected and every required
/// arg has to be filled. Pure, so the refusal is the same whether the run was
/// confirmed with the keyboard or the mouse.
pub(super) fn launch_run_params(
    launch: &WorkflowLaunchState,
) -> Result<WorkflowRunParams, LaunchRefusal> {
    let entry = launch
        .workflows
        .get(launch.selected)
        .ok_or(LaunchRefusal::NoWorkflow)?;
    if let Some((index, arg)) = launch
        .args
        .iter()
        .enumerate()
        .find(|(_, arg)| arg.required && arg.value.trim().is_empty())
    {
        return Err(LaunchRefusal::MissingArg {
            index,
            name: arg.name.clone(),
        });
    }

    let args = launch
        .args
        .iter()
        .filter(|arg| !arg.value.trim().is_empty())
        .map(|arg| (arg.name.clone(), arg.value.trim().to_string()))
        .collect();
    // The tier row is seeded from the workflow's `default_tier`, so falling
    // back to it keeps the request identical to the one `kvx workflow run
    // start` sends with no `--tier`.
    let tier = launch.tier.or(entry.default_tier);
    Ok(WorkflowRunParams {
        workflow_id: entry.workflow_id.clone(),
        version: None,
        tier: tier.map(crate::app::workflow::wire_tier),
        args,
        restore_from: None,
        include_prior_summaries: None,
    })
}

/// The wire tier back into the engine's. The API handler owns the inbound
/// direction; this is the outbound one, for a `default_tier` read off
/// `workflow.list`.
pub(crate) fn engine_tier(tier: WorkflowTier) -> Tier {
    match tier {
        WorkflowTier::Auto => Tier::Auto,
        WorkflowTier::Max => Tier::Max,
        WorkflowTier::High => Tier::High,
        WorkflowTier::Medium => Tier::Medium,
        WorkflowTier::Low => Tier::Low,
    }
}

fn success_result(response: &str) -> Option<ResponseResult> {
    serde_json::from_str::<SuccessResponse>(response)
        .ok()
        .map(|success| success.result)
}

fn error_message(response: &str) -> Option<String> {
    serde_json::from_str::<ErrorResponse>(response)
        .ok()
        .map(|error| error.error.message)
}

// ── runtime half ────────────────────────────────────────────────────────────

impl App {
    /// Opens the launcher, seeded from `workflow.list`.
    ///
    /// Returns whether it opened: with no store (a slim build, or a server that
    /// could not open the workflow database) and with no workflows there is
    /// nothing to launch, and the caller — which knows which binding was
    /// pressed — says so rather than leaving a bound key doing nothing at all.
    pub(crate) fn open_workflow_launcher(&mut self) -> bool {
        let response = self.dispatch_api_request(
            "tui.workflow.list",
            Method::WorkflowList(EmptyParams::default()),
        );
        let Some(ResponseResult::WorkflowList { workflows }) = success_result(&response) else {
            if let Some(message) = error_message(&response) {
                tracing::debug!(%message, "the workflow launcher could not list workflows");
            }
            return false;
        };
        let entries: Vec<WorkflowLaunchEntry> = workflows
            .into_iter()
            .filter(|workflow| !workflow.archived)
            .map(|workflow| WorkflowLaunchEntry {
                workflow_id: workflow.workflow_id,
                name: workflow.name,
                description: workflow.description,
                default_tier: Some(engine_tier(workflow.default_tier)),
            })
            .collect();
        if entries.is_empty() {
            return false;
        }

        let tier = entries.first().and_then(|entry| entry.default_tier);
        self.state.view.workflow_launch = WorkflowLaunchState {
            workflows: entries,
            tier,
            ..WorkflowLaunchState::default()
        };
        self.state.mode = Mode::WorkflowLaunch;
        self.load_workflow_launch_args();
        true
    }

    /// `keys.open_workflow_launcher` was pressed with nothing to launch.
    pub(crate) fn notify_no_workflows_to_launch(&mut self) {
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: None,
            path: None,
            message: "no workflows on this server — create one with kvx workflow create"
                .to_string(),
        });
    }

    pub(crate) fn handle_workflow_launch_key(&mut self, key: KeyEvent) {
        let intent = apply_workflow_launch_key(&mut self.state, key);
        self.apply_workflow_launch_intent(intent);
    }

    pub(super) fn handle_workflow_launch_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let intent = move_selection(&mut self.state.view.workflow_launch, -1);
                self.apply_workflow_launch_intent(intent);
            }
            MouseEventKind::ScrollDown => {
                let intent = move_selection(&mut self.state.view.workflow_launch, 1);
                self.apply_workflow_launch_intent(intent);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let intent = self.click_workflow_launch(mouse.column, mouse.row);
                self.apply_workflow_launch_intent(intent);
            }
            _ => {}
        }
        true
    }

    /// Hit-tests against exactly the rects the view-computation pass stored, so
    /// what is clickable can never disagree with what was drawn.
    fn click_workflow_launch(&mut self, column: u16, row: u16) -> LaunchIntent {
        let launch = &mut self.state.view.workflow_launch;
        let Some(target) = workflow_launch_target_at(launch, column, row) else {
            // Clicking away from a modal closes it, the same gesture every
            // other overlay answers to.
            return if workflow_launch_contains(launch, column, row) {
                LaunchIntent::None
            } else {
                LaunchIntent::Close
            };
        };
        match target {
            WorkflowLaunchTarget::Workflow(index) => {
                launch.focus = WorkflowLaunchFocus::Workflows;
                if index == launch.selected {
                    return LaunchIntent::None;
                }
                launch.selected = index;
                launch.error = None;
                LaunchIntent::SelectionChanged
            }
            WorkflowLaunchTarget::Arg(index) => {
                launch.focus = WorkflowLaunchFocus::Arg(index);
                LaunchIntent::None
            }
            WorkflowLaunchTarget::Tier(tier) => {
                launch.focus = WorkflowLaunchFocus::Tier;
                launch.tier = Some(tier);
                launch.error = None;
                LaunchIntent::None
            }
            WorkflowLaunchTarget::Run => LaunchIntent::Confirm,
            WorkflowLaunchTarget::Cancel => LaunchIntent::Close,
        }
    }

    fn apply_workflow_launch_intent(&mut self, intent: LaunchIntent) {
        match intent {
            LaunchIntent::None => {}
            LaunchIntent::Close => leave_modal(&mut self.state),
            LaunchIntent::SelectionChanged => self.load_workflow_launch_args(),
            LaunchIntent::Confirm => self.submit_workflow_launch(),
        }
    }

    /// Reads the selected workflow's declared args from `workflow.get`'s
    /// `detail` projection — the same one the CLI renders — and turns them into
    /// one line each, pre-filled with the declared default.
    ///
    /// The tier row is re-seeded here too: the selected workflow's
    /// `default_tier` is the authoring-time answer, and Phase 2 adds no second
    /// copy of it (§4 D17).
    fn load_workflow_launch_args(&mut self) {
        let Some(workflow_id) = self
            .state
            .view
            .workflow_launch
            .workflows
            .get(self.state.view.workflow_launch.selected)
            .map(|entry| entry.workflow_id.clone())
        else {
            return;
        };
        let seeded_tier = self
            .state
            .view
            .workflow_launch
            .workflows
            .get(self.state.view.workflow_launch.selected)
            .and_then(|entry| entry.default_tier);

        let response = self.dispatch_api_request(
            "tui.workflow.get",
            Method::WorkflowGet(WorkflowTarget { workflow_id }),
        );
        let args = match success_result(&response) {
            Some(ResponseResult::WorkflowGet {
                detail: Some(detail),
                ..
            }) => detail
                .args
                .into_iter()
                .map(|arg| WorkflowLaunchArg {
                    name: arg.name,
                    description: arg.description,
                    // An arg with a default is never a gate: the run would
                    // start with that value from the CLI too.
                    required: arg.required && arg.default.is_none(),
                    value: arg.default.unwrap_or_default(),
                })
                .collect(),
            _ => Vec::new(),
        };

        let launch = &mut self.state.view.workflow_launch;
        launch.args = args;
        launch.tier = seeded_tier.or(launch.tier);
        launch.error = None;
        if let WorkflowLaunchFocus::Arg(index) = launch.focus {
            if index >= launch.args.len() {
                launch.focus = WorkflowLaunchFocus::Workflows;
            }
        }
    }

    /// Starts the run through the same in-process path `workflow.run` uses.
    fn submit_workflow_launch(&mut self) {
        if self.state.view.workflow_launch.submitting {
            return;
        }
        let params = match launch_run_params(&self.state.view.workflow_launch) {
            Ok(params) => params,
            Err(refusal) => {
                let launch = &mut self.state.view.workflow_launch;
                launch.error = Some(refusal.message());
                if let LaunchRefusal::MissingArg { index, .. } = refusal {
                    launch.focus = WorkflowLaunchFocus::Arg(index);
                }
                return;
            }
        };

        self.state.view.workflow_launch.submitting = true;
        let response =
            self.dispatch_runtime_mutation("tui.workflow.run", Method::WorkflowRun(params));
        self.state.view.workflow_launch.submitting = false;
        if let Some(message) = error_message(&response) {
            // The envelope is the only place the TUI learns the run was
            // refused; dropping it is what would make the run button the one
            // action in the launcher that can fail in silence.
            self.state.view.workflow_launch.error = Some(message);
            return;
        }

        // A started run is one the user wants to watch, and the overlay is
        // where they watch it. A lead run is loaded from the store (it has no
        // mirrored graph); an engine-era mirror still wins if one is somehow
        // there. Falling back to `leave_modal` keeps the exit honest when there
        // is nothing to show.
        if self.open_workflow_dag_on_the_live_run() || self.state.workflow_run_graph().is_some() {
            self.state.mode = Mode::WorkflowDag;
        } else {
            leave_modal(&mut self.state);
        }
        self.state.view.workflow_launch = WorkflowLaunchState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, tier: Tier) -> WorkflowLaunchEntry {
        WorkflowLaunchEntry {
            workflow_id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            default_tier: Some(tier),
        }
    }

    fn arg(name: &str, required: bool) -> WorkflowLaunchArg {
        WorkflowLaunchArg {
            name: name.to_string(),
            description: String::new(),
            required,
            value: String::new(),
        }
    }

    fn state_with_form() -> AppState {
        let mut state = AppState::test_new();
        state.mode = Mode::WorkflowLaunch;
        state.view.workflow_launch = WorkflowLaunchState {
            workflows: vec![
                entry("workflow:1", Tier::High),
                entry("workflow:2", Tier::Low),
            ],
            args: vec![arg("goal", true), arg("scope", false)],
            tier: Some(Tier::High),
            ..WorkflowLaunchState::default()
        };
        state
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_walks_the_ring_from_the_list_through_the_args_to_the_tier_row() {
        let mut state = state_with_form();

        for expected in [
            WorkflowLaunchFocus::Arg(0),
            WorkflowLaunchFocus::Arg(1),
            WorkflowLaunchFocus::Tier,
            WorkflowLaunchFocus::Confirm,
            WorkflowLaunchFocus::Workflows,
        ] {
            assert_eq!(
                apply_workflow_launch_key(&mut state, key(KeyCode::Tab)),
                LaunchIntent::None
            );
            assert_eq!(state.view.workflow_launch.focus, expected);
        }

        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::BackTab)),
            LaunchIntent::None
        );
        assert_eq!(
            state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Confirm
        );
    }

    #[test]
    fn arrows_move_the_selection_in_the_list_and_the_focus_everywhere_else() {
        let mut state = state_with_form();

        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Down)),
            LaunchIntent::SelectionChanged
        );
        assert_eq!(state.view.workflow_launch.selected, 1);

        state.view.workflow_launch.focus = WorkflowLaunchFocus::Arg(0);
        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Down)),
            LaunchIntent::None
        );
        assert_eq!(
            state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Arg(1)
        );
        assert_eq!(
            state.view.workflow_launch.selected, 1,
            "the list selection does not move while an arg has focus"
        );
    }

    #[test]
    fn workflow_launcher_list_moves_on_j_and_k() {
        let mut state = state_with_form();

        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Char('j'))),
            LaunchIntent::SelectionChanged
        );
        assert_eq!(state.view.workflow_launch.selected, 1);

        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Char('k'))),
            LaunchIntent::SelectionChanged
        );
        assert_eq!(state.view.workflow_launch.selected, 0);
    }

    #[test]
    fn workflow_launcher_arg_field_still_types_j_and_k() {
        let mut state = state_with_form();
        state.view.workflow_launch.focus = WorkflowLaunchFocus::Arg(0);

        apply_workflow_launch_key(&mut state, key(KeyCode::Char('j')));
        apply_workflow_launch_key(&mut state, key(KeyCode::Char('k')));
        assert_eq!(state.view.workflow_launch.args[0].value, "jk");
        assert_eq!(
            state.view.workflow_launch.selected, 0,
            "typing into an arg field must not move the list selection"
        );
    }

    #[test]
    fn typing_edits_only_the_focused_arg_line() {
        let mut state = state_with_form();
        state.view.workflow_launch.focus = WorkflowLaunchFocus::Arg(0);

        for character in "dark mode".chars() {
            apply_workflow_launch_key(&mut state, key(KeyCode::Char(character)));
        }
        assert_eq!(state.view.workflow_launch.args[0].value, "dark mode");
        assert_eq!(state.view.workflow_launch.args[1].value, "");

        apply_workflow_launch_key(&mut state, key(KeyCode::Backspace));
        assert_eq!(state.view.workflow_launch.args[0].value, "dark mod");
        apply_workflow_launch_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(state.view.workflow_launch.args[0].value, "");

        // With the list focused there is no text target, so a character key is
        // not silently swallowed into some other field.
        state.view.workflow_launch.focus = WorkflowLaunchFocus::Workflows;
        apply_workflow_launch_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.view.workflow_launch.args[0].value, "");
    }

    #[test]
    fn the_tier_row_cycles_and_is_what_reaches_the_start_call() {
        let mut state = state_with_form();
        state.view.workflow_launch.focus = WorkflowLaunchFocus::Tier;
        state.view.workflow_launch.args[0].value = "ship it".into();

        // auto · max · high · medium · low, starting from the seeded `high`.
        apply_workflow_launch_key(&mut state, key(KeyCode::Right));
        assert_eq!(state.view.workflow_launch.tier, Some(Tier::Medium));
        apply_workflow_launch_key(&mut state, key(KeyCode::Left));
        apply_workflow_launch_key(&mut state, key(KeyCode::Left));
        assert_eq!(state.view.workflow_launch.tier, Some(Tier::Max));

        let params = launch_run_params(&state.view.workflow_launch).expect("the form is complete");
        assert_eq!(params.tier, Some(WorkflowTier::Max));
        assert_eq!(params.workflow_id, "workflow:1");
        assert_eq!(params.args.get("goal").map(String::as_str), Some("ship it"));
        assert!(
            !params.args.contains_key("scope"),
            "an empty optional arg is not sent as an empty string"
        );
    }

    #[test]
    fn an_unseeded_tier_row_falls_back_to_the_workflows_default_tier() {
        let mut state = state_with_form();
        state.view.workflow_launch.tier = None;
        state.view.workflow_launch.args[0].value = "ship it".into();

        let params = launch_run_params(&state.view.workflow_launch).expect("the form is complete");
        assert_eq!(params.tier, Some(WorkflowTier::High));
    }

    #[test]
    fn a_required_defaultless_arg_gates_the_run() {
        let state = state_with_form();

        assert_eq!(
            launch_run_params(&state.view.workflow_launch),
            Err(LaunchRefusal::MissingArg {
                index: 0,
                name: "goal".to_string(),
            })
        );

        let mut filled = state;
        filled.view.workflow_launch.args[0].value = "   ".into();
        assert!(
            launch_run_params(&filled.view.workflow_launch).is_err(),
            "whitespace is not an answer"
        );
        filled.view.workflow_launch.args[0].value = "add dark mode".into();
        assert!(launch_run_params(&filled.view.workflow_launch).is_ok());
    }

    #[test]
    fn an_empty_launcher_cannot_be_confirmed() {
        let mut state = AppState::test_new();
        state.mode = Mode::WorkflowLaunch;
        assert_eq!(
            launch_run_params(&state.view.workflow_launch),
            Err(LaunchRefusal::NoWorkflow)
        );
    }

    #[test]
    fn escape_asks_to_close_and_enter_asks_to_confirm() {
        let mut state = state_with_form();
        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Esc)),
            LaunchIntent::Close
        );
        assert_eq!(
            apply_workflow_launch_key(&mut state, key(KeyCode::Enter)),
            LaunchIntent::Confirm
        );
    }

    #[test]
    fn pasted_text_lands_in_the_focused_arg_and_nowhere_else() {
        let mut state = state_with_form();
        assert!(!insert_workflow_launch_text(&mut state, "hello"));

        state.view.workflow_launch.focus = WorkflowLaunchFocus::Arg(1);
        assert!(insert_workflow_launch_text(&mut state, "docs\nonly"));
        assert_eq!(state.view.workflow_launch.args[1].value, "docsonly");
    }

    // ── the runtime half ────────────────────────────────────────────────────

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg_attr(not(feature = "workflow"), allow(unused_mut))]
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        // Never let a unit test open — or lock — the user's real workflow
        // database. With the feature off there is no store to redirect.
        #[cfg(feature = "workflow")]
        {
            app.workflow_store = crate::app::workflow_store::WorkflowStoreHandle::in_memory();
        }
        app
    }

    /// Geometry the way the view-computation pass would have stored it, so the
    /// mouse tests exercise the hit-test rather than the layout.
    fn launch_view_with_geometry() -> WorkflowLaunchState {
        use ratatui::layout::Rect;

        WorkflowLaunchState {
            workflows: vec![
                entry("workflow:1", Tier::High),
                entry("workflow:2", Tier::Low),
            ],
            args: vec![arg("goal", true)],
            tier: Some(Tier::High),
            modal_rect: Rect::new(10, 5, 60, 20),
            list_rect: Rect::new(11, 8, 58, 2),
            workflow_rects: vec![Rect::new(11, 8, 58, 1), Rect::new(11, 9, 58, 1)],
            arg_rects: vec![Rect::new(11, 12, 58, 1)],
            tier_rects: crate::ui::LAUNCH_TIERS
                .iter()
                .enumerate()
                .map(|(index, _)| Rect::new(11 + index as u16 * 6, 16, 6, 1))
                .collect(),
            button_rects: vec![Rect::new(20, 22, 8, 1), Rect::new(30, 22, 12, 1)],
            ..WorkflowLaunchState::default()
        }
    }

    fn click(app: &mut App, column: u16, row: u16) -> bool {
        app.handle_workflow_launch_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn clicks_land_on_exactly_the_rects_the_frame_drew() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowLaunch;
        app.state.view.workflow_launch = launch_view_with_geometry();

        assert!(click(&mut app, 20, 9), "the modal swallows the event");
        assert_eq!(app.state.view.workflow_launch.selected, 1);
        assert_eq!(
            app.state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Workflows
        );

        app.state.view.workflow_launch = launch_view_with_geometry();
        click(&mut app, 20, 12);
        assert_eq!(
            app.state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Arg(0)
        );

        // The fourth tier button is `medium`, and clicking it both picks the
        // tier and moves focus to the row.
        click(&mut app, 11 + 3 * 6, 16);
        assert_eq!(app.state.view.workflow_launch.tier, Some(Tier::Medium));
        assert_eq!(
            app.state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Tier
        );

        // A click inside the modal but on no target changes nothing.
        let before = app.state.view.workflow_launch.clone();
        click(&mut app, 11, 6);
        assert_eq!(app.state.view.workflow_launch, before);
    }

    #[test]
    fn cancel_and_a_click_outside_both_close_the_launcher() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowLaunch;
        app.state.view.workflow_launch = launch_view_with_geometry();
        click(&mut app, 31, 22);
        assert_ne!(app.state.mode, Mode::WorkflowLaunch);

        app.state.mode = Mode::WorkflowLaunch;
        app.state.view.workflow_launch = launch_view_with_geometry();
        click(&mut app, 0, 0);
        assert_ne!(app.state.mode, Mode::WorkflowLaunch);
    }

    #[test]
    fn a_server_with_no_workflows_does_not_open_an_empty_launcher() {
        let mut app = test_app();
        assert!(!app.open_workflow_launcher());
        assert_ne!(app.state.mode, Mode::WorkflowLaunch);
    }

    #[cfg(feature = "workflow")]
    fn create_workflow(app: &mut App, name: &str, default_tier: &str) -> String {
        let response = app.dispatch_api_request(
            "test.workflow.create",
            Method::WorkflowCreate(crate::api::schema::WorkflowCreateParams {
                definition: crate::api::schema::WorkflowDefinitionDocument {
                    format: crate::api::schema::WorkflowDefinitionFormat::Toml,
                    text: format!(
                        r#"
name = "{name}"
description = "a test workflow"
default_tier = "{default_tier}"

[[arg]]
name = "goal"
required = true

[[arg]]
name = "scope"
default = "everything"

[[node]]
key = "plan"
label = "Plan"
runner = "command"
command = ["/bin/true"]
prompt_template = "plan {{{{goal}}}} within {{{{scope}}}}"
output_schema = {{ type = "object" }}
"#
                    ),
                },
            }),
        );
        let Some(ResponseResult::WorkflowCreated { workflow, .. }) = success_result(&response)
        else {
            panic!("the workflow was created: {response}");
        };
        workflow.workflow_id
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn the_launcher_lists_workflows_and_seeds_the_tier_row_from_default_tier() {
        let mut app = test_app();
        create_workflow(&mut app, "ship-feature", "low");

        assert!(app.open_workflow_launcher());
        assert_eq!(app.state.mode, Mode::WorkflowLaunch);
        let launch = &app.state.view.workflow_launch;
        assert_eq!(launch.workflows.len(), 1);
        assert_eq!(launch.workflows[0].name, "ship-feature");
        assert_eq!(launch.tier, Some(Tier::Low), "seeded from default_tier");
        // The declared args arrive as lines, defaults pre-filled, and only the
        // required-and-defaultless one gates confirm.
        let names: Vec<&str> = launch.args.iter().map(|arg| arg.name.as_str()).collect();
        assert_eq!(names, vec!["goal", "scope"]);
        assert!(launch.args[0].required);
        assert_eq!(launch.args[0].value, "");
        assert!(!launch.args[1].required);
        assert_eq!(launch.args[1].value, "everything");
    }

    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn confirm_is_refused_until_the_required_arg_is_filled_then_starts_the_run() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature", "high");
        assert!(app.open_workflow_launcher());

        app.handle_workflow_launch_key(key(KeyCode::Enter));
        assert_eq!(
            app.state.mode,
            Mode::WorkflowLaunch,
            "an incomplete form keeps the modal open"
        );
        assert_eq!(
            app.state.view.workflow_launch.error.as_deref(),
            Some("goal is required")
        );
        assert_eq!(
            app.state.view.workflow_launch.focus,
            WorkflowLaunchFocus::Arg(0),
            "focus moves to the field that refused"
        );

        for character in "dark mode".chars() {
            app.handle_workflow_launch_key(key(KeyCode::Char(character)));
        }
        app.state.view.workflow_launch.tier = Some(Tier::Low);
        app.handle_workflow_launch_key(key(KeyCode::Enter));

        assert_ne!(app.state.mode, Mode::WorkflowLaunch, "the run started");
        assert_eq!(
            app.state.view.workflow_launch,
            WorkflowLaunchState::default(),
            "the form is not left behind for the next open"
        );

        // The tier the row was on is the tier the run was started with.
        let listed = app.dispatch_api_request(
            "test.workflow.run.list",
            Method::WorkflowRunList(crate::api::schema::WorkflowRunListParams {
                workflow_id: Some(workflow_id),
                limit: None,
            }),
        );
        let listed: serde_json::Value =
            serde_json::from_str(&listed).expect("the response is json");
        assert_eq!(
            listed["result"]["runs"][0]["tier"], "low",
            "the launcher's tier reached the start call: {listed}"
        );
    }
}
