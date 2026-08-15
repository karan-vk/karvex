//! The review cycle's two documents: what the interviewer is told, what the
//! synthesiser is told, and how their answers parse back.
//!
//! `phase4-retarget-plan.md` §3.5. Render and parse live in one file, the
//! [`crate::workflow::lead_prompt`] precedent (`task_subject` and
//! `subject_node_key` are two halves of one contract and share a module):
//! these prompts *ask* for a shape, and the parser here is the only definition
//! of that shape. There is no output-schema validator any more — a malformed
//! answer is refused by `kvx`, with the reason printed in the agent's own
//! pane, and the agent fixes it and retries. The parser is therefore the
//! schema, and it must say which field it choked on.
//!
//! Two rules run through every string below.
//!
//! * **Every path is absolute.** The interview pane's cwd is the cycle
//!   directory and the member's project directory is only `--add-dir`'d
//!   (spike S2 amendment 3), so a relative path in a prompt resolves somewhere
//!   the agent did not mean. Same rule the watchdog's messages follow.
//! * **An evidence-only interview never speaks in the teammate's voice.** The
//!   opener says so, the questions are re-worded to the third person, and the
//!   synthesis document repeats it per block. `workflow::review`'s
//!   [`crate::workflow::review::Attribution`] enforces it on the way to the
//!   store, because a prompt is a request; but a request that contradicts the
//!   enforcement would just teach the agent to write dishonest prose that
//!   karvex then labels honestly, which helps nobody.
//!
//! Pure by construction: values in, `String`/`Result` out. No filesystem, no
//! store, no runtime.

use std::fmt;
use std::fmt::Write as _;

use crate::workflow::model::{InterviewMode, NodeKey, NodeStatus};
use crate::workflow::review::{
    format_duration_ms, EvidenceOnlyReason, FindingLevel, FindingVerdict, MemberAttribution,
    MemberEvidence, ParsedFinding, RunEvidence, TaskEvidence,
};

/// Bumped whenever either rendered document changes in a way an interviewer or
/// a synthesiser would behave differently for. Rendered into the documents
/// themselves so a stored cycle says which contract produced it, and asserted
/// by the render tests so a template change is never silent — exactly what
/// [`crate::workflow::lead_prompt::LEAD_PROMPT_VERSION`] does for the run.
pub const REVIEW_PROMPT_VERSION: u32 = 1;

/// The five fixed answer keys (`00-overview.md` Feature 4). Fixed means fixed:
/// the parser requires all five, the renderer asks all five in this order, and
/// a sixth is a contract change with a version bump.
pub const INTERVIEW_QUESTION_KEYS: [&str; 5] = [
    "account",
    "what_happened",
    "blockers",
    "upstream_gaps",
    "brief_changes",
];

/// Per-field budget on an interview answer, in characters.
///
/// A cap exists because the synthesis document concatenates every answer and a
/// runaway field would crowd out the other interviews. It is generous on
/// purpose: refusing a thoughtful answer for being long would be a worse
/// failure than a long prompt.
pub const ANSWER_FIELD_MAX_CHARS: usize = 4000;

/// Per-finding budget on the `rationale` field, in characters.
pub const RATIONALE_MAX_CHARS: usize = 4000;

// ── the interview document ─────────────────────────────────────────────────

/// One question, as the two modes phrase it.
struct Question {
    key: &'static str,
    /// Asked of the teammate itself, in a resumed interview.
    first_person: &'static str,
    /// Asked of a reader of the record, in an evidence-only interview.
    third_person: &'static str,
}

const QUESTIONS: [Question; 5] = [
    Question {
        key: "account",
        first_person: "What were you asked to do, as you understood it at the time?",
        third_person: "What was this member asked to do, as far as the record shows?",
    },
    Question {
        key: "what_happened",
        first_person: "What did you actually do? Walk through it in order.",
        third_person: "What did this member actually do, in order, according to the record?",
    },
    Question {
        key: "blockers",
        first_person: "What blocked you, and for how long?",
        third_person: "What blocked this member, and for how long, according to the record?",
    },
    Question {
        key: "upstream_gaps",
        first_person: "What did you need from upstream — the lead, another teammate, the \
                       plan — and not get?",
        third_person: "What did this member need from upstream — the lead, another teammate, \
                       the plan — and not get?",
    },
    Question {
        key: "brief_changes",
        first_person: "What would you change about your own brief so the next agent doing \
                       this task does better?",
        third_person: "What should change about this member's brief so the next agent doing \
                       this task does better?",
    },
];

/// Everything the interview document needs. Borrowed, so the adapter renders
/// from the evidence it already loaded.
#[derive(Debug)]
pub struct InterviewPromptInput<'a> {
    pub member: &'a str,
    pub is_lead: bool,
    pub mode: InterviewMode,
    /// Why the mode is evidence-only. Rendered verbatim into the opener, so
    /// the reader is told what karvex could not do rather than left to guess.
    pub evidence_only_reason: Option<EvidenceOnlyReason>,
    pub run: &'a RunEvidence,
    /// The member's measured record. `None` when karvex has no task evidence
    /// for it at all — which is itself something to ask about.
    pub evidence: Option<&'a MemberEvidence>,
    /// Absolute path of the cycle directory (the interview pane's cwd).
    pub cycle_dir: &'a str,
    /// Absolute path the answer JSON must be written to.
    pub answer_path: &'a str,
}

/// Renders the interview document: karvex's measured record put *to* the
/// teammate, then the five questions, then how to answer.
///
/// The measured half comes first on purpose. `08-phase4-plan.md` got exactly
/// one thing unambiguously right about this feature — putting the numbers to
/// the agent is what makes the answers worth reading, because an agent asked
/// "how did it go?" writes a press release and an agent shown "you sat idle
/// for 14m 22s while your task was in progress, and here is the nudge we sent
/// you" has something to explain.
pub fn render_interview_prompt(input: &InterviewPromptInput<'_>) -> String {
    let mut out = String::with_capacity(4096);
    let evidence_only = matches!(input.mode, InterviewMode::EvidenceOnly);

    let _ = writeln!(out, "# Review interview: {}", input.member);
    out.push('\n');
    render_interview_opener(&mut out, input, evidence_only);
    render_run_header(
        &mut out,
        input.run,
        input.member,
        input.is_lead,
        evidence_only,
    );
    render_measured_record(&mut out, input, evidence_only);
    render_questions(&mut out, evidence_only);
    render_answer_contract(&mut out, input, evidence_only);
    out
}

fn render_interview_opener(
    out: &mut String,
    input: &InterviewPromptInput<'_>,
    evidence_only: bool,
) {
    if evidence_only {
        out.push_str(
            "**You are reviewing evidence, not resuming the session.** You are not \
             this teammate and you do not remember its run. Everything you know about it \
             is in this document, which karvex measured from the outside.\n",
        );
        out.push('\n');
        let _ = writeln!(
            out,
            "Answer as an outside reader of the record, about `{}` in the third person. \
             Do not write in its voice, do not say \"I did\", and do not invent an intention \
             the record does not show. Where the record is silent, say it is silent — that \
             is a useful answer and a fabricated one is not.",
            input.member,
        );
        if let Some(reason) = input.evidence_only_reason {
            out.push('\n');
            let _ = writeln!(
                out,
                "Karvex wanted the teammate's own account and could not get it: {}.",
                reason.sentence(),
            );
        }
        out.push('\n');
        out.push_str(
            "Your answers will be recorded as `interview_mode: \"evidence_only\"` and will \
             never be presented as this teammate's own words.\n",
        );
    } else {
        let _ = writeln!(
            out,
            "**This is your own session, resumed.** You are `{}` from the karvex workflow \
             run below, revived with `claude --resume … --fork-session`. This is a fork: \
             your original transcript is untouched and stays valid evidence, and nothing you \
             do here changes it.",
            input.member,
        );
        out.push('\n');
        out.push_str(
            "Karvex is interviewing you about that run. This is a two-party interview, not \
             a performance review: karvex measured what it could see from the outside, that \
             record is below, and you are the only one who can say what it felt like from \
             the inside. Where karvex's numbers are wrong or misleading, say so — a \
             correction is one of the most valuable answers you can give.\n",
        );
    }
    out.push('\n');
    let _ = writeln!(out, "- Review prompt contract: `v{REVIEW_PROMPT_VERSION}`");
    let _ = writeln!(
        out,
        "- Interview mode: `{}`",
        match input.mode {
            InterviewMode::Resumed => "resumed",
            InterviewMode::EvidenceOnly => "evidence_only",
        }
    );
    out.push('\n');
}

fn render_run_header(
    out: &mut String,
    run: &RunEvidence,
    member: &str,
    is_lead: bool,
    evidence_only: bool,
) {
    out.push_str("## The run\n\n");
    let _ = writeln!(out, "- Workflow: `{}`", run.workflow_name);
    let _ = writeln!(out, "- Run id: `{}`", run.run_id.as_str());
    let _ = writeln!(out, "- Definition version: `v{}`", run.kvdag_version);
    let _ = writeln!(out, "- Final run status: `{}`", run_status_str(run));
    if let Some(ended) = run.ended_at_unix_ms {
        let _ = writeln!(
            out,
            "- Wall time: {}",
            format_duration_ms(ended.saturating_sub(run.started_at_unix_ms)),
        );
    }
    // The second person is reserved for a resumed interview. An evidence-only
    // reader is not this teammate, and a prompt that says "your task" to it is
    // the first step towards an answer written in a voice it has no right to.
    let (role_label, name_label) = if evidence_only {
        (
            "Role on the team of the member under review",
            "Member under review",
        )
    } else {
        ("Your role on the team", "Your member name")
    };
    let _ = writeln!(
        out,
        "- {role_label}: {}",
        if is_lead { "team lead" } else { "teammate" },
    );
    let _ = writeln!(out, "- {name_label}: `{member}`");
    if let Some(failure) = &run.failure {
        let _ = writeln!(out, "- Recorded failure: {failure}");
    }
    out.push('\n');
    if let Some(summary) = &run.summary {
        out.push_str("The lead's own end-of-run summary said:\n\n");
        for line in summary.trim_end().lines() {
            let _ = writeln!(out, "> {line}");
        }
        out.push('\n');
    }
}

fn render_measured_record(out: &mut String, input: &InterviewPromptInput<'_>, evidence_only: bool) {
    out.push_str("## What karvex measured\n\n");
    out.push_str(
        "Karvex did not schedule this run and did not read anyone's mind. It watched the \
         team's shared task list and each pane's own reported state, and this is the whole \
         of what it recorded. Numbers are measurements, not accusations.\n\n",
    );

    let Some(evidence) = input.evidence else {
        let _ = writeln!(
            out,
            "Karvex has **no task evidence at all** for `{}`: it never saw this member claim \
             or change a task. That is itself one of the things this interview is for.",
            input.member,
        );
        out.push('\n');
        return;
    };

    if evidence.tasks.is_empty() {
        let _ = writeln!(
            out,
            "Karvex saw `{}` on the team but never saw it own a task.",
            input.member,
        );
        out.push('\n');
    }

    for (index, task) in evidence.tasks.iter().enumerate() {
        render_task_evidence(out, index + 1, task, input.member, evidence_only);
    }

    if let Some(state) = &evidence.last_state {
        let _ = writeln!(out, "Last observed pane state: `{state}`.");
        out.push('\n');
    }
}

fn render_task_evidence(
    out: &mut String,
    index: usize,
    task: &TaskEvidence,
    member: &str,
    evidence_only: bool,
) {
    let _ = writeln!(out, "### {index}. {}", task.subject);
    out.push('\n');
    match &task.node_key {
        Some(key) => {
            let _ = writeln!(
                out,
                "- Planned task, definition node `{}` (instance `{}`)",
                key.as_str(),
                task.path.as_str(),
            );
        }
        None => {
            let _ = writeln!(
                out,
                "- Emergent task — not in the plan karvex gave the lead (instance `{}`)",
                task.path.as_str(),
            );
        }
    }
    let _ = writeln!(out, "- Final status: `{}`", node_status_str(task.status));
    if let Some(attention) = task.attention {
        let _ = writeln!(
            out,
            "- Karvex's watchdog flagged it as `{}`",
            attention.as_str(),
        );
    }
    let _ = writeln!(
        out,
        "- Time in progress: {} (of which the owning pane was idle for {})",
        format_duration_ms(task.in_progress_ms),
        format_duration_ms(task.idle_while_in_progress_ms),
    );
    if !task.unresolved_blockers.is_empty() {
        let _ = writeln!(
            out,
            "- Still waiting on when the run ended: {}",
            task.unresolved_blockers
                .iter()
                .map(|subject| format!("`{subject}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    if task.owner_changes.is_empty() {
        let _ = writeln!(out, "- Never changed hands");
    } else {
        out.push_str("- Changed hands:\n");
        for change in &task.owner_changes {
            let _ = writeln!(
                out,
                "  - from {} to {}",
                owner_str(change.from.as_deref()),
                owner_str(change.to.as_deref()),
            );
        }
        out.push_str(
            "  (karvex can see that a task was reassigned; it cannot see why the lead did \
             it.)\n",
        );
    }

    if task.interventions.is_empty() {
        let _ = writeln!(out, "- Karvex's watchdog never intervened on this task");
    } else {
        let _ = writeln!(
            out,
            "- Karvex's watchdog intervened {} time(s):",
            task.interventions.len(),
        );
        for intervention in &task.interventions {
            let _ = writeln!(
                out,
                "  - rung {} (`{}`), {}",
                intervention.rung,
                intervention.kind,
                match (intervention.delivered, intervention.channel.as_deref()) {
                    (true, Some(channel)) => format!("delivered over `{channel}`"),
                    (true, None) => "delivered".to_string(),
                    (false, _) if evidence_only => {
                        "**never delivered** — karvex composed it and could not get it to \
                         the member, so it may never have seen it"
                            .to_string()
                    }
                    (false, _) => "**never delivered** — karvex composed it and could not \
                                   get it to you, so you may never have seen it"
                        .to_string(),
                },
            );
            if let Some(text) = &intervention.text {
                for line in text.trim_end().lines() {
                    let _ = writeln!(out, "    > {line}");
                }
            }
        }
    }
    // Only worth a line when the task did not end up where this interview is
    // pointed: "you own the task we are asking you about" is not information.
    if task.owner.as_deref() != Some(member) {
        let _ = writeln!(
            out,
            "- Owner karvex last recorded: {} (this interview is about `{member}`'s part in \
             it)",
            owner_str(task.owner.as_deref()),
        );
    }
    out.push('\n');
}

fn render_questions(out: &mut String, evidence_only: bool) {
    out.push_str("## The five questions\n\n");
    out.push_str(
        "Answer all five. Be specific and concrete: name tasks, files, and moments. \
         Disagreeing with the record above is allowed and wanted; saying \"the record is \
         wrong about X because Y\" is a better answer than agreeing politely.\n\n",
    );
    for question in &QUESTIONS {
        let text = if evidence_only {
            question.third_person
        } else {
            question.first_person
        };
        let _ = writeln!(out, "- `{}` — {}", question.key, text);
    }
    out.push('\n');
}

fn render_answer_contract(out: &mut String, input: &InterviewPromptInput<'_>, evidence_only: bool) {
    out.push_str("## How to answer\n\n");
    let _ = writeln!(
        out,
        "1. Write your answers as a single JSON object to `{}`. It must have exactly these \
         five string keys and nothing is optional:",
        input.answer_path,
    );
    out.push('\n');
    out.push_str("```json\n");
    out.push_str(&example_answer_json(evidence_only));
    out.push_str("```\n\n");
    let _ = writeln!(
        out,
        "2. Then run this from your own pane, exactly as written:\n\n   \
         `kvx workflow review answer --file {}`",
        input.answer_path,
    );
    out.push('\n');
    out.push_str(
        "`kvx` is on your PATH and `KARVEX_WORKFLOW_REVIEW_INTERVIEW` is already exported \
         in this pane, so the command identifies this interview by itself. If karvex refuses \
         the file it prints exactly which field is wrong — fix that field and run the \
         command again. Nothing else you write anywhere is collected: that one call is the \
         whole of your report.\n\n",
    );
    let _ = writeln!(
        out,
        "Each field is capped at {ANSWER_FIELD_MAX_CHARS} characters. Empty strings are \
         refused — if a question genuinely does not apply, say so in words. The cycle \
         directory `{}` is writable; nothing outside it needs to change.",
        input.cycle_dir,
    );
    out.push('\n');
}

/// The example the interview document embeds. It is a *valid* document —
/// [`parse_interview_answer`] accepts it — so the contract shown to the agent
/// and the contract karvex enforces cannot drift apart. The render tests parse
/// it back out of the rendered prompt.
fn example_answer_json(evidence_only: bool) -> String {
    let voice = if evidence_only {
        [
            "The record shows this member was asked to …",
            "According to the task list and the journal it …",
            "It appears to have been blocked by …",
            "The record shows it never received …",
            "The brief should have said …",
        ]
    } else {
        [
            "I understood my task as …",
            "I read …, then wrote …, then …",
            "I was blocked on … for about …",
            "I needed … from the lead and never got it",
            "My brief should have told me …",
        ]
    };
    let mut out = String::new();
    out.push_str("{\n");
    for (index, key) in INTERVIEW_QUESTION_KEYS.iter().enumerate() {
        let comma = if index + 1 == INTERVIEW_QUESTION_KEYS.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(out, "  \"{key}\": \"{}\"{comma}", voice[index]);
    }
    out.push_str("}\n");
    out
}

// ── the synthesis document ─────────────────────────────────────────────────

/// One interview as the synthesis document sees it: the member, what karvex is
/// allowed to claim about it, and the answer if there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesisSource {
    pub member: String,
    pub is_lead: bool,
    /// What karvex decided about attributability, from
    /// [`crate::workflow::review::ReviewCycleState::attribution`]. The
    /// synthesis prompt reads it and says so per block; it is not the
    /// synthesiser's to override.
    pub attribution: MemberAttribution,
    /// The parsed answer, when the interview produced one.
    pub answer: Option<InterviewAnswer>,
}

#[derive(Debug)]
pub struct SynthesisPromptInput<'a> {
    pub run: &'a RunEvidence,
    pub sources: &'a [SynthesisSource],
    /// The definition's node keys, so a finding can only be filed against a
    /// node that exists.
    pub node_keys: &'a [NodeKey],
    /// Absolute path of the cycle directory (the synthesis pane's cwd).
    pub cycle_dir: &'a str,
    /// Absolute path the findings JSON must be written to.
    pub findings_path: &'a str,
}

/// Renders the synthesis document: every collected answer, labelled with what
/// may be claimed about it, then the classification contract, then the findings
/// shape.
pub fn render_synthesis_prompt(input: &SynthesisPromptInput<'_>) -> String {
    let mut out = String::with_capacity(4096);
    let _ = writeln!(
        out,
        "# Review synthesis: {} run `{}`",
        input.run.workflow_name,
        input.run.run_id.as_str(),
    );
    out.push('\n');
    out.push_str(
        "You are synthesising a karvex workflow run's self-improvement review. Karvex \
         interviewed the team about a run that has already finished; below are the answers \
         it got and the record it measured. Your job is to turn that into a small set of \
         concrete findings about the workflow *definition*, each one classified and each one \
         grounded in something specific.\n",
    );
    out.push('\n');
    let _ = writeln!(out, "- Review prompt contract: `v{REVIEW_PROMPT_VERSION}`");
    let _ = writeln!(
        out,
        "- Definition version under review: `v{}`",
        input.run.kvdag_version
    );
    let _ = writeln!(out, "- Final run status: `{}`", run_status_str(input.run));
    out.push('\n');

    render_attribution_rules(&mut out);
    render_sources(&mut out, input);
    render_run_shape(&mut out, input);
    render_classification(&mut out);
    render_findings_contract(&mut out, input);
    out
}

fn render_attribution_rules(out: &mut String) {
    out.push_str("## Whose words are whose\n\n");
    out.push_str(
        "Each block below is labelled either **the member's own account** or \
         **evidence-only**. The difference is not a formality:\n\n",
    );
    out.push_str(
        "- *The member's own account* is a real answer from that agent's own resumed \
           session. You may attribute it to them and quote it.\n",
    );
    out.push_str(
        "- *Evidence-only* is an inference drawn from karvex's outside record by someone \
           who is not that agent. You must never write it as that agent's words, never \
           quote it as testimony, and never say \"X said\" about it.\n",
    );
    out.push('\n');
    out.push_str(
        "Karvex records the attribution of every finding itself, from what actually \
         happened during the interview rather than from what you write, so an evidence-only \
         source produces an `evidence_only` finding no matter how it is phrased. Name the \
         source you used in `source_member` anyway: it is what lets a human reading the \
         finding later check it against the right transcript.\n\n",
    );
}

fn render_sources(out: &mut String, input: &SynthesisPromptInput<'_>) {
    out.push_str("## The interviews\n\n");
    if input.sources.is_empty() {
        out.push_str(
            "No interviews were collected at all. Work from the run record below alone, and \
             attribute nothing to anyone.\n\n",
        );
        return;
    }
    for source in input.sources {
        let _ = writeln!(
            out,
            "### `{}`{}",
            source.member,
            if source.is_lead { " (team lead)" } else { "" },
        );
        out.push('\n');
        match source.attribution.mode {
            InterviewMode::Resumed => {
                let _ = writeln!(
                    out,
                    "**This is `{}`'s own account**, from its own resumed session.",
                    source.member,
                );
            }
            InterviewMode::EvidenceOnly => {
                let _ = writeln!(
                    out,
                    "**Evidence-only — these are NOT `{}`'s words.** Karvex could not take \
                     this member's own account: {}. Treat everything below as one reader's \
                     inference about `{}`, never as its testimony.",
                    source.member,
                    source
                        .attribution
                        .reason
                        .map(EvidenceOnlyReason::sentence)
                        .unwrap_or("karvex could not reach the teammate's own session"),
                    source.member,
                );
            }
        }
        out.push('\n');
        match &source.answer {
            Some(answer) => {
                for (key, value) in answer.fields() {
                    let _ = writeln!(out, "- **`{key}`**");
                    for line in value.trim_end().lines() {
                        let _ = writeln!(out, "  > {line}");
                    }
                }
            }
            None => {
                out.push_str(
                    "No answer was collected from this interview at all. There is nothing \
                     here to attribute to anyone.\n",
                );
            }
        }
        out.push('\n');
    }
}

fn render_run_shape(out: &mut String, input: &SynthesisPromptInput<'_>) {
    out.push_str("## What karvex measured\n\n");
    let run = input.run;
    if let Some(failure) = &run.failure {
        let _ = writeln!(out, "The run recorded a failure: {failure}\n");
    }
    for member in &run.members {
        let _ = writeln!(
            out,
            "- `{}` — {} task(s), {} watchdog intervention(s), highest rung {}, idle while \
             in progress {}, {} reassignment(s), {} emergent task(s), {} unfinished",
            member.name,
            member.tasks.len(),
            member.interventions(),
            member.highest_rung(),
            format_duration_ms(member.idle_while_in_progress_ms()),
            member.owner_changes(),
            member.emergent_tasks(),
            member.unfinished_tasks(),
        );
    }
    if !run.unowned_tasks.is_empty() {
        let _ = writeln!(
            out,
            "- {} task(s) nobody ever claimed: {}",
            run.unowned_tasks.len(),
            run.unowned_tasks
                .iter()
                .map(|task| format!("`{}`", task.subject))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    out.push('\n');
    if input.node_keys.is_empty() {
        out.push_str(
            "The definition under review declares no nodes, so there is nothing to file a \
             finding against.\n\n",
        );
    } else {
        let _ = writeln!(
            out,
            "Findings may only be filed against these definition nodes: {}.",
            input
                .node_keys
                .iter()
                .map(|key| format!("`{}`", key.as_str()))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }
}

fn render_classification(out: &mut String) {
    out.push_str("## Classify every finding\n\n");
    out.push_str(
        "Each finding is either prompt-level or structural. Choosing correctly matters \
         because karvex compiles them differently: a `prompt` finding rewrites a node's \
         brief, a `structural` one changes the shape of the work.\n\n",
    );
    out.push_str(
        "- `prompt` — the node's brief was wrong. Wrong wording, missing context, missing \
           acceptance criteria, the wrong role. The right agent, badly briefed. Merges \
           `prompt_template` and/or `role`.\n",
    );
    out.push_str(
        "- `structural` — the plan was wrong. The task was too big, too small, needed a \
           different capability tier, needed a longer budget, or should not exist. Merges \
           `demand`, `timeout_ms`, `max_attempts`.\n",
    );
    out.push('\n');
    out.push_str("Then give each finding a verdict:\n\n");
    out.push_str(
        "- `keep` — this node is fine. Say so when you looked and found nothing; \"we \
           checked and it was fine\" is evidence, and a review that only ever complains is \
           not a review.\n",
    );
    out.push_str("- `improve` — change it as described in `proposed_change`.\n");
    out.push_str(
        "- `replace` — this role should be fired and replaced. A `replace` finding **must** \
           carry a complete `replacement` object; karvex refuses the whole document \
           otherwise, so do not report one you cannot write out in full.\n",
    );
    out.push('\n');
    out.push_str(
        "Ground every finding in something specific: a measured number above, or a named \
         answer. A finding whose rationale could have been written without reading any of \
         this is noise. Reporting no findings at all is a legitimate outcome — report an \
         empty list rather than inventing one.\n\n",
    );
}

fn render_findings_contract(out: &mut String, input: &SynthesisPromptInput<'_>) {
    out.push_str("## How to report\n\n");
    let _ = writeln!(
        out,
        "1. Write your findings as a single JSON object to `{}`:",
        input.findings_path,
    );
    out.push('\n');
    out.push_str("```json\n");
    out.push_str(EXAMPLE_FINDINGS_JSON);
    out.push_str("```\n\n");
    let _ = writeln!(
        out,
        "2. Then run this from your own pane, exactly as written:\n\n   \
         `kvx workflow review report --file {}`",
        input.findings_path,
    );
    out.push('\n');
    out.push_str(
        "`kvx` is on your PATH and karvex identifies the cycle from this pane's own \
         environment. If karvex refuses the file it prints exactly which finding and which \
         field is wrong — fix it and run the command again.\n\n",
    );
    let _ = writeln!(
        out,
        "`node_key` must be one of the definition nodes listed above. `level` is `prompt` \
         or `structural`; `verdict` is `keep`, `improve`, or `replace`; `rationale` is \
         capped at {RATIONALE_MAX_CHARS} characters. `source_member` is the interview you \
         drew the finding from, or omitted when it came from the measured record alone. \
         `evidence` and `proposed_change` are free-form objects. The cycle directory `{}` is \
         writable; nothing outside it needs to change.",
        input.cycle_dir,
    );
    out.push('\n');
}

/// The example the synthesis document embeds — valid by construction, parsed
/// back out of the rendered prompt by the render tests, and deliberately
/// covering the one conditional the parser enforces (`replace` carries a
/// `replacement`).
const EXAMPLE_FINDINGS_JSON: &str = r#"{
  "findings": [
    {
      "node_key": "research",
      "source_member": "research",
      "level": "prompt",
      "verdict": "improve",
      "rationale": "The brief never said which sources counted, so the teammate spent 12m on the wrong ones.",
      "evidence": {"idle_while_in_progress_ms": 742000, "watchdog_interventions": 2},
      "proposed_change": {"prompt_template": "…the rewritten brief…"}
    },
    {
      "node_key": "verify",
      "level": "structural",
      "verdict": "replace",
      "rationale": "Verification never completed in three runs; the role is wrong for the task.",
      "evidence": {"unfinished_tasks": 1},
      "proposed_change": {"note": "swap the role"},
      "replacement": {"key": "verify", "role": "…the full replacement role definition…"}
    }
  ]
}
"#;

// ── parsing back ───────────────────────────────────────────────────────────

/// One interview's answers: the five fixed keys, all present, all non-empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterviewAnswer {
    pub account: String,
    pub what_happened: String,
    pub blockers: String,
    pub upstream_gaps: String,
    pub brief_changes: String,
}

impl InterviewAnswer {
    /// The five fields in question order, so every consumer renders them the
    /// same way.
    pub fn fields(&self) -> [(&'static str, &str); 5] {
        [
            ("account", self.account.as_str()),
            ("what_happened", self.what_happened.as_str()),
            ("blockers", self.blockers.as_str()),
            ("upstream_gaps", self.upstream_gaps.as_str()),
            ("brief_changes", self.brief_changes.as_str()),
        ]
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields()
            .into_iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }
}

/// Why an interview answer was refused. Every variant names the field, because
/// this text is printed in the agent's pane and it is the only correction it
/// gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerParseError {
    NotJson {
        detail: String,
    },
    NotAnObject,
    MissingField {
        field: &'static str,
    },
    NotAString {
        field: &'static str,
    },
    EmptyField {
        field: &'static str,
    },
    TooLong {
        field: &'static str,
        limit: usize,
        actual: usize,
    },
}

impl AnswerParseError {
    /// The field the refusal is about, when it is about one.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::NotJson { .. } | Self::NotAnObject => None,
            Self::MissingField { field }
            | Self::NotAString { field }
            | Self::EmptyField { field }
            | Self::TooLong { field, .. } => Some(field),
        }
    }
}

impl fmt::Display for AnswerParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson { detail } => write!(
                f,
                "the answer file is not valid JSON ({detail}). It must be a single JSON \
                 object with the five keys {}.",
                INTERVIEW_QUESTION_KEYS.join(", "),
            ),
            Self::NotAnObject => write!(
                f,
                "the answer file must be a JSON object with the five keys {}, not another \
                 kind of value.",
                INTERVIEW_QUESTION_KEYS.join(", "),
            ),
            Self::MissingField { field } => write!(
                f,
                "the answer is missing the required field `{field}`. All five of {} must be \
                 present.",
                INTERVIEW_QUESTION_KEYS.join(", "),
            ),
            Self::NotAString { field } => {
                write!(f, "the answer's `{field}` must be a string.")
            }
            Self::EmptyField { field } => write!(
                f,
                "the answer's `{field}` is empty. If the question does not apply, say so in \
                 words instead of leaving it blank.",
            ),
            Self::TooLong {
                field,
                limit,
                actual,
            } => write!(
                f,
                "the answer's `{field}` is {actual} characters, over the {limit} character \
                 limit. Shorten it.",
            ),
        }
    }
}

/// Parses one interview's answer document.
///
/// Strict about the five keys and forgiving about everything else: extra keys
/// are ignored rather than refused, because an agent adding a helpful
/// `notes` field should not have its whole interview rejected.
pub fn parse_interview_answer(raw: &str) -> Result<InterviewAnswer, AnswerParseError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| AnswerParseError::NotJson {
            detail: err.to_string(),
        })?;
    let object = value.as_object().ok_or(AnswerParseError::NotAnObject)?;

    let mut fields = [""; 5];
    for (index, key) in INTERVIEW_QUESTION_KEYS.iter().enumerate() {
        let entry = object
            .get(*key)
            .ok_or(AnswerParseError::MissingField { field: key })?;
        let text = entry
            .as_str()
            .ok_or(AnswerParseError::NotAString { field: key })?;
        if text.trim().is_empty() {
            return Err(AnswerParseError::EmptyField { field: key });
        }
        let length = text.chars().count();
        if length > ANSWER_FIELD_MAX_CHARS {
            return Err(AnswerParseError::TooLong {
                field: key,
                limit: ANSWER_FIELD_MAX_CHARS,
                actual: length,
            });
        }
        fields[index] = text;
    }

    Ok(InterviewAnswer {
        account: fields[0].trim().to_string(),
        what_happened: fields[1].trim().to_string(),
        blockers: fields[2].trim().to_string(),
        upstream_gaps: fields[3].trim().to_string(),
        brief_changes: fields[4].trim().to_string(),
    })
}

/// Why a findings document was refused. Every variant that can name a finding
/// names its index and its `node_key`, because "finding 2 is wrong" is
/// actionable and "the document is wrong" is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingsParseError {
    NotJson {
        detail: String,
    },
    /// Neither `{"findings": [...]}` nor a bare array.
    NotFindings,
    FindingsNotAnArray,
    NotAnObject {
        index: usize,
    },
    MissingField {
        index: usize,
        field: &'static str,
    },
    NotAString {
        index: usize,
        field: &'static str,
    },
    EmptyField {
        index: usize,
        field: &'static str,
    },
    TooLong {
        index: usize,
        field: &'static str,
        limit: usize,
        actual: usize,
    },
    UnknownLevel {
        index: usize,
        value: String,
    },
    UnknownVerdict {
        index: usize,
        value: String,
    },
    /// The one conditional the store cannot express as a column ASSERT and the
    /// `0001` table event turns into a DB error. Refused here instead, while
    /// the agent is still around to fix it.
    ReplacementMissing {
        index: usize,
        node_key: String,
    },
    ReplacementNotAnObject {
        index: usize,
        node_key: String,
    },
    /// `replacement` on a verdict that is not `replace`: karvex would silently
    /// carry a field it will never apply.
    ReplacementNotAllowed {
        index: usize,
        node_key: String,
        verdict: &'static str,
    },
}

impl FindingsParseError {
    pub fn index(&self) -> Option<usize> {
        match self {
            Self::NotJson { .. } | Self::NotFindings | Self::FindingsNotAnArray => None,
            Self::NotAnObject { index }
            | Self::MissingField { index, .. }
            | Self::NotAString { index, .. }
            | Self::EmptyField { index, .. }
            | Self::TooLong { index, .. }
            | Self::UnknownLevel { index, .. }
            | Self::UnknownVerdict { index, .. }
            | Self::ReplacementMissing { index, .. }
            | Self::ReplacementNotAnObject { index, .. }
            | Self::ReplacementNotAllowed { index, .. } => Some(*index),
        }
    }
}

impl fmt::Display for FindingsParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson { detail } => write!(
                f,
                "the findings file is not valid JSON ({detail}). It must be a JSON object \
                 with a `findings` array."
            ),
            Self::NotFindings => write!(
                f,
                "the findings file must be a JSON object with a `findings` array (a bare \
                 array is also accepted)."
            ),
            Self::FindingsNotAnArray => {
                write!(f, "`findings` must be an array of finding objects.")
            }
            Self::NotAnObject { index } => {
                write!(f, "finding {index} is not a JSON object.")
            }
            Self::MissingField { index, field } => {
                write!(
                    f,
                    "finding {index} is missing the required field `{field}`."
                )
            }
            Self::NotAString { index, field } => {
                write!(f, "finding {index}'s `{field}` must be a string.")
            }
            Self::EmptyField { index, field } => {
                write!(f, "finding {index}'s `{field}` is empty.")
            }
            Self::TooLong {
                index,
                field,
                limit,
                actual,
            } => write!(
                f,
                "finding {index}'s `{field}` is {actual} characters, over the {limit} \
                 character limit."
            ),
            Self::UnknownLevel { index, value } => write!(
                f,
                "finding {index}'s `level` is `{value}`; it must be `{}` or `{}`.",
                FindingLevel::Prompt.as_str(),
                FindingLevel::Structural.as_str(),
            ),
            Self::UnknownVerdict { index, value } => write!(
                f,
                "finding {index}'s `verdict` is `{value}`; it must be `{}`, `{}`, or `{}`.",
                FindingVerdict::Keep.as_str(),
                FindingVerdict::Improve.as_str(),
                FindingVerdict::Replace.as_str(),
            ),
            Self::ReplacementMissing { index, node_key } => write!(
                f,
                "finding {index} (`{node_key}`) has verdict `replace` but no `replacement`. \
                 Firing a role requires writing the replacement role definition out in full, \
                 in the same finding."
            ),
            Self::ReplacementNotAnObject { index, node_key } => write!(
                f,
                "finding {index} (`{node_key}`)'s `replacement` must be a JSON object \
                 describing the whole replacement node."
            ),
            Self::ReplacementNotAllowed {
                index,
                node_key,
                verdict,
            } => write!(
                f,
                "finding {index} (`{node_key}`) carries a `replacement` but its verdict is \
                 `{verdict}`. Only a `replace` verdict may carry one."
            ),
        }
    }
}

/// Parses the synthesiser's findings document.
///
/// Accepts `{"findings": [...]}` and a bare `[...]`, because both are what
/// agents actually write and neither is ambiguous. An empty list parses: "we
/// looked and found nothing" is a real result and refusing it would teach the
/// synthesiser to invent findings.
pub fn parse_findings(raw: &str) -> Result<Vec<ParsedFinding>, FindingsParseError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| FindingsParseError::NotJson {
            detail: err.to_string(),
        })?;

    let items = match &value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(object) => match object.get("findings") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            Some(_) => return Err(FindingsParseError::FindingsNotAnArray),
            None => return Err(FindingsParseError::NotFindings),
        },
        _ => return Err(FindingsParseError::NotFindings),
    };

    let mut findings = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        findings.push(parse_finding(index, item)?);
    }
    Ok(findings)
}

fn parse_finding(
    index: usize,
    value: &serde_json::Value,
) -> Result<ParsedFinding, FindingsParseError> {
    let object = value
        .as_object()
        .ok_or(FindingsParseError::NotAnObject { index })?;

    let node_key = required_string(index, object, "node_key")?;
    let level_raw = required_string(index, object, "level")?;
    let level =
        FindingLevel::parse(level_raw.trim()).ok_or_else(|| FindingsParseError::UnknownLevel {
            index,
            value: level_raw.clone(),
        })?;
    let verdict_raw = required_string(index, object, "verdict")?;
    let verdict = FindingVerdict::parse(verdict_raw.trim()).ok_or_else(|| {
        FindingsParseError::UnknownVerdict {
            index,
            value: verdict_raw.clone(),
        }
    })?;
    let rationale = required_string(index, object, "rationale")?;
    let rationale_len = rationale.chars().count();
    if rationale_len > RATIONALE_MAX_CHARS {
        return Err(FindingsParseError::TooLong {
            index,
            field: "rationale",
            limit: RATIONALE_MAX_CHARS,
            actual: rationale_len,
        });
    }

    let source_member = object
        .get("source_member")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    let replacement = match object.get("replacement") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => {
            return Err(FindingsParseError::ReplacementNotAnObject {
                index,
                node_key: node_key.clone(),
            })
        }
    };
    match (verdict, &replacement) {
        (FindingVerdict::Replace, None) => {
            return Err(FindingsParseError::ReplacementMissing {
                index,
                node_key: node_key.clone(),
            })
        }
        (other, Some(_)) if other != FindingVerdict::Replace => {
            return Err(FindingsParseError::ReplacementNotAllowed {
                index,
                node_key: node_key.clone(),
                verdict: other.as_str(),
            })
        }
        _ => {}
    }

    Ok(ParsedFinding {
        node_key: NodeKey(node_key.trim().to_string()),
        source_member,
        level,
        verdict,
        rationale: rationale.trim().to_string(),
        evidence: object
            .get("evidence")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        proposed_change: object
            .get("proposed_change")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        replacement,
    })
}

fn required_string(
    index: usize,
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<String, FindingsParseError> {
    let entry = object
        .get(field)
        .ok_or(FindingsParseError::MissingField { index, field })?;
    let text = entry
        .as_str()
        .ok_or(FindingsParseError::NotAString { index, field })?;
    if text.trim().is_empty() {
        return Err(FindingsParseError::EmptyField { index, field });
    }
    Ok(text.to_string())
}

// ── shared little renderers ────────────────────────────────────────────────

fn owner_str(owner: Option<&str>) -> String {
    match owner.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("`{name}`"),
        None => "nobody (unclaimed)".to_string(),
    }
}

fn run_status_str(run: &RunEvidence) -> &'static str {
    use crate::workflow::model::RunStatus;
    match run.status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn node_status_str(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "pending",
        NodeStatus::Ready => "ready",
        NodeStatus::Running => "running",
        NodeStatus::NeedsAttention => "needs_attention",
        NodeStatus::Blocked => "blocked",
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Skipped => "skipped",
        NodeStatus::Restored => "restored",
        NodeStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{InstancePath, RunId, RunStatus};
    use crate::workflow::review::{
        InterventionEvidence, MemberEvidence, OwnerChange, RunEvidence, TaskEvidence,
    };

    /// Pulls the first fenced ```json block out of a rendered document, so the
    /// round-trip tests parse exactly the example the agent is shown. If the
    /// example and the parser ever disagree, these tests fail rather than the
    /// interview.
    fn first_json_block(rendered: &str) -> String {
        let start = rendered
            .find("```json\n")
            .expect("rendered document embeds a json example")
            + "```json\n".len();
        let rest = &rendered[start..];
        let end = rest.find("```").expect("the json example is closed");
        rest[..end].to_string()
    }

    fn task_with_trouble() -> TaskEvidence {
        TaskEvidence {
            path: InstancePath("research".into()),
            node_key: Some(NodeKey("research".into())),
            subject: "research: find the prior art".into(),
            status: NodeStatus::Blocked,
            emergent: false,
            attention: Some(crate::workflow::model::Attention::Stuck),
            owner: Some("scout".into()),
            owner_changes: vec![OwnerChange {
                at_unix_ms: 1_000,
                from: Some("scout".into()),
                to: Some("scribe".into()),
            }],
            unresolved_blockers: vec!["plan: decide the shape".into()],
            first_seen_at_unix_ms: 0,
            last_change_at_unix_ms: 2_000,
            in_progress_ms: 3_872_000,
            idle_while_in_progress_ms: 862_000,
            interventions: vec![
                InterventionEvidence {
                    at_unix_ms: 500,
                    rung: 1,
                    kind: "local_loop".into(),
                    channel: Some("message".into()),
                    delivered: true,
                    text: Some("[karvex · watchdog] no new tool calls for 60s".into()),
                },
                InterventionEvidence {
                    at_unix_ms: 900,
                    rung: 3,
                    kind: "local_loop".into(),
                    channel: None,
                    delivered: false,
                    text: Some("[karvex · watchdog] escalating to the lead".into()),
                },
            ],
        }
    }

    fn evidence() -> MemberEvidence {
        MemberEvidence {
            name: "scout".into(),
            tasks: vec![task_with_trouble()],
            last_state: Some("idle".into()),
            last_state_at_unix_ms: Some(2_000),
        }
    }

    fn run_evidence() -> RunEvidence {
        RunEvidence {
            run_id: RunId("workflow_run:7".into()),
            workflow_name: "prior art".into(),
            kvdag_version: 3,
            status: RunStatus::Failed,
            started_at_unix_ms: 0,
            ended_at_unix_ms: Some(3_872_000),
            summary: Some("we did not finish the survey".into()),
            failure: Some("lead reported failure".into()),
            members: vec![evidence()],
            unowned_tasks: Vec::new(),
        }
    }

    fn interview_input<'a>(
        run: &'a RunEvidence,
        member_evidence: &'a MemberEvidence,
        mode: InterviewMode,
    ) -> InterviewPromptInput<'a> {
        InterviewPromptInput {
            member: "scout",
            is_lead: false,
            mode,
            evidence_only_reason: match mode {
                InterviewMode::Resumed => None,
                InterviewMode::EvidenceOnly => Some(EvidenceOnlyReason::TranscriptUnreadable),
            },
            run,
            evidence: Some(member_evidence),
            cycle_dir: "/abs/runs/7/review/1",
            answer_path: "/abs/runs/7/review/1/answers/scout.json",
        }
    }

    #[test]
    fn the_interview_document_states_its_contract_version() {
        let run = run_evidence();
        let member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(
            rendered.contains(&format!(
                "Review prompt contract: `v{REVIEW_PROMPT_VERSION}`"
            )),
            "a template change must move the version, and the version must be visible",
        );
    }

    #[test]
    fn the_synthesis_document_states_its_contract_version() {
        let run = run_evidence();
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &[],
            node_keys: &[NodeKey("research".into())],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });
        assert!(rendered.contains(&format!(
            "Review prompt contract: `v{REVIEW_PROMPT_VERSION}`"
        )));
    }

    #[test]
    fn the_interview_puts_karvexs_measured_record_to_the_teammate() {
        let run = run_evidence();
        let member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));

        // What it was asked.
        assert!(rendered.contains("research: find the prior art"));
        assert!(rendered.contains("definition node `research`"));
        // What it did, measured.
        assert!(
            rendered.contains(
                "Time in progress: 1h 04m 32s (of which the owning pane was idle for 14m 22s)"
            ),
            "the idle number is the one karvex can defend; it must be in the document",
        );
        assert!(rendered.contains("Still waiting on when the run ended: `plan: decide the shape`"));
        assert!(rendered.contains("from `scout` to `scribe`"));
        assert!(rendered.contains("cannot see why the lead did it"));
        // Where the watchdog intervened, verbatim.
        assert!(rendered.contains("rung 1 (`local_loop`), delivered over `message`"));
        assert!(rendered.contains("> [karvex · watchdog] no new tool calls for 60s"));
        assert!(
            rendered.contains("**never delivered**"),
            "an intervention karvex could not deliver must not be claimed as one it sent",
        );
        assert!(rendered.contains("watchdog flagged it as `stuck`"));
        // How the run ended.
        assert!(rendered.contains("Final run status: `failed`"));
        assert!(rendered.contains("> we did not finish the survey"));
    }

    #[test]
    fn the_resumed_opener_is_honest_about_the_fork() {
        let run = run_evidence();
        let member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(rendered.contains("**This is your own session, resumed.**"));
        assert!(rendered.contains("your original transcript is untouched"));
        assert!(rendered.contains("Interview mode: `resumed`"));
        assert!(rendered.contains("- Your member name: `scout`"));
        assert!(!rendered.contains("reviewing evidence, not resuming"));
    }

    #[test]
    fn the_evidence_only_prompt_never_claims_to_be_the_teammates_own_account() {
        let run = run_evidence();
        let member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::EvidenceOnly));

        assert!(rendered.contains("**You are reviewing evidence, not resuming the session.**"));
        assert!(rendered.contains("You are not this teammate"));
        assert!(rendered.contains("Do not write in its voice"));
        assert!(rendered.contains("Interview mode: `evidence_only`"));
        assert!(
            rendered.contains("never be presented as this teammate's own words"),
            "the honesty rule has to be stated to the agent, not only enforced behind it",
        );
        assert!(
            rendered.contains(EvidenceOnlyReason::TranscriptUnreadable.sentence()),
            "the reader is told what karvex could not do, not left to guess",
        );
        assert!(
            !rendered.contains("This is your own session"),
            "the two openers must never both appear",
        );
        // The questions themselves change voice.
        assert!(rendered.contains("What was this member asked to do"));
        assert!(!rendered.contains("What were you asked to do"));
        // And so does everything karvex says about the member.
        assert!(rendered.contains("- Member under review: `scout`"));
        assert!(
            !rendered.contains("Your member name"),
            "the second person is reserved for a resumed interview",
        );
        assert!(!rendered.contains("could not get it to you"));
        assert!(rendered.contains("could not get it to the member"));
    }

    #[test]
    fn the_five_question_keys_are_fixed_and_all_asked() {
        let run = run_evidence();
        let member = evidence();
        for mode in [InterviewMode::Resumed, InterviewMode::EvidenceOnly] {
            let rendered = render_interview_prompt(&interview_input(&run, &member, mode));
            for key in INTERVIEW_QUESTION_KEYS {
                assert!(
                    rendered.contains(&format!("- `{key}` — ")),
                    "{key} is not asked"
                );
            }
        }
        assert_eq!(
            QUESTIONS.map(|question| question.key),
            INTERVIEW_QUESTION_KEYS,
            "the renderer and the parser must ask for the same five keys",
        );
    }

    #[test]
    fn the_interview_names_absolute_paths_and_the_self_report_command() {
        let run = run_evidence();
        let member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(rendered.contains("/abs/runs/7/review/1/answers/scout.json"));
        assert!(rendered.contains(
            "`kvx workflow review answer --file /abs/runs/7/review/1/answers/scout.json`"
        ));
        assert!(rendered.contains("KARVEX_WORKFLOW_REVIEW_INTERVIEW"));
        assert!(
            !rendered.contains("./"),
            "every path in a review document is absolute",
        );
    }

    #[test]
    fn the_synthesis_names_absolute_paths_and_the_report_command() {
        let run = run_evidence();
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &[],
            node_keys: &[NodeKey("research".into())],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });
        assert!(rendered
            .contains("`kvx workflow review report --file /abs/runs/7/review/1/findings.json`"));
        assert!(!rendered.contains("./"));
    }

    #[test]
    fn the_interview_example_round_trips_through_its_own_parser() {
        let run = run_evidence();
        let member = evidence();
        for mode in [InterviewMode::Resumed, InterviewMode::EvidenceOnly] {
            let rendered = render_interview_prompt(&interview_input(&run, &member, mode));
            let example = first_json_block(&rendered);
            let answer = parse_interview_answer(&example)
                .expect("the example karvex shows the agent must be one karvex accepts");
            for (key, value) in answer.fields() {
                assert!(!value.is_empty(), "{key} came back empty");
            }
        }
    }

    #[test]
    fn the_findings_example_round_trips_through_its_own_parser() {
        let run = run_evidence();
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &[],
            node_keys: &[NodeKey("research".into())],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });
        let findings = parse_findings(&first_json_block(&rendered))
            .expect("the example karvex shows the synthesiser must be one karvex accepts");
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].level, FindingLevel::Prompt);
        assert_eq!(findings[0].verdict, FindingVerdict::Improve);
        assert_eq!(findings[0].source_member.as_deref(), Some("research"));
        assert_eq!(findings[1].verdict, FindingVerdict::Replace);
        assert!(
            findings[1].replacement.is_some(),
            "the example must exercise the one conditional the parser enforces",
        );
        assert!(findings[1].source_member.is_none());
    }

    #[test]
    fn the_synthesis_labels_each_block_with_what_may_be_claimed_about_it() {
        let run = run_evidence();
        let answer = InterviewAnswer {
            account: "I was asked to survey".into(),
            what_happened: "I surveyed".into(),
            blockers: "the api".into(),
            upstream_gaps: "nothing".into(),
            brief_changes: "say which sources count".into(),
        };
        let sources = vec![
            SynthesisSource {
                member: "scout".into(),
                is_lead: false,
                attribution: MemberAttribution::resumed(crate::workflow::model::InterrogationId(
                    "interrogation:1".into(),
                )),
                answer: Some(answer.clone()),
            },
            SynthesisSource {
                member: "scribe".into(),
                is_lead: true,
                attribution: MemberAttribution::evidence_only(EvidenceOnlyReason::InterviewBlocked),
                answer: Some(answer),
            },
        ];
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &sources,
            node_keys: &[NodeKey("research".into())],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });

        assert!(rendered.contains("**This is `scout`'s own account**"));
        assert!(rendered.contains("**Evidence-only — these are NOT `scribe`'s words.**"));
        assert!(rendered.contains("Karvex could not take this member's own account:"));
        assert!(rendered.contains(EvidenceOnlyReason::InterviewBlocked.sentence()));
        assert!(rendered.contains("never as its testimony"));
        assert!(
            rendered.contains("karvex records the attribution of every finding itself")
                || rendered.contains("Karvex records the attribution of every finding itself"),
            "the synthesiser is told karvex stamps attribution, so it does not try to",
        );
        assert!(rendered.contains("`scribe`"));
        assert!(rendered.contains("(team lead)"));
    }

    #[test]
    fn the_synthesis_classifies_findings_prompt_level_versus_structural() {
        let run = run_evidence();
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &[],
            node_keys: &[NodeKey("research".into())],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });
        assert!(rendered.contains("- `prompt` — the node's brief was wrong."));
        assert!(rendered.contains("- `structural` — the plan was wrong."));
        for verdict in FindingVerdict::ALL {
            assert!(
                rendered.contains(&format!("- `{}` —", verdict.as_str())),
                "{} is not explained",
                verdict.as_str(),
            );
        }
        assert!(rendered.contains("must** carry a complete `replacement` object"));
        assert!(rendered
            .contains("Findings may only be filed against these definition nodes: `research`."),);
        assert!(
            rendered.contains("Reporting no findings at all is a legitimate outcome"),
            "a review that must complain will invent complaints",
        );
    }

    #[test]
    fn a_missing_interview_lists_nothing_and_attributes_nothing() {
        let run = run_evidence();
        let sources = vec![SynthesisSource {
            member: "ghost".into(),
            is_lead: false,
            attribution: MemberAttribution::evidence_only(EvidenceOnlyReason::InterviewPaneGone),
            answer: None,
        }];
        let rendered = render_synthesis_prompt(&SynthesisPromptInput {
            run: &run,
            sources: &sources,
            node_keys: &[],
            cycle_dir: "/abs/runs/7/review/1",
            findings_path: "/abs/runs/7/review/1/findings.json",
        });
        assert!(rendered.contains("No answer was collected from this interview at all."));
        assert!(rendered.contains("nothing here to attribute to anyone"));
        assert!(
            rendered.contains("declares no nodes, so there is nothing to file a finding against")
        );
    }

    #[test]
    fn a_malformed_answer_names_the_field() {
        let missing = parse_interview_answer(r#"{"account": "a", "what_happened": "b"}"#)
            .expect_err("an incomplete answer is refused");
        assert_eq!(missing.field(), Some("blockers"));
        assert!(missing.to_string().contains("`blockers`"));

        let wrong_type =
            parse_interview_answer(r#"{"account": 1, "what_happened": "b", "blockers": "c", "upstream_gaps": "d", "brief_changes": "e"}"#)
                .expect_err("a non-string field is refused");
        assert_eq!(
            wrong_type,
            AnswerParseError::NotAString { field: "account" }
        );

        let empty =
            parse_interview_answer(r#"{"account": "  ", "what_happened": "b", "blockers": "c", "upstream_gaps": "d", "brief_changes": "e"}"#)
                .expect_err("a blank field is refused");
        assert_eq!(empty, AnswerParseError::EmptyField { field: "account" });
        assert!(empty.to_string().contains("say so in words"));

        let long = format!("\"{}\"", "x".repeat(ANSWER_FIELD_MAX_CHARS + 1));
        let too_long = parse_interview_answer(&format!(
            r#"{{"account": {long}, "what_happened": "b", "blockers": "c", "upstream_gaps": "d", "brief_changes": "e"}}"#
        ))
        .expect_err("an over-long field is refused");
        assert_eq!(
            too_long,
            AnswerParseError::TooLong {
                field: "account",
                limit: ANSWER_FIELD_MAX_CHARS,
                actual: ANSWER_FIELD_MAX_CHARS + 1,
            },
        );

        let not_json = parse_interview_answer("not json at all").expect_err("garbage is refused");
        assert!(matches!(not_json, AnswerParseError::NotJson { .. }));
        assert!(not_json.to_string().contains("account, what_happened"));

        assert_eq!(
            parse_interview_answer("[]").expect_err("an array is not an answer"),
            AnswerParseError::NotAnObject,
        );
    }

    #[test]
    fn an_answer_ignores_extra_keys_and_trims() {
        let answer = parse_interview_answer(
            r#"{"account": " a ", "what_happened": "b", "blockers": "c", "upstream_gaps": "d", "brief_changes": "e", "notes": "extra"}"#,
        )
        .expect("a helpful extra field does not reject the whole interview");
        assert_eq!(answer.account, "a");
        assert_eq!(answer.get("brief_changes"), Some("e"));
        assert_eq!(answer.get("notes"), None);
    }

    #[test]
    fn a_replace_finding_without_a_replacement_is_a_parse_level_refusal() {
        let err = parse_findings(
            r#"{"findings": [{"node_key": "verify", "level": "structural", "verdict": "replace", "rationale": "it never worked"}]}"#,
        )
        .expect_err("firing a role requires writing the replacement");
        assert_eq!(
            err,
            FindingsParseError::ReplacementMissing {
                index: 0,
                node_key: "verify".into(),
            },
        );
        assert!(err.to_string().contains("`verify`"));
        assert_eq!(err.index(), Some(0));
    }

    #[test]
    fn a_replacement_on_a_non_replace_verdict_is_refused() {
        let err = parse_findings(
            r#"[{"node_key": "verify", "level": "prompt", "verdict": "keep", "rationale": "fine", "replacement": {}}]"#,
        )
        .expect_err("karvex would carry a field it will never apply");
        assert_eq!(
            err,
            FindingsParseError::ReplacementNotAllowed {
                index: 0,
                node_key: "verify".into(),
                verdict: "keep",
            },
        );
    }

    #[test]
    fn the_findings_parser_names_the_finding_and_the_field() {
        let unknown_level = parse_findings(
            r#"[{"node_key": "a", "level": "vibes", "verdict": "keep", "rationale": "r"}]"#,
        )
        .expect_err("the level vocabulary is closed");
        assert_eq!(
            unknown_level,
            FindingsParseError::UnknownLevel {
                index: 0,
                value: "vibes".into(),
            },
        );
        assert!(unknown_level.to_string().contains("`prompt`"));

        let unknown_verdict = parse_findings(
            r#"[{"node_key": "a", "level": "prompt", "verdict": "fire", "rationale": "r"}]"#,
        )
        .expect_err("the verdict vocabulary is closed");
        assert!(unknown_verdict.to_string().contains("`replace`"));

        let missing = parse_findings(
            r#"{"findings": [{"node_key": "a", "level": "prompt", "verdict": "keep", "rationale": "r"}, {"level": "prompt", "verdict": "keep", "rationale": "r"}]}"#,
        )
        .expect_err("a missing node key is refused");
        assert_eq!(
            missing,
            FindingsParseError::MissingField {
                index: 1,
                field: "node_key",
            },
        );
        assert!(missing.to_string().contains("finding 1"));

        assert_eq!(
            parse_findings(r#"{"results": []}"#).expect_err("the key is `findings`"),
            FindingsParseError::NotFindings,
        );
        assert_eq!(
            parse_findings(r#"{"findings": {}}"#).expect_err("`findings` is an array"),
            FindingsParseError::FindingsNotAnArray,
        );
        assert!(matches!(
            parse_findings("{").expect_err("garbage is refused"),
            FindingsParseError::NotJson { .. },
        ));
    }

    #[test]
    fn an_empty_findings_list_is_a_legitimate_result() {
        assert!(parse_findings(r#"{"findings": []}"#)
            .expect("we looked and found nothing is a real answer")
            .is_empty());
        assert!(parse_findings("[]")
            .expect("a bare array parses too")
            .is_empty());
    }

    #[test]
    fn a_finding_defaults_its_free_form_objects_rather_than_refusing() {
        let findings = parse_findings(
            r#"[{"node_key": " verify ", "level": "prompt", "verdict": "keep", "rationale": " it was fine "}]"#,
        )
        .expect("evidence and proposed_change are optional");
        assert_eq!(findings[0].node_key, NodeKey("verify".into()));
        assert_eq!(findings[0].rationale, "it was fine");
        assert_eq!(findings[0].evidence, serde_json::json!({}));
        assert_eq!(findings[0].proposed_change, serde_json::json!({}));
    }

    #[test]
    fn the_owner_line_appears_only_when_the_task_left_the_interviewee() {
        let run = run_evidence();
        let mut member = evidence();
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(
            !rendered.contains("Owner karvex last recorded"),
            "\"you own the task we are asking you about\" is not information",
        );

        member.tasks[0].owner = Some("scribe".into());
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(rendered.contains("- Owner karvex last recorded: `scribe`"));
        assert!(rendered.contains("this interview is about `scout`'s part in it"));

        member.tasks[0].owner = None;
        let rendered =
            render_interview_prompt(&interview_input(&run, &member, InterviewMode::Resumed));
        assert!(rendered.contains("Owner karvex last recorded: nobody (unclaimed)"));
    }

    #[test]
    fn a_member_with_no_measured_record_is_told_so_rather_than_shown_a_blank() {
        let run = run_evidence();
        let mut input = interview_input(&run, &run.members[0], InterviewMode::Resumed);
        input.evidence = None;
        let rendered = render_interview_prompt(&input);
        assert!(rendered.contains("**no task evidence at all**"));
        assert!(rendered.contains("That is itself one of the things this interview is for."));
    }
}
