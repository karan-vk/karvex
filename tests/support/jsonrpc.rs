//! Newline-delimited JSON client helpers for the API socket.
//!
//! `tests/api_ping.rs` grew a private copy of this shape first; it is lifted
//! here so a second integration binary (`tests/workflow_headless.rs`) can drive
//! requests, subscriptions, and the event stream without another copy.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// A connection to the API socket that reads one JSON value per line.
///
/// One connection per subscription: the server streams events on the same
/// connection that opened them.
pub struct JsonLineReader {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl JsonLineReader {
    pub fn connect(socket_path: &Path) -> Self {
        Self {
            stream: UnixStream::connect(socket_path).unwrap_or_else(|err| {
                panic!("failed to connect to {}: {err}", socket_path.display())
            }),
            buf: Vec::new(),
        }
    }

    pub fn send_line(&mut self, json: &str) {
        self.stream.write_all(json.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.stream.flush().unwrap();
    }

    pub fn read_json_line(&mut self, timeout: Duration) -> serde_json::Value {
        self.try_read_json_line(timeout)
            .unwrap_or_else(|| panic!("timed out waiting for a json line"))
    }

    pub fn try_read_json_line(&mut self, timeout: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        self.stream.set_nonblocking(true).unwrap();

        loop {
            if let Some(pos) = self.buf.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8(self.buf.drain(..=pos).collect()).unwrap();
                self.stream.set_nonblocking(false).unwrap();
                return Some(serde_json::from_str(&line).unwrap_or_else(|err| {
                    panic!("malformed json line {line:?}: {err}");
                }));
            }

            if Instant::now() >= deadline {
                self.stream.set_nonblocking(false).unwrap();
                return None;
            }

            let mut bytes = [0u8; 4096];
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("stream closed while waiting for a json line"),
                Ok(n) => self.buf.extend_from_slice(&bytes[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to read a json line: {err}"),
            }
        }
    }
}

/// Sends one request on a fresh connection and returns its response.
pub fn send_request(socket_path: &Path, json: &str) -> serde_json::Value {
    send_request_with_timeout(socket_path, json, Duration::from_secs(10))
}

pub fn send_request_with_timeout(
    socket_path: &Path,
    json: &str,
    timeout: Duration,
) -> serde_json::Value {
    let mut reader = JsonLineReader::connect(socket_path);
    reader.send_line(json);
    reader.read_json_line(timeout)
}

/// Sends a request and asserts it succeeded, returning the `result` object.
pub fn request_ok(socket_path: &Path, json: &str) -> serde_json::Value {
    let response = send_request(socket_path, json);
    assert!(
        response.get("error").is_none(),
        "request failed: {json}\nresponse: {response}"
    );
    response["result"].clone()
}

/// Returns the error `code` of a response, or an empty string when it succeeded.
pub fn error_code(response: &serde_json::Value) -> String {
    response["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Opens an `events.subscribe` connection and returns its reader, positioned
/// before the `subscription_started` acknowledgement.
pub fn open_subscription(socket_path: &Path, json: &str) -> JsonLineReader {
    let mut reader = JsonLineReader::connect(socket_path);
    reader.send_line(json);
    reader
}

/// Drains the event stream until `expected` arrives, keeping every event seen
/// on the way in `seen` so ordering can be asserted afterwards.
pub fn wait_for_event(
    reader: &mut JsonLineReader,
    seen: &mut Vec<serde_json::Value>,
    expected: &str,
    timeout: Duration,
) -> serde_json::Value {
    wait_for_event_matching(reader, seen, expected, timeout, |_| true)
}

pub fn wait_for_event_matching<F>(
    reader: &mut JsonLineReader,
    seen: &mut Vec<serde_json::Value>,
    expected: &str,
    timeout: Duration,
    mut matches: F,
) -> serde_json::Value
where
    F: FnMut(&serde_json::Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        let Some(value) = reader.try_read_json_line(remaining) else {
            panic!(
                "timed out waiting for event {expected}; saw {:?}",
                event_kinds(seen)
            );
        };
        if value.get("event").is_some() {
            seen.push(value.clone());
        }
        if value["event"] == expected && matches(&value) {
            return value;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for event {expected}; saw {:?}",
                event_kinds(seen)
            );
        }
    }
}

/// Reads whatever else is already queued without blocking for long, so a test
/// can assert on the complete event stream at the end.
pub fn drain_events(
    reader: &mut JsonLineReader,
    seen: &mut Vec<serde_json::Value>,
    quiet_for: Duration,
) {
    while let Some(value) = reader.try_read_json_line(quiet_for) {
        if value.get("event").is_some() {
            seen.push(value);
        }
    }
}

pub fn event_kinds(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event["event"].as_str())
        .map(str::to_string)
        .collect()
}

pub fn events_of_kind<'a>(
    events: &'a [serde_json::Value],
    kind: &str,
) -> Vec<&'a serde_json::Value> {
    events
        .iter()
        .filter(|event| event["event"] == kind)
        .collect()
}

pub fn first_event_of_kind<'a>(
    events: &'a [serde_json::Value],
    kind: &str,
) -> &'a serde_json::Value {
    events
        .iter()
        .find(|event| event["event"] == kind)
        .unwrap_or_else(|| panic!("missing event {kind}; saw {:?}", event_kinds(events)))
}

/// Index of the first event of `kind` in the stream, used to assert ordering.
pub fn position_of_kind(events: &[serde_json::Value], kind: &str) -> usize {
    events
        .iter()
        .position(|event| event["event"] == kind)
        .unwrap_or_else(|| panic!("missing event {kind}; saw {:?}", event_kinds(events)))
}

/// Polls `probe` until it returns `Some`, or panics with `what` on timeout.
pub fn poll_until<T, F>(what: &str, timeout: Duration, interval: Duration, mut probe: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        thread::sleep(interval);
    }
}
