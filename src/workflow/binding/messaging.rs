//! Talking to a run's Claude Code sessions over their documented inbox
//! sockets.
//!
//! ## The channel
//!
//! Every Claude Code session with cross-session messaging enabled binds a
//! per-session Unix socket and exports its path as
//! `CLAUDE_CODE_MESSAGING_SOCKET`, alongside a per-session
//! `CLAUDE_CODE_MESSAGING_TOKEN`. karvex learns both from the run's
//! `SessionStart` hook (`super::identity`). The protocol is newline-delimited
//! JSON; upstream documents the auth frame and Claude Code's own startup log
//! prints the whole recipe:
//!
//! ```text
//! [uds-messaging] Inject messages (auth line optional here):
//!   { echo '{"type":"auth","token":"'"$CLAUDE_CODE_MESSAGING_TOKEN"'"}';
//!     echo '{"type":"user","message":{"role":"user","content":"hello"}}'; }
//!   | socat - UNIX-CONNECT:<socket>
//! ```
//!
//! Verified end to end against 2.1.232 on 2026-08-15: those two lines into a
//! live session's socket started a turn in that session, and the receiving
//! Claude acted on the text. Two details were confirmed at the same time and
//! are load-bearing here:
//!
//! * A frame carrying a `session_id` that is **not** the receiving session's is
//!   dropped. karvex always sends one, so a socket path that has been recycled
//!   to a different session by the time karvex writes cannot misdeliver.
//! * The auth frame is optional on Linux and required on Windows. karvex always
//!   sends it when it has a token: it costs one line, and it is what identifies
//!   the writer as the session's own child where process evidence is missing
//!   (macOS after the writer exits, and containers where Claude Code is pid 1).
//!
//! ## What karvex is, to the receiving session
//!
//! karvex is the lead's *parent*, not its child, so Claude Code's own-child
//! process check never fires for it and a karvex message is an ordinary peer
//! message subject to the receiving session's inbound controls. That would make
//! delivery depend on the lead's permission mode, so the run's `--settings`
//! payload sets the documented `crossSessionInbound: "accept"` — the same knob
//! upstream documents for letting an unattended worker take messages.
//!
//! Pure by construction: this module encodes frames and classifies support. The
//! socket write itself is the caller's, in the `App` glue.

use std::fmt;

/// The first Claude Code release with cross-session messaging.
pub const MIN_MESSAGING_VERSION: (u32, u32, u32) = (2, 1, 224);

/// The environment variables that switch off the feature-flag evaluation
/// cross-session messaging depends on.
///
/// Upstream is explicit that when any of these turns feature-flag evaluation
/// off, messaging "stays off" — and stays off *silently*: the session looks
/// entirely healthy, `/status` shows no peer address, and a send simply never
/// arrives.
///
/// **Their presence is a suspicion, not a verdict.** Probed live on 2026-08-15:
/// on an account whose GrowthBook features are already cached in
/// `~/.claude.json`, `DISABLE_TELEMETRY=1 claude` changed nothing — the peer
/// address was still shown, `/list-agents` still worked, and the socket and
/// token were still exported. Only a fresh, uncached `HOME` reproduced the
/// documented kill. So karvex reports these as a *may have*, never refuses a
/// launch or a send over them, and takes its authoritative answer from the
/// evidence the session itself reports: a session that came up with messaging
/// on tells karvex its socket through the run's `SessionStart` hook, and one
/// that did not has no socket to tell.
pub const MESSAGING_KILL_SWITCH_VARS: [&str; 4] = [
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_TELEMETRY",
    "DO_NOT_TRACK",
    "DISABLE_GROWTHBOOK",
];

/// Whether a value of one of [`MESSAGING_KILL_SWITCH_VARS`] actually disables
/// anything.
///
/// Claude Code treats these as booleans and an empty or `0`/`false` value as
/// unset, so reporting a run as unmessageable because `DO_NOT_TRACK=` is
/// exported empty would be a false alarm.
pub fn kill_switch_is_engaged(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// What the pre-launch preflight concluded about messaging this run's sessions.
///
/// Two of these are *facts* karvex can check before launching — the installed
/// version and the platform — and one is a *suspicion* it deliberately cannot
/// turn into a refusal. See [`MESSAGING_KILL_SWITCH_VARS`] for why: the same
/// variable kills messaging on an account whose feature flags have never been
/// fetched and does nothing at all on one where they are cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessagingSupport {
    Available,
    /// `claude` is older than cross-session messaging.
    ClaudeTooOld {
        found: String,
        required: String,
    },
    /// Upstream does not offer cross-session messaging on native Windows.
    UnsupportedPlatform {
        platform: &'static str,
    },
    /// A documented kill switch reaches the pane. Advisory only.
    KillSwitchSuspected {
        variable: String,
        value: String,
    },
}

impl MessagingSupport {
    /// Whether karvex expects messaging to work. A suspected kill switch counts
    /// as available, because it usually is.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available | Self::KillSwitchSuspected { .. })
    }

    /// Whether karvex should refuse to even try.
    ///
    /// Only the two checkable facts. A suspected kill switch never blocks a
    /// send: the authoritative answer is whether the session reported an inbox
    /// socket through the run's `SessionStart` hook, and refusing on a suspicion
    /// would break messaging on every machine that merely sets `DO_NOT_TRACK`.
    pub fn blocks_messaging(&self) -> bool {
        matches!(
            self,
            Self::ClaudeTooOld { .. } | Self::UnsupportedPlatform { .. }
        )
    }

    /// A short, stable word for the wire and for logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::ClaudeTooOld { .. } => "claude_too_old",
            Self::UnsupportedPlatform { .. } => "unsupported_platform",
            Self::KillSwitchSuspected { .. } => "kill_switch_suspected",
        }
    }
}

impl fmt::Display for MessagingSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available => f.write_str("cross-session messaging is available"),
            Self::ClaudeTooOld { found, required } => write!(
                f,
                "messaging a run's sessions needs Claude Code {required} or newer; \
                 this machine has {found}"
            ),
            Self::UnsupportedPlatform { platform } => write!(
                f,
                "Claude Code does not offer cross-session messaging on {platform}, \
                 so karvex cannot message this run's sessions"
            ),
            Self::KillSwitchSuspected { variable, value } => write!(
                f,
                "{variable}={value} reaches the run's pane. That variable can switch off the \
                 feature-flag evaluation cross-session messaging depends on, but only when the \
                 flags have never been fetched — probed live, it changed nothing on an account \
                 with them cached. karvex is trying anyway; if the run's sessions come up with no \
                 inbox socket, unset it and start the run again."
            ),
        }
    }
}

/// Which platform the check is being made for. Named rather than `cfg`-ed so
/// the policy is testable for every target from any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagingPlatform {
    Linux,
    MacOs,
    Windows,
}

impl MessagingPlatform {
    /// This build's platform.
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Self::Linux
        }
    }
}

/// The whole pre-launch preflight, as a pure function over the three things
/// that decide it: the installed version, the platform, and the environment the
/// run's pane will carry.
///
/// Deliberately *not* fatal to a run, and deliberately not the last word. A run
/// whose lead cannot be messaged is still a perfectly good run — the user
/// steers it by clicking its pane. And a suspected kill switch is a warning
/// rather than a verdict, because it is not reliably detectable from here:
/// karvex's authoritative signal is post-launch and evidence-based, namely
/// whether each session reported an inbox socket through the run's own
/// `SessionStart` hook.
///
/// What this does buy is a named, loud reason in the two cases that *are*
/// checkable, and a warning worth reading in the third.
pub fn classify_support<'a>(
    claude_version: (u32, u32, u32),
    platform: MessagingPlatform,
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> MessagingSupport {
    if claude_version < MIN_MESSAGING_VERSION {
        return MessagingSupport::ClaudeTooOld {
            found: version_string(claude_version),
            required: version_string(MIN_MESSAGING_VERSION),
        };
    }
    if matches!(platform, MessagingPlatform::Windows) {
        return MessagingSupport::UnsupportedPlatform {
            platform: "native Windows",
        };
    }
    for (name, value) in env {
        if MESSAGING_KILL_SWITCH_VARS.contains(&name) && kill_switch_is_engaged(value) {
            return MessagingSupport::KillSwitchSuspected {
                variable: name.to_string(),
                value: value.trim().to_string(),
            };
        }
    }
    MessagingSupport::Available
}

fn version_string(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

// ── frames ─────────────────────────────────────────────────────────────────

/// How urgently the receiving session should read the message.
///
/// Claude Code's own vocabulary: `now` is read between tool calls in the
/// current turn, `next` (its default) queues for the next one, `later` queues
/// behind everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    Now,
    #[default]
    Next,
    Later,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::Next => "next",
            Self::Later => "later",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "now" => Some(Self::Now),
            "next" | "" => Some(Self::Next),
            "later" => Some(Self::Later),
            _ => None,
        }
    }
}

/// One message karvex is about to write into a session's inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The receiving session's id. Always sent: Claude Code drops a frame whose
    /// `session_id` is not its own, which is what stops a recycled socket path
    /// from delivering this run's message into somebody else's session.
    pub session_id: String,
    /// The sender name the receiving Claude sees. Not a reply address —
    /// karvex is not a Claude Code session and cannot be replied to over this
    /// channel, so the text should say how to answer if an answer is wanted.
    pub from: String,
    pub priority: Priority,
    pub text: String,
}

/// Why an envelope was refused before anything was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    EmptyText,
    /// Claude Code ignores a `user` frame whose content is not a non-empty
    /// string; a control character in the middle of a JSON line would end the
    /// frame early.
    ControlCharacters,
    TextTooLong {
        len: usize,
        max: usize,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyText => f.write_str("a message needs some text"),
            Self::ControlCharacters => f.write_str(
                "a message cannot contain control characters other than newline and tab",
            ),
            Self::TextTooLong { len, max } => write!(
                f,
                "the message is {len} bytes; Claude Code's inbox drops a connection whose line \
                 exceeds {max} bytes"
            ),
        }
    }
}

/// Claude Code drops a connection whose buffer passes 1 MiB without a newline.
/// karvex refuses well before that, because a message that large is a mistake
/// rather than a steer.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The bytes to write to the session's socket, auth frame first.
///
/// One connection, two lines, then a half-close. The auth frame is only sent
/// when karvex actually has the session's token; upstream treats a bad auth
/// frame as a reason to drop the connection where auth is required, so an
/// invented token would be worse than none.
pub fn encode_message(envelope: &Envelope, token: Option<&str>) -> Result<Vec<u8>, EnvelopeError> {
    let text = envelope.text.trim_end_matches(['\r', '\n']);
    if text.trim().is_empty() {
        return Err(EnvelopeError::EmptyText);
    }
    if text
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        return Err(EnvelopeError::ControlCharacters);
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(EnvelopeError::TextTooLong {
            len: text.len(),
            max: MAX_MESSAGE_BYTES,
        });
    }

    let mut out = String::new();
    if let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) {
        out.push_str(&serde_json::json!({ "type": "auth", "token": token }).to_string());
        out.push('\n');
    }
    out.push_str(
        &serde_json::json!({
            "type": "user",
            "from": envelope.from,
            "session_id": envelope.session_id,
            "priority": envelope.priority.as_str(),
            "message": { "role": "user", "content": text },
        })
        .to_string(),
    );
    out.push('\n');
    Ok(out.into_bytes())
}

/// How a message actually reached a run session.
///
/// Journalled on every send, because the two channels have genuinely different
/// semantics and a caller that cannot tell them apart will mis-diagnose the
/// difference. The inbox socket is Claude Code's own peer channel: the message
/// arrives labelled as coming from another session, subject to the receiver's
/// inbound controls, and never counts as the user's consent. Pane input is
/// karvex typing into the terminal, which is indistinguishable from the user
/// typing it.
///
/// The fallback is not cosmetic. A teammate's `CLAUDE_CODE_MESSAGING_TOKEN`
/// exists only in that teammate's own hook environment — probed live, teammates
/// never register in `~/.claude/sessions/`, so there is no key file and no
/// token to recover afterwards. If karvex restarts, every in-memory endpoint is
/// gone and pane input is the only channel left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryChannel {
    /// Claude Code's documented per-session inbox socket.
    InboxSocket,
    /// Typed into the session's karvex pane, the way karvex steers every other
    /// agent.
    PaneInput,
}

impl DeliveryChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InboxSocket => "inbox_socket",
            Self::PaneInput => "pane_input",
        }
    }
}

/// The `from` name karvex signs a run's messages with.
///
/// Names the run, not the product: the receiving Claude is told the message
/// came from another session, and "karvex-run-abc123" is what lets a teammate
/// tell a steer from its lead apart from a steer from the orchestrator.
pub fn sender_name(run_id: &crate::workflow::model::RunId) -> String {
    super::identity::lead_session_name(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::RunId;

    fn envelope() -> Envelope {
        Envelope {
            session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".to_string(),
            from: "karvex-run-abc12345".to_string(),
            priority: Priority::Now,
            text: "the schema migration landed; rebase before you continue".to_string(),
        }
    }

    #[test]
    fn the_installed_version_supports_messaging() {
        assert_eq!(
            classify_support((2, 1, 232), MessagingPlatform::Linux, []),
            MessagingSupport::Available
        );
    }

    #[test]
    fn a_claude_below_the_messaging_release_is_reported_rather_than_assumed_working() {
        let support = classify_support((2, 1, 223), MessagingPlatform::Linux, []);
        assert_eq!(support.code(), "claude_too_old");
        assert!(support.to_string().contains("2.1.224"));
    }

    #[test]
    fn native_windows_has_no_cross_session_messaging() {
        let support = classify_support((2, 1, 232), MessagingPlatform::Windows, []);
        assert_eq!(support.code(), "unsupported_platform");
        assert!(support.to_string().contains("Windows"));
        // macOS does.
        assert!(classify_support((2, 1, 232), MessagingPlatform::MacOs, []).is_available());
    }

    #[test]
    fn every_documented_kill_switch_is_detected_and_named() {
        for variable in MESSAGING_KILL_SWITCH_VARS {
            let support =
                classify_support((2, 1, 232), MessagingPlatform::Linux, [(variable, "1")]);
            match &support {
                MessagingSupport::KillSwitchSuspected {
                    variable: named, ..
                } => {
                    assert_eq!(named, variable)
                }
                other => panic!("{variable} was not detected: {other:?}"),
            }
            assert!(support.to_string().contains(variable));
        }
    }

    /// Probed live: on an account with GrowthBook features already cached,
    /// `DISABLE_TELEMETRY=1 claude` left the peer address, `/list-agents`, and
    /// the exported socket and token all intact. Only a fresh uncached `HOME`
    /// reproduced the documented kill. A suspicion must therefore never refuse
    /// a launch or a send.
    #[test]
    fn a_suspected_kill_switch_warns_but_never_blocks() {
        let support = classify_support(
            (2, 1, 232),
            MessagingPlatform::Linux,
            [("DISABLE_TELEMETRY", "1")],
        );
        assert!(support.is_available(), "a suspicion is not a refusal");
        assert!(!support.blocks_messaging());
        assert_eq!(support.code(), "kill_switch_suspected");
        // The wording has to say it is a maybe, or a user will chase the wrong
        // thing when their run is perfectly healthy.
        let message = support.to_string();
        assert!(message.contains("can switch off"), "{message}");
        assert!(message.contains("cached"), "{message}");
    }

    #[test]
    fn the_two_checkable_facts_are_the_only_things_that_block() {
        assert!(classify_support((2, 1, 223), MessagingPlatform::Linux, []).blocks_messaging());
        assert!(classify_support((2, 1, 232), MessagingPlatform::Windows, []).blocks_messaging());
        assert!(!classify_support((2, 1, 232), MessagingPlatform::Linux, []).blocks_messaging());
    }

    #[test]
    fn a_kill_switch_set_to_a_falsey_value_is_not_even_suspected() {
        for value in ["", "  ", "0", "false", "FALSE", "no", "off"] {
            assert_eq!(
                classify_support(
                    (2, 1, 232),
                    MessagingPlatform::Linux,
                    [("DO_NOT_TRACK", value)]
                ),
                MessagingSupport::Available,
                "DO_NOT_TRACK={value:?} should not even raise a suspicion"
            );
        }
        for value in ["1", "true", "yes"] {
            assert!(matches!(
                classify_support(
                    (2, 1, 232),
                    MessagingPlatform::Linux,
                    [("DO_NOT_TRACK", value)]
                ),
                MessagingSupport::KillSwitchSuspected { .. }
            ));
        }
    }

    #[test]
    fn an_unrelated_variable_raises_no_suspicion() {
        assert_eq!(
            classify_support(
                (2, 1, 232),
                MessagingPlatform::Linux,
                [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_BUG_COMMAND", "1")]
            ),
            MessagingSupport::Available
        );
    }

    #[test]
    fn the_encoded_frames_are_the_two_lines_upstream_documents() {
        let bytes = encode_message(&envelope(), Some("50093985aaaabbbbccccddddeeeeffff"))
            .expect("a plain message encodes");
        let text = String::from_utf8(bytes).expect("utf8");
        let mut lines = text.lines();
        let auth: serde_json::Value =
            serde_json::from_str(lines.next().expect("an auth line")).expect("auth json");
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "50093985aaaabbbbccccddddeeeeffff");
        let user: serde_json::Value =
            serde_json::from_str(lines.next().expect("a user line")).expect("user json");
        assert_eq!(user["type"], "user");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(
            user["message"]["content"],
            "the schema migration landed; rebase before you continue"
        );
        assert_eq!(user["priority"], "now");
        assert_eq!(user["from"], "karvex-run-abc12345");
        assert_eq!(lines.next(), None);
        assert!(text.ends_with('\n'), "every frame is newline terminated");
    }

    /// Verified live: a frame whose `session_id` is not the receiving session's
    /// is dropped. Addressing every message is what makes a recycled socket
    /// path fail closed instead of steering a stranger's session.
    #[test]
    fn every_message_is_addressed_to_the_session_it_is_for() {
        let bytes = encode_message(&envelope(), None).expect("encodes without a token");
        let user: serde_json::Value =
            serde_json::from_str(String::from_utf8(bytes).expect("utf8").trim()).expect("one line");
        assert_eq!(user["session_id"], "51ea857f-cb96-4372-ae75-bab1640c8428");
    }

    #[test]
    fn no_token_means_no_auth_frame_rather_than_an_invented_one() {
        let bytes = encode_message(&envelope(), None).expect("encodes");
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains("\"auth\""));
        // An all-whitespace token is the same as none.
        let blank = encode_message(&envelope(), Some("   ")).expect("encodes");
        assert_eq!(String::from_utf8(blank).expect("utf8").lines().count(), 1);
    }

    #[test]
    fn an_empty_message_is_refused_before_anything_is_written() {
        let mut empty = envelope();
        empty.text = "   \n".to_string();
        assert_eq!(encode_message(&empty, None), Err(EnvelopeError::EmptyText));
    }

    #[test]
    fn a_control_character_cannot_smuggle_a_second_frame() {
        let mut sneaky = envelope();
        sneaky.text = "ok\u{0}".to_string();
        assert_eq!(
            encode_message(&sneaky, None),
            Err(EnvelopeError::ControlCharacters)
        );
    }

    #[test]
    fn a_newline_in_the_text_is_carried_rather_than_refused() {
        let mut multiline = envelope();
        multiline.text = "line one\nline two".to_string();
        let bytes = encode_message(&multiline, None).expect("multi-line text is fine");
        let text = String::from_utf8(bytes).expect("utf8");
        // One physical line on the wire: the newline is JSON-escaped.
        assert_eq!(text.lines().count(), 1);
        let user: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
        assert_eq!(user["message"]["content"], "line one\nline two");
    }

    #[test]
    fn an_oversized_message_is_refused_rather_than_dropped_by_the_receiver() {
        let mut huge = envelope();
        huge.text = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(matches!(
            encode_message(&huge, None),
            Err(EnvelopeError::TextTooLong { .. })
        ));
    }

    #[test]
    fn priorities_round_trip_through_their_wire_words() {
        for priority in [Priority::Now, Priority::Next, Priority::Later] {
            assert_eq!(Priority::parse(priority.as_str()), Some(priority));
        }
        assert_eq!(Priority::parse(""), Some(Priority::Next));
        assert_eq!(Priority::parse("URGENT"), None);
        assert_eq!(Priority::default(), Priority::Next);
    }

    #[test]
    fn the_two_delivery_channels_have_stable_wire_words() {
        assert_eq!(DeliveryChannel::InboxSocket.as_str(), "inbox_socket");
        assert_eq!(DeliveryChannel::PaneInput.as_str(), "pane_input");
    }

    #[test]
    fn the_sender_name_is_the_runs_own_session_name() {
        assert_eq!(
            sender_name(&RunId::new("workflow_run:abc12345ff")),
            "karvex-run-abc12345"
        );
    }
}
