//! Counting the commands one test's calls cost, on a Redis other tests share.
//!
//! `INFO commandstats` cannot do this. Its counters are server-wide, so every
//! other test in the binary adds to them, and `CONFIG RESETSTAT` from another
//! binary (or another session on the same Redis) zeroes them mid-measurement.
//! Taking the minimum over several attempts does not rescue it: the attempts
//! run back to back within a few milliseconds, so one busy neighbour overlaps
//! all of them, and a foreign reset pushes the minimum *below* the true count.
//! `open_grant_and_settle_grant_are_single_round_trips` failed about one run
//! in ten under `--test-threads=4` for exactly the first reason.
//!
//! `MONITOR` is scoped by content instead of by time: it reports every command
//! the server executes, with its arguments, so a test that filters on an
//! identifier only it uses — a `fresh_principal`'s `proj_<hex>` — counts its
//! own commands and nothing else, whatever runs beside it.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;

use super::{raw_from_env, url_from_env};

/// One command the server executed, as `MONITOR` reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    /// The command name, upper-cased: `EVALSHA`, `HMGET`, ...
    pub command: String,
    /// Whether a script issued it. `MONITOR` tags those `[<db> lua]` rather
    /// than with a client address; they are not round trips.
    pub from_script: bool,
}

/// Runs `op` and returns what it produced together with every command the
/// server executed while it ran whose arguments contain `needle`.
///
/// **`needle` must be unique to the caller** — a `fresh_principal`'s project
/// id — or other tests' commands are counted too, which is the defect this
/// helper exists to remove.
///
/// The window is closed by an `ECHO` of a fresh marker sent after `op`
/// returns. `MONITOR` reports commands in the order the server executed them,
/// so once the marker arrives every command `op` caused has arrived before it.
/// The window opens when `MONITOR` has replied `OK`, which
/// `get_async_monitor` waits for.
pub async fn commands_naming<T>(needle: &str, op: impl Future<Output = T>) -> (T, Vec<Executed>) {
    let client = redis::Client::open(url_from_env().as_str()).unwrap();
    let mut monitor = client
        .get_async_monitor()
        .await
        .expect("the test Redis must accept MONITOR");

    let output = op.await;

    let marker = format!("rhtest-monitor-end-{}", uuid::Uuid::new_v4().simple());
    let mut raw = raw_from_env().await;
    let _: String = redis::cmd("ECHO")
        .arg(&marker)
        .query_async(&mut raw)
        .await
        .unwrap();

    let mut executed = Vec::new();
    let mut lines = monitor.on_message::<String>();
    // Bounded: a stream that never delivers the marker must fail the test,
    // not hang the runner.
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next().await {
            if line.contains(&marker) {
                return;
            }
            executed.extend(parse(&line, needle));
        }
        panic!("the MONITOR stream closed before the end marker {marker} arrived");
    })
    .await
    .expect("the end marker must arrive on the MONITOR stream within 30s");
    (output, executed)
}

/// The top-level commands in `executed`: the round trips a client made.
pub fn round_trips(executed: &[Executed]) -> Vec<&str> {
    executed
        .iter()
        .filter(|e| !e.from_script)
        .map(|e| e.command.as_str())
        .collect()
}

/// One `MONITOR` line, `<time> [<db> <client-or-lua>] "<CMD>" "<arg>" ...`,
/// if its arguments contain `needle`.
fn parse(line: &str, needle: &str) -> Option<Executed> {
    let (_, rest) = line.split_once('[')?;
    let (origin, args) = rest.split_once("] ")?;
    if !args.contains(needle) {
        return None;
    }
    let command = args.split('"').nth(1)?.to_ascii_uppercase();
    Some(Executed {
        command,
        from_script: origin.ends_with(" lua"),
    })
}

/// Script calls from a client that is not the one under measurement, issued
/// continuously until [`ForeignTraffic::stop`].
///
/// This is what another test in the same binary looks like to the server. A
/// round-trip test that runs its measurement under this traffic proves its
/// count is its own, rather than hoping the neighbours happen to be quiet.
pub struct ForeignTraffic {
    issued: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl ForeignTraffic {
    /// Starts the traffic and returns once the server has run its first call,
    /// so a measurement taken after this is taken under it.
    pub async fn start() -> Self {
        let issued = Arc::new(AtomicU64::new(0));
        let mut raw = raw_from_env().await;
        let counter = issued.clone();
        let task = tokio::spawn(async move {
            loop {
                let _: i64 = redis::cmd("EVAL")
                    .arg("return 1")
                    .arg(0)
                    .query_async(&mut raw)
                    .await
                    .unwrap();
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        while issued.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        Self { issued, task }
    }

    /// Stops the traffic and returns how many calls it made.
    pub fn stop(self) -> u64 {
        self.task.abort();
        self.issued.load(Ordering::Relaxed)
    }
}
