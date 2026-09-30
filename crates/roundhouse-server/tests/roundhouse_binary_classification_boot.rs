// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Boot-brief item 1: the real `roundhouse` binary's startup composition,
//! through `CARGO_BIN_EXE_roundhouse` rather than a reconstructed rig.
//!
//! `classification_runtime.rs`'s own `Rig` calls `classify_runtime::compose`
//! and `Engine::with_classifier` directly, which is the library seam and not
//! `main.rs`'s use of it -- a mutation that dropped `.with_classifier(runtime)`
//! or `runtime.supervise()` out of `main.rs` would be invisible to it. This
//! file spawns the actual compiled binary against a loopback classifier double
//! and drives it over its real HTTP surface, so what is exercised is the
//! wiring a deployment actually gets.
//!
//! Serving uses the binary's own built-in offline echo stub
//! (`ROUNDHOUSE_FRONTIER_UPSTREAM` left unset) rather than a second loopback
//! double: `main.rs`'s module doc names this as the zero-config path that
//! serves "no live inference", and the catalog it composes (`echo/echo`) is a
//! [`roundhouse_core::routing::Target::Frontier`] the admitted-pool check
//! accepts, which is all `request_classification` needs to fire. Only the
//! classifier is a real loopback service, and only a synthetic key ever
//! reaches it.
//!
//! The child's environment is cleared and rebuilt from nothing: this process
//! may carry ambient credentials, and none of them may reach a spawned
//! `roundhouse` by accident.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use roundhouse_core::classify::{ClassificationIntent, ClassificationRecord};
use roundhouse_core::event::{SessionEvent, SessionEventKind};
use roundhouse_server::test_support::classification::ANSWER;

const AUTH_ENV_NAME: &str = "ROUNDHOUSE_BINARY_BOOT_TEST_KEY";
const AUTH_ENV_VALUE: &str = "sk-roundhouse-binary-boot-test-synthetic";

// --------------------------------------------------------- classifier double

/// A loopback classifier, on its own multi-thread runtime so the rest of this
/// file can stay synchronous -- the child process and the raw HTTP client
/// below are both blocking, and mixing one async runtime already owned by
/// `#[tokio::test]` with `std::process::Child` plumbing buys nothing here.
///
/// **Multi-thread, not current-thread.** A current-thread `Runtime` only
/// drives its tasks while something on this thread is inside `block_on`; once
/// `start` returns, nothing would poll the spawned server again and the
/// double would accept a TCP connection and then never answer it. The
/// multi-thread flavor keeps its own worker threads running for as long as
/// the `Runtime` value is alive, which is what actually serves requests that
/// arrive after `start` has returned.
struct ClassifierDouble {
    addr: SocketAddr,
    calls: Arc<AtomicUsize>,
    _runtime: tokio::runtime::Runtime,
}

impl ClassifierDouble {
    fn start() -> Self {
        use axum::Router;
        use axum::body::Body;
        use axum::extract::State;
        use axum::response::Response;
        use axum::routing::post;

        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a multi-thread runtime for the classifier double");
        let handler_calls = Arc::clone(&calls);
        let addr = runtime.block_on(async move {
            async fn handle(State(calls): State<Arc<AtomicUsize>>) -> Response {
                calls.fetch_add(1, Ordering::SeqCst);
                Response::new(Body::from(ANSWER))
            }
            let app = Router::new()
                .route("/systemone", post(handle))
                .with_state(handler_calls);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback listener");
            let addr = listener.local_addr().expect("a bound address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            addr
        });
        Self {
            addr,
            calls,
            _runtime: runtime,
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

// ------------------------------------------------------------- config files

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "roundhouse-binary-classification-boot-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("a scratch directory");
    root
}

/// Write a classification config file the binary's own `ClassifyConfig::load`
/// accepts, pointed at `base_url`.
fn write_classify_config(dir: &Path, base_url: &str, enabled: bool) -> PathBuf {
    let path = dir.join("classify.json");
    let json =
        roundhouse_server::test_support::classification::classify_config_json(base_url, |value| {
            value["enabled"] = serde_json::json!(enabled);
            value["revision"] = serde_json::json!(1);
            value["auth"]["env"] = serde_json::json!(AUTH_ENV_NAME);
        });
    std::fs::write(&path, json).expect("the config file writes");
    path
}

// ------------------------------------------------------------- child process

/// Kills the child on drop, including on a panicking assertion: a test that
/// fails must not leave a `roundhouse` listening on a port for the rest of the
/// suite to trip over.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the real binary with a cleared environment, only the variables named
/// in `vars`, and stdout/stderr piped so the readiness line can be read back.
fn spawn_roundhouse(vars: &[(&str, &str)]) -> (ChildGuard, Arc<Mutex<Vec<String>>>) {
    let bin = env!("CARGO_BIN_EXE_roundhouse");
    let mut command = Command::new(bin);
    command.env_clear();
    command.env("RUST_LOG", "info");
    command.env("ROUNDHOUSE_ADDR", "127.0.0.1:0");
    command.env("PATH", "/usr/bin:/bin");
    for (name, value) in vars {
        command.env(name, value);
    }
    // `tracing_subscriber::fmt()`'s default writer is stdout, not stderr, and
    // main.rs does not configure otherwise -- the readiness line and every
    // boot-refusal diagnostic land here.
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("the built roundhouse binary spawns");
    let stdout: ChildStdout = child.stdout.take().expect("piped stdout");
    let lines = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&lines);
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            collected.lock().unwrap().push(strip_ansi(&line));
        }
    });
    (ChildGuard(child), lines)
}

/// Drop `\x1b[...m` SGR sequences. `tracing_subscriber::fmt()` colors its
/// output unconditionally (main.rs never calls `.with_ansi(false)`), so every
/// field separator this file greps for -- `addr=`, `roundhouse listening` --
/// otherwise carries an invisible escape between the letters that look
/// adjacent in a terminal.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// A stream's lines, collected live by a reader thread as the child writes
/// them, so a caller can inspect what has arrived so far without waiting for
/// the process to exit.
type CollectedLines = Arc<Mutex<Vec<String>>>;

/// Spawn the real binary as [`spawn_roundhouse`] does, but also capture
/// stderr: the `#[tokio::main]` termination handler prints a boot `Err`'s
/// `Debug` there, while `tracing_subscriber::fmt()` prints every log line —
/// including "roundhouse listening" — to stdout. server-6's claim is about
/// the *order* of those two streams' content, so a test for it needs both.
fn spawn_roundhouse_capturing_both(
    vars: &[(&str, &str)],
) -> (ChildGuard, CollectedLines, CollectedLines) {
    let bin = env!("CARGO_BIN_EXE_roundhouse");
    let mut command = Command::new(bin);
    command.env_clear();
    command.env("RUST_LOG", "info");
    command.env("ROUNDHOUSE_ADDR", "127.0.0.1:0");
    command.env("PATH", "/usr/bin:/bin");
    for (name, value) in vars {
        command.env(name, value);
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("the built roundhouse binary spawns");
    let stdout: ChildStdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let stdout_lines = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&stdout_lines);
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            collected.lock().unwrap().push(strip_ansi(&line));
        }
    });
    let stderr_lines = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&stderr_lines);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            collected.lock().unwrap().push(strip_ansi(&line));
        }
    });
    (ChildGuard(child), stdout_lines, stderr_lines)
}

/// Bounded: a binary that serves instead of refusing to boot is killed rather
/// than hung on, which turns "the fix regressed and the child now runs
/// forever" into a timeout with a clear message instead of a stalled suite.
fn wait_for_exit(guard: &mut ChildGuard, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = guard
            .0
            .try_wait()
            .expect("polling the child's status does not itself error")
        {
            return status;
        }
        if Instant::now() >= deadline {
            panic!(
                "the roundhouse binary did not exit within {timeout:?}; an enabled \
                 classifier with no credential should refuse to boot rather than serve"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for the "roundhouse listening" line and parse the address it names.
///
/// Bounded: a binary that never becomes ready fails the test with whatever it
/// printed instead of hanging the suite.
fn wait_for_listen_addr(lines: &Arc<Mutex<Vec<String>>>, timeout: Duration) -> SocketAddr {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let seen = lines.lock().unwrap();
            for line in seen.iter() {
                if let Some(after) = line.find("roundhouse listening")
                    && let Some(addr_at) = line[after..].find("addr=")
                {
                    let rest = &line[after + addr_at + "addr=".len()..];
                    let addr_str: String =
                        rest.chars().take_while(|c| !c.is_whitespace()).collect();
                    if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                        return addr;
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            let seen = lines.lock().unwrap();
            panic!(
                "the roundhouse binary never reported a listening address within \
                 {timeout:?}; stdout so far:\n{}",
                seen.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --------------------------------------------------------- raw HTTP client

/// Enough of HTTP/1.1 to drive the native transport synchronously: a
/// `Content-Length` or chunked response body, read with an overall deadline.
/// No new crate -- `reqwest`/`hyper` are not direct dependencies of this
/// package and this file may not edit the manifest to add one.
struct RawResponse {
    status: u16,
    body: Vec<u8>,
}

fn read_line(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Instant) -> String {
    loop {
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
            buf.drain(..pos + 2);
            return line;
        }
        fill(stream, buf, deadline);
    }
}

fn fill(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        panic!("the HTTP response from roundhouse did not complete within its deadline");
    }
    stream
        .set_read_timeout(Some(remaining.min(Duration::from_millis(200))))
        .expect("a read timeout sets");
    let mut chunk = [0u8; 8192];
    match stream.read(&mut chunk) {
        Ok(0) => {
            if buf.is_empty() {
                panic!("the connection to roundhouse closed with no response");
            }
            // EOF with data already buffered is handled by the caller, which
            // only loops back here when it still needs more bytes than are
            // present -- so treat this as "nothing more is coming" by padding
            // nothing; the caller's own loop condition decides whether that is
            // fatal.
        }
        Ok(n) => buf.extend_from_slice(&chunk[..n]),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut => {}
        Err(error) => panic!("reading from roundhouse: {error}"),
    }
}

fn read_exact_n(stream: &mut TcpStream, buf: &mut Vec<u8>, n: usize, deadline: Instant) -> Vec<u8> {
    while buf.len() < n {
        fill(stream, buf, deadline);
    }
    buf.drain(..n).collect()
}

/// One request, one response. Chunked bodies are decoded to their content in
/// full -- this is a test client for small fixtures, not a streaming reader.
fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> RawResponse {
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect(addr).expect("roundhouse accepts a connection");
    stream
        .set_write_timeout(Some(timeout))
        .expect("a write timeout sets");

    let payload = body.unwrap_or_default();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    request.push_str("\r\n");
    request.push_str(payload);
    stream
        .write_all(request.as_bytes())
        .expect("the request writes");

    let mut buf = Vec::new();
    let status_line = read_line(&mut stream, &mut buf, deadline);
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("an HTTP status line: {status_line:?}"));

    let mut headers = HashMap::new();
    loop {
        let line = read_line(&mut stream, &mut buf, deadline);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let body_bytes = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        let mut decoded = Vec::new();
        loop {
            let size_line = read_line(&mut stream, &mut buf, deadline);
            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_hex, 16)
                .unwrap_or_else(|_| panic!("a chunk size: {size_line:?}"));
            if size == 0 {
                // Trailing headers, if any, end with a blank line.
                loop {
                    let trailer = read_line(&mut stream, &mut buf, deadline);
                    if trailer.is_empty() {
                        break;
                    }
                }
                break;
            }
            let chunk = read_exact_n(&mut stream, &mut buf, size, deadline);
            decoded.extend_from_slice(&chunk);
            // Each chunk's data is followed by its own trailing CRLF.
            let _ = read_line(&mut stream, &mut buf, deadline);
        }
        decoded
    } else if let Some(len) = headers.get("content-length").and_then(|v| v.parse().ok()) {
        read_exact_n(&mut stream, &mut buf, len, deadline)
    } else {
        // No length given: read until the server closes the connection.
        loop {
            let before = buf.len();
            fill(&mut stream, &mut buf, deadline);
            if buf.len() == before {
                break;
            }
        }
        buf.clone()
    };

    RawResponse {
        status,
        body: body_bytes,
    }
}

/// Every `SessionEvent` carried as an SSE `data:` frame in a response body.
///
/// Frames axum cannot encode surface as `type: "error"`, which this simply
/// will not parse as a `SessionEvent` and skips -- a genuine encoding failure
/// would already fail the test's own assertions on what it expected to see.
fn sse_events(body: &[u8]) -> Vec<SessionEvent> {
    let text = String::from_utf8_lossy(body);
    text.lines()
        .filter_map(|line| {
            line.strip_prefix("data: ")
                .or_else(|| line.strip_prefix("data:"))
        })
        .filter_map(|payload| serde_json::from_str::<SessionEvent>(payload.trim()).ok())
        .collect()
}

// ------------------------------------------------------------------- turns

fn create_session(addr: SocketAddr) -> String {
    let response = request(
        addr,
        "POST",
        "/v1/sessions",
        Some("{}"),
        Duration::from_secs(5),
    );
    assert_eq!(response.status, 200, "creating a session must succeed");
    let parsed: serde_json::Value =
        serde_json::from_slice(&response.body).expect("a JSON session reply");
    parsed["session_id"]
        .as_str()
        .expect("a session_id")
        .to_string()
}

fn run_turn(addr: SocketAddr, session_id: &str, turn_id: &str, text: &str) -> Vec<SessionEvent> {
    let body = serde_json::json!({
        "turn_id": turn_id,
        "input": [{"role": "user", "text": text}],
    })
    .to_string();
    let response = request(
        addr,
        "POST",
        &format!("/v1/sessions/{session_id}/responses"),
        Some(&body),
        Duration::from_secs(10),
    );
    assert_eq!(response.status, 200, "the turn must be admitted");
    sse_events(&response.body)
}

fn intents(events: &[SessionEvent]) -> Vec<ClassificationIntent> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRequested { record } => Some(record.clone()),
            _ => None,
        })
        .collect()
}

fn results(events: &[SessionEvent]) -> Vec<ClassificationRecord> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::ClassificationRecorded { record } => Some(record.clone()),
            _ => None,
        })
        .collect()
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            panic!("condition never became true within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ----------------------------------------------------------------- claims

/// **The claim boot-brief item 1 names.** A deployment that enables
/// classification, through the real binary's own composition, reaches the
/// loopback service and the answer becomes durable metadata a later request
/// can see -- driven over the wire, so a mutation dropping `.with_classifier`
/// or `runtime.supervise()` out of `main.rs` turns this red.
///
/// **Why this polls across several turns rather than asserting on the
/// dispatching turn's own stream.** `Engine::run_turn` writes the
/// classification intent *after* the turn's own terminal event (`request_classification`
/// runs once the response is already settled, so the grant and the call stay
/// off the serving path -- see `engine.rs`'s own comment above that call). The
/// SSE follower behind `POST .../responses` ends the stream as soon as it has
/// forwarded the response it was opened for, which can be before that later
/// append lands on the wire. So the intent's own arrival is observed the
/// robust way already used to prove asynchrony elsewhere in this suite: by
/// polling the loopback double's call count, which can only have moved if
/// `prepare` → `record_classification_intent` → `spawn` all already ran.
/// Delivery is then polled across successive turns rather than assumed to
/// land on the very next one, for the same reason `deliver_classifier_output`
/// delivers only what is already parked -- a result that finishes its HTTP
/// round trip a few milliseconds late is still delivered, just not
/// necessarily by turn two.
#[test]
fn enabled_classification_reaches_the_loopback_service_and_becomes_durable_metadata() {
    let classifier = ClassifierDouble::start();
    let dir = scratch("enabled");
    let config_path = write_classify_config(&dir, &format!("http://{}", classifier.addr), true);

    let (guard, stdout) = spawn_roundhouse(&[
        (
            "ROUNDHOUSE_CLASSIFY_CONFIG",
            config_path.to_str().expect("a utf-8 path"),
        ),
        (AUTH_ENV_NAME, AUTH_ENV_VALUE),
    ]);
    let addr = wait_for_listen_addr(&stdout, Duration::from_secs(10));

    let session_id = create_session(addr);
    let first_events = run_turn(addr, &session_id, "t1", "fix the parser and prove it");
    assert!(
        results(&first_events).is_empty(),
        "nothing can have been delivered yet on the turn that bought it"
    );

    // Proof that the dispatching turn actually reached `spawn`: the call is on
    // the real loopback classifier's own counter, which only moves once the
    // durable intent is already committed (`request_classification` spawns
    // only after `record_classification_intent` returns `Ok`).
    wait_until(|| classifier.count() >= 1, Duration::from_secs(5));

    let mut delivered = Vec::new();
    for turn in 2..12 {
        let events = run_turn(
            addr,
            &session_id,
            &format!("t{turn}"),
            "now add a regression test",
        );
        delivered = results(&events);
        if !delivered.is_empty() {
            break;
        }
    }
    assert_eq!(
        delivered.len(),
        1,
        "a later turn's own writer must eventually deliver the result"
    );
    assert!(
        delivered[0].outcome.classification().is_some(),
        "the loopback classifier answered a complete, usable taxonomy: {delivered:?}"
    );

    drop(guard);
}

/// **The other half of the claim.** No configuration at all means no call and
/// no credential read -- the shipped state, and the control every
/// enabled-path assertion above needs to mean something.
#[test]
fn absent_configuration_makes_no_call_and_reads_no_credential() {
    let classifier = ClassifierDouble::start();

    // No ROUNDHOUSE_CLASSIFY_CONFIG at all, and no credential in the child's
    // environment for one to be read from even by accident.
    let (guard, stdout) = spawn_roundhouse(&[]);
    let addr = wait_for_listen_addr(&stdout, Duration::from_secs(10));

    let session_id = create_session(addr);
    let events = run_turn(addr, &session_id, "t1", "fix the parser");

    assert!(
        intents(&events).is_empty(),
        "no classifier configured, no intent"
    );
    assert!(results(&events).is_empty());
    assert_eq!(
        classifier.count(),
        0,
        "an unconfigured deployment must never reach the loopback classifier"
    );

    drop(guard);
}

/// **The same control, for a file that is present and says `enabled: false`.**
/// `classify_config::from_env`'s own contract is that this is the same runtime
/// state as no file at all.
#[test]
fn disabled_configuration_makes_no_call_and_reads_no_credential() {
    let classifier = ClassifierDouble::start();
    let dir = scratch("disabled");
    let config_path = write_classify_config(&dir, &format!("http://{}", classifier.addr), false);

    // No AUTH_ENV_NAME in the child's environment either: a disabled
    // configuration must never need a credential, only skip using it.
    let (guard, stdout) = spawn_roundhouse(&[(
        "ROUNDHOUSE_CLASSIFY_CONFIG",
        config_path.to_str().expect("a utf-8 path"),
    )]);
    let addr = wait_for_listen_addr(&stdout, Duration::from_secs(10));

    let session_id = create_session(addr);
    let events = run_turn(addr, &session_id, "t1", "fix the parser");

    assert!(intents(&events).is_empty());
    assert!(results(&events).is_empty());
    assert_eq!(classifier.count(), 0);

    drop(guard);
}

/// **server-6: an enabled classifier with no credential must refuse to boot
/// before the process ever announces it is listening.**
///
/// `compose` has refused a credential-less enabled file since M14.1; what
/// this proves is *when* the process learns that, relative to opening its
/// listening socket and telling an operator it did. Composing inside `serve`,
/// after the bind, meant "roundhouse listening" — the line an operator or a
/// supervisor greps for to know traffic can be sent — could appear before the
/// boot refusal that immediately follows it, the same shape every other boot
/// check in `main` (the catalog, the control plane, the directory) avoids by
/// running ahead of the bind.
#[test]
fn an_enabled_classifier_with_no_credential_refuses_to_boot_before_listening() {
    let classifier = ClassifierDouble::start();
    let dir = scratch("missing-credential");
    // A reachable base URL, deliberately never contacted: the refusal this
    // test is about happens before any HTTP client for it is even built.
    let config_path = write_classify_config(&dir, &format!("http://{}", classifier.addr), true);

    // AUTH_ENV_NAME is enabled-classifier's own credential variable, and it
    // is not in this list: env_clear() plus an omission is what makes it
    // genuinely absent from the child's environment.
    let (mut guard, stdout, stderr) = spawn_roundhouse_capturing_both(&[(
        "ROUNDHOUSE_CLASSIFY_CONFIG",
        config_path.to_str().expect("a utf-8 path"),
    )]);

    let status = wait_for_exit(&mut guard, Duration::from_secs(10));

    assert!(
        !status.success(),
        "a refused boot must exit non-zero, not zero: {status:?}"
    );
    let stdout = stdout.lock().unwrap();
    let stderr = stderr.lock().unwrap();
    assert!(
        !stdout
            .iter()
            .any(|line| line.contains("roundhouse listening")),
        "the process must never announce it is listening when it is about to \
         refuse to boot; stdout:\n{}",
        stdout.join("\n")
    );
    assert!(
        stderr.iter().any(|line| line.contains(AUTH_ENV_NAME)),
        "the refusal must name the missing credential variable, so a boot \
         failure for an unrelated reason cannot pass this test by accident; \
         stderr:\n{}",
        stderr.join("\n")
    );

    assert_eq!(
        classifier.count(),
        0,
        "never reached, since it never composed"
    );

    drop(guard);
}
