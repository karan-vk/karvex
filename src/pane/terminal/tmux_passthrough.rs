//! Streaming unwrapper for tmux DCS passthrough (`ESC P tmux ; … ESC \`).
//!
//! Once `$TMUX` is exported into a pane, the near-universal convention among
//! terminal apps (neovim's OSC 52 provider, fzf, yazi, lazygit, tmux-aware
//! prompts, and Karvex's own `crate::terminal_notify`) is to wrap OSC/DCS
//! sequences in tmux passthrough: `ESC P tmux ; <payload with every ESC
//! doubled> ESC \`.
//!
//! Karvex's hand-written inbound scanners (`DefaultColorEventTracker`,
//! `XtgettcapQueryTracker`, `AgentOscStateTracker`, `OscDebugTracker`, …) all
//! treat a DCS string as opaque and skip it, so a wrapped OSC 11 / OSC 4 /
//! XTGETTCAP query never reaches the code that synthesises Karvex's reply.
//! This filter removes the wrapper at the very top of the pane's inbound byte
//! path so every observer downstream sees the payload exactly as it would have
//! arrived unwrapped.
//!
//! Design constraints:
//!
//! * **Streaming.** PTY reads split anywhere, including in the middle of the
//!   `ESC P t m u x ;` prefix or the `ESC \` terminator, so the state and a
//!   short held-back run survive across calls.
//! * **Bounded.** Nothing grows without a terminator: the unwrapped payload is
//!   capped, and the held-back prefix run can never exceed [`MAX_HELD_PREFIX`].
//! * **Cheap on the fast path.** Streams without a DCS introducer, and streams
//!   whose escape sequences are not tmux passthrough, are returned as
//!   [`Cow::Borrowed`] with no allocation and no copy.
//!
//! A read that ends on an undecided introducer (a trailing `ESC`, or a partial
//! `ESC P t m u x`) holds those bytes back until the next read resolves them.
//! That is not observable: libghostty's own parser would have been sitting in
//! its escape state on exactly those bytes, rendering nothing, until the rest
//! of the sequence arrived.

use std::borrow::Cow;

use tracing::debug;

/// 7-bit DCS introducer is `ESC P`; `ESC` also starts every string terminator.
const ESC: u8 = 0x1b;
/// 8-bit DCS introducer (C1 `DCS`).
const DCS_8BIT: u8 = 0x90;
/// 8-bit string terminator (C1 `ST`).
const ST_8BIT: u8 = 0x9c;

/// The literal bytes that follow the DCS introducer in a tmux passthrough.
const PREFIX: &[u8] = b"tmux;";

/// Longest run of bytes the filter can be holding back while it decides whether
/// an introducer starts a tmux passthrough: `ESC P t m u x` (the `;` resolves
/// it). Used only as a debug assertion / sanity bound.
const MAX_HELD_PREFIX: usize = 1 + PREFIX.len();

/// Cap on the unwrapped payload of a single passthrough.
///
/// libghostty's own "normal" OSC buffer is 2 KiB (`osc.zig: MAX_BUF`) and it
/// only exceeds that for commands like OSC 52 that legitimately carry a
/// clipboard. 64 KiB keeps realistic base64 clipboard writes working while
/// bounding what one hostile or truncated sequence can make a pane retain.
const MAX_PAYLOAD: usize = 64 * 1024;

/// After the payload cap is hit the sequence is abandoned, not buffered. We
/// keep discarding while looking for the terminator, but only for this many
/// bytes — otherwise a passthrough that is never terminated would blank the
/// pane forever instead of merely losing one sequence.
const MAX_DISCARD: usize = MAX_PAYLOAD;

/// Retained payload capacity above which the buffer is released after a flush,
/// so one large clipboard write does not pin memory per pane.
const PAYLOAD_KEEP_CAPACITY: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Outside any escape sequence we care about.
    #[default]
    Ground,
    /// Saw `ESC` in [`State::Ground`]; the `ESC` is held back.
    Escape,
    /// Saw a DCS introducer; matching `PREFIX[..matched]` so far, all held back.
    Prefix { matched: usize },
    /// Inside a tmux passthrough payload.
    Payload,
    /// Inside a payload, saw `ESC`: either a doubled `ESC` or the terminator.
    PayloadEscape,
    /// Payload exceeded the cap; dropping bytes until the terminator.
    Discard,
    /// [`State::Discard`] plus a pending `ESC`.
    DiscardEscape,
    /// A DCS that is not tmux passthrough: forwarded byte-identical.
    ForeignDcs,
    /// [`State::ForeignDcs`] plus a pending `ESC`.
    ForeignDcsEscape,
}

impl State {
    /// True while bytes are being held back pending a prefix decision.
    fn is_holding(self) -> bool {
        matches!(self, State::Escape | State::Prefix { .. })
    }
}

/// Streaming `ESC P tmux ; … ESC \` unwrapper. One instance per pane, held on
/// the pane's `Mutex`-guarded core so it survives read boundaries.
#[derive(Debug, Default)]
pub(super) struct TmuxPassthroughFilter {
    state: State,
    /// Raw held-back bytes carried over from earlier calls. Only ever the
    /// partially matched introducer + prefix, so at most [`MAX_HELD_PREFIX`].
    held: Vec<u8>,
    /// Unwrapped (ESC-undoubled) payload of the passthrough being parsed.
    payload: Vec<u8>,
    /// Bytes consumed since the payload cap was hit, for [`MAX_DISCARD`].
    discarded: usize,
}

impl TmuxPassthroughFilter {
    /// Returns `input` with every complete tmux passthrough replaced by its
    /// unwrapped payload. Everything else — including unrelated DCS strings —
    /// is byte-identical, and is returned borrowed.
    pub(super) fn filter<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        // There is deliberately no separate pre-scan fast path: the loop below
        // already answers `Cow::Borrowed` after a single `position` scan when
        // the chunk holds no DCS introducer, and after zero copies when its
        // escape sequences turn out not to be tmux passthrough.
        //
        // `out` is materialised only when the output diverges from the input.
        // While it is `None`, the logical output so far is `input[..emitted]`;
        // `input[emitted..i]` is the held-back run.
        let mut out: Option<Vec<u8>> = None;
        let mut emitted = 0usize;
        let mut i = 0usize;

        while i < input.len() {
            if self.state == State::Ground {
                // Bulk-skip the plain run up to the next possible introducer.
                let rest = &input[i..];
                let next = rest
                    .iter()
                    .position(|&byte| byte == ESC || byte == DCS_8BIT);
                let Some(offset) = next else {
                    Self::forward(&mut out, input, &mut emitted, input.len());
                    break;
                };
                if offset > 0 {
                    i += offset;
                    Self::forward(&mut out, input, &mut emitted, i);
                }
            }

            let byte = input[i];
            let state = self.state;
            match state {
                State::Ground => {
                    // Start holding the introducer.
                    debug_assert_eq!(emitted, i);
                    self.state = if byte == DCS_8BIT {
                        State::Prefix { matched: 0 }
                    } else {
                        State::Escape
                    };
                }
                State::Escape => match byte {
                    b'P' => self.state = State::Prefix { matched: 0 },
                    ESC => {
                        // The previous ESC cannot be a DCS introducer. Release
                        // it and start a fresh hold at this byte, so the held
                        // run stays bounded on a stream of bare escapes.
                        self.release_held(&mut out, input, &mut emitted, i);
                    }
                    DCS_8BIT => {
                        self.release_held(&mut out, input, &mut emitted, i);
                        self.state = State::Prefix { matched: 0 };
                    }
                    _ => {
                        self.release_held(&mut out, input, &mut emitted, i + 1);
                        self.state = State::Ground;
                    }
                },
                State::Prefix { matched } => {
                    if byte == PREFIX[matched] {
                        if matched + 1 == PREFIX.len() {
                            // Committed: drop the wrapper bytes entirely.
                            Self::ensure_out(&mut out, input, emitted);
                            self.held.clear();
                            emitted = i + 1;
                            self.payload.clear();
                            self.discarded = 0;
                            self.state = State::Payload;
                        } else {
                            self.state = State::Prefix {
                                matched: matched + 1,
                            };
                        }
                    } else {
                        // Not tmux passthrough: replay the wrapper verbatim and
                        // forward the rest of the DCS untouched.
                        self.release_held(&mut out, input, &mut emitted, i + 1);
                        self.state = match byte {
                            ESC => State::ForeignDcsEscape,
                            ST_8BIT => State::Ground,
                            _ => State::ForeignDcs,
                        };
                    }
                }
                State::Payload => {
                    Self::ensure_out(&mut out, input, emitted);
                    emitted = i + 1;
                    match byte {
                        ESC => self.state = State::PayloadEscape,
                        ST_8BIT => self.flush_payload(&mut out),
                        _ => self.payload.push(byte),
                    }
                    self.enforce_payload_cap();
                }
                State::PayloadEscape => {
                    Self::ensure_out(&mut out, input, emitted);
                    emitted = i + 1;
                    match byte {
                        // `ESC ESC` in the payload is one literal ESC.
                        ESC => {
                            self.payload.push(ESC);
                            self.state = State::Payload;
                        }
                        b'\\' => self.flush_payload(&mut out),
                        // Stray `ESC` before an 8-bit ST: keep the ESC, end here.
                        ST_8BIT => {
                            self.payload.push(ESC);
                            self.flush_payload(&mut out);
                        }
                        // Tolerate an app that failed to double its escapes.
                        _ => {
                            self.payload.push(ESC);
                            self.payload.push(byte);
                            self.state = State::Payload;
                        }
                    }
                    self.enforce_payload_cap();
                }
                State::Discard | State::DiscardEscape => {
                    Self::ensure_out(&mut out, input, emitted);
                    emitted = i + 1;
                    self.discarded = self.discarded.saturating_add(1);
                    let terminated = match (state, byte) {
                        (State::Discard, ESC) => {
                            self.state = State::DiscardEscape;
                            false
                        }
                        (State::Discard, ST_8BIT) | (State::DiscardEscape, b'\\') => true,
                        (State::DiscardEscape, ESC) => false,
                        (State::DiscardEscape, ST_8BIT) => true,
                        (State::DiscardEscape, _) => {
                            self.state = State::Discard;
                            false
                        }
                        _ => false,
                    };
                    if terminated {
                        self.state = State::Ground;
                    } else if self.discarded >= MAX_DISCARD {
                        // Give up rather than swallow the rest of the stream.
                        debug!("abandoned unterminated tmux passthrough after discard cap");
                        self.state = State::Ground;
                    }
                }
                State::ForeignDcs => {
                    Self::forward(&mut out, input, &mut emitted, i + 1);
                    match byte {
                        ESC => self.state = State::ForeignDcsEscape,
                        ST_8BIT => self.state = State::Ground,
                        _ => {}
                    }
                }
                State::ForeignDcsEscape => {
                    Self::forward(&mut out, input, &mut emitted, i + 1);
                    match byte {
                        b'\\' | ST_8BIT => self.state = State::Ground,
                        ESC => {}
                        _ => self.state = State::ForeignDcs,
                    }
                }
            }
            i += 1;
        }

        if self.state.is_holding() && emitted < input.len() {
            // Carry the undecided run into the next read.
            Self::ensure_out(&mut out, input, emitted);
            self.held.extend_from_slice(&input[emitted..]);
            debug_assert!(self.held.len() <= MAX_HELD_PREFIX);
            emitted = input.len();
        }
        debug_assert_eq!(emitted, input.len());

        match out {
            Some(bytes) => Cow::Owned(bytes),
            None => Cow::Borrowed(input),
        }
    }

    /// Passes `input[emitted..end]` through unchanged.
    fn forward(out: &mut Option<Vec<u8>>, input: &[u8], emitted: &mut usize, end: usize) {
        if let Some(out) = out.as_mut() {
            out.extend_from_slice(&input[*emitted..end]);
        }
        *emitted = end;
    }

    /// Emits the held-back run (carry-over plus `input[emitted..end]`) verbatim.
    fn release_held(
        &mut self,
        out: &mut Option<Vec<u8>>,
        input: &[u8],
        emitted: &mut usize,
        end: usize,
    ) {
        if self.held.is_empty() {
            // The held bytes are contiguous with the output, so passing them
            // through needs no divergence from the borrowed input.
            Self::forward(out, input, emitted, end);
            return;
        }
        let out = Self::ensure_out(out, input, *emitted);
        out.extend_from_slice(&self.held);
        self.held.clear();
        out.extend_from_slice(&input[*emitted..end]);
        *emitted = end;
    }

    /// Materialises the owned output buffer, seeded with `input[..emitted]`.
    fn ensure_out<'o>(
        out: &'o mut Option<Vec<u8>>,
        input: &[u8],
        emitted: usize,
    ) -> &'o mut Vec<u8> {
        out.get_or_insert_with(|| {
            let mut owned = Vec::with_capacity(input.len());
            owned.extend_from_slice(&input[..emitted]);
            owned
        })
    }

    /// Emits the unwrapped payload and returns to [`State::Ground`].
    fn flush_payload(&mut self, out: &mut Option<Vec<u8>>) {
        if let Some(out) = out.as_mut() {
            out.extend_from_slice(&self.payload);
        }
        if self.payload.capacity() > PAYLOAD_KEEP_CAPACITY {
            self.payload = Vec::new();
        } else {
            self.payload.clear();
        }
        self.discarded = 0;
        self.state = State::Ground;
    }

    /// Drops an oversized payload instead of buffering it without bound.
    fn enforce_payload_cap(&mut self) {
        if self.payload.len() <= MAX_PAYLOAD {
            return;
        }
        debug!(
            payload_len = self.payload.len(),
            "dropping oversized tmux passthrough payload"
        );
        self.payload = Vec::new();
        self.discarded = 0;
        self.state = match self.state {
            State::PayloadEscape => State::DiscardEscape,
            _ => State::Discard,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = TmuxPassthroughFilter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(filter.filter(chunk).as_ref());
        }
        out
    }

    #[test]
    fn tmux_passthrough_unwraps_osc52_clipboard_write() {
        assert_eq!(
            filtered(&[b"before\x1bPtmux;\x1b\x1b]52;c;Y2xpcA==\x07\x1b\\after"]),
            b"before\x1b]52;c;Y2xpcA==\x07after".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_unwraps_osc11_query() {
        assert_eq!(
            filtered(&[b"\x1bPtmux;\x1b\x1b]11;?\x07\x1b\\"]),
            b"\x1b]11;?\x07".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_unescapes_doubled_escape() {
        // Inner sequence terminated with ST, so both of its escapes are doubled.
        assert_eq!(
            filtered(&[b"\x1bPtmux;\x1b\x1b]11;?\x1b\x1b\\\x1b\\"]),
            b"\x1b]11;?\x1b\\".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_tolerates_undoubled_escape_in_payload() {
        assert_eq!(
            filtered(&[b"\x1bPtmux;\x1b]11;?\x07\x1b\\"]),
            b"\x1b]11;?\x07".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_handles_sequence_split_across_reads() {
        let wrapped: &[u8] = b"pre\x1bPtmux;\x1b\x1b]52;c;Y2xpcA==\x07\x1b\\post";
        let expected = b"pre\x1b]52;c;Y2xpcA==\x07post".to_vec();
        for split in 0..=wrapped.len() {
            assert_eq!(
                filtered(&[&wrapped[..split], &wrapped[split..]]),
                expected,
                "single split at {split}"
            );
        }
        // Every byte delivered on its own read exercises all boundaries at once.
        let singles: Vec<&[u8]> = wrapped.chunks(1).collect();
        assert_eq!(filtered(&singles), expected, "byte-at-a-time");
    }

    #[test]
    fn tmux_passthrough_accepts_eight_bit_dcs_and_st() {
        assert_eq!(
            filtered(&[b"\x90tmux;\x1b\x1b]52;c;Yg==\x07\x9c"]),
            b"\x1b]52;c;Yg==\x07".to_vec()
        );
        for split in 0..=8usize {
            let wrapped: &[u8] = b"\x90tmux;\x1b\x1b]11;?\x07\x9c";
            assert_eq!(
                filtered(&[&wrapped[..split], &wrapped[split..]]),
                b"\x1b]11;?\x07".to_vec(),
                "8-bit split at {split}"
            );
        }
    }

    #[test]
    fn tmux_passthrough_forwards_unrelated_dcs_unchanged() {
        for input in [
            b"\x1bP+q5463\x1b\\".as_slice(),
            b"\x1bP1000p\x1b\\".as_slice(),
            b"\x1bPtmu\x1b\\".as_slice(),
            b"\x1bPtmuy;payload\x1b\\".as_slice(),
            b"\x1bP\x1b\\".as_slice(),
            b"\x90+q5463\x9c".as_slice(),
            b"\x1b[0m\x1b]0;title\x07plain".as_slice(),
        ] {
            assert_eq!(filtered(&[input]), input.to_vec(), "{input:?}");
            for split in 0..=input.len() {
                assert_eq!(
                    filtered(&[&input[..split], &input[split..]]),
                    input.to_vec(),
                    "{input:?} split at {split}"
                );
            }
        }
    }

    #[test]
    fn tmux_passthrough_forwards_bare_escapes_without_holding_unboundedly() {
        let mut filter = TmuxPassthroughFilter::default();
        let escapes = vec![ESC; 4096];
        let out = filter.filter(&escapes);
        // All but the last (still undecided) ESC must be released immediately.
        assert_eq!(out.len(), escapes.len() - 1);
        assert!(filter.held.len() <= MAX_HELD_PREFIX);
        assert_eq!(filter.filter(b"x").as_ref(), b"\x1bx");
    }

    #[test]
    fn tmux_passthrough_drops_unterminated_sequence_at_cap() {
        let mut filter = TmuxPassthroughFilter::default();
        let mut input = Vec::from(b"\x1bPtmux;".as_slice());
        input.extend(std::iter::repeat_n(b'a', MAX_PAYLOAD + 1));
        assert!(filter.filter(&input).as_ref().is_empty());
        assert!(filter.payload.is_empty());

        // The stream still recovers once the sequence finally terminates.
        assert!(filter.filter(b"tail\x1b\\").as_ref().is_empty());
        assert_eq!(filter.filter(b"live").as_ref(), b"live");
    }

    #[test]
    fn tmux_passthrough_recovers_from_never_terminated_sequence() {
        let mut filter = TmuxPassthroughFilter::default();
        let mut input = Vec::from(b"\x1bPtmux;".as_slice());
        // Exactly enough to fill the payload cap and then exhaust the discard
        // window: a passthrough that never terminates must not blank the pane.
        input.extend(std::iter::repeat_n(b'a', MAX_PAYLOAD + 1 + MAX_DISCARD));
        assert!(filter.filter(&input).as_ref().is_empty());
        assert_eq!(filter.state, State::Ground);
        assert_eq!(filter.filter(b"live").as_ref(), b"live");
    }

    #[test]
    fn tmux_passthrough_borrows_input_without_dcs_introducer() {
        let mut filter = TmuxPassthroughFilter::default();
        assert!(matches!(
            filter.filter(b"plain terminal output\r\n"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn tmux_passthrough_borrows_input_with_unrelated_escape_sequences() {
        let mut filter = TmuxPassthroughFilter::default();
        for chunk in [
            b"\x1b[1;32mgreen\x1b[0m\r\n".as_slice(),
            b"\x1b]0;title\x07".as_slice(),
            b"\x1bP+q5463\x1b\\done".as_slice(),
        ] {
            assert!(
                matches!(filter.filter(chunk), Cow::Borrowed(_)),
                "{chunk:?} should not allocate"
            );
        }
    }

    #[test]
    fn tmux_passthrough_handles_adjacent_and_empty_sequences() {
        assert_eq!(
            filtered(&[b"\x1bPtmux;\x1b\\\x1bPtmux;\x1b\x1b]11;?\x07\x1b\\"]),
            b"\x1b]11;?\x07".to_vec()
        );
    }

    #[test]
    fn tmux_passthrough_passes_binary_payload_through() {
        let payload: Vec<u8> = (0u8..=u8::MAX)
            .filter(|byte| *byte != ESC && *byte != ST_8BIT)
            .collect();
        let mut wrapped = Vec::from(b"\x1bPtmux;".as_slice());
        wrapped.extend_from_slice(&payload);
        wrapped.extend_from_slice(b"\x1b\\");
        assert_eq!(filtered(&[&wrapped]), payload);
    }
}
