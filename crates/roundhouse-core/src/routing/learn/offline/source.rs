// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reading the stores, and the run that ties a calibration together.
//!
//! **Enumeration is the source index, never a scan.** Learner sessions are
//! the permanent marks `SessionStore::learning_sessions` pages through, the
//! same index the recovery task and the audit read. The append
//! that writes an entry-producing event writes its mark in the same step, so
//! the list holds every session with a learning entry, whatever the learner
//! store did. A member whose mark the store cannot read has no project to
//! file it under, so it is counted as excluded and named, never dropped.
//!
//! **The manifest is the cutoff.** Each session's log is read to
//! the sequence the manifest pins, or to its end when nothing is pinned, and
//! [`InputManifest`] records the ids, the last sequence read and the SHA-256
//! of the event bytes. A pinned run whose log no longer matches its digest is
//! refused rather than reported on different evidence under the same name.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::drift::{self, DriftCheck};
use super::extract::Evidence;
use super::report::Report;
use super::write::artifact_bytes;
use super::{CalibrationConfig, CalibrationError};
use crate::control::ProjectId;
use crate::event::SessionEvent;
use crate::ids::SessionId;
use crate::learn_store::LearnerStore;
use crate::routing::learn::artifact::Artifact;
use crate::store::{LearningCursor, SessionStore};

/// Members per index page. An implementation bound, not a policy: a pass
/// reads every page.
pub const ENUMERATION_PAGE: usize = 256;

/// Events per log read.
const READ_BATCH: usize = 1024;

/// One session's log, to the manifest's cutoff.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionLog {
    pub session: SessionId,
    pub events: Vec<SessionEvent>,
}

/// One session's cutoff: the last sequence read, and the digest of the event
/// bytes through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutoffEntry {
    pub session: SessionId,
    pub through_seq: u64,
    pub sha256: String,
}

/// The evaluation cutoff: which logs, read how far, with which bytes.
///
/// **Its digest names every result.** The artifact records it, and the report
/// prints it first, so a promotion approval can name the evidence it read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputManifest {
    pub project: ProjectId,
    /// In byte order of the session ids.
    pub sessions: Vec<CutoffEntry>,
}

impl InputManifest {
    /// The manifest of `logs`, which must be in byte order of their ids.
    pub fn of(project: ProjectId, logs: &[SessionLog]) -> Self {
        Self {
            project,
            sessions: logs
                .iter()
                .map(|log| CutoffEntry {
                    session: log.session.clone(),
                    through_seq: log.events.last().map_or(0, |event| event.seq),
                    sha256: log_digest(&log.events),
                })
                .collect(),
        }
    }

    /// The bytes a run writes, and the bytes the digest hashes.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("the manifest is plain data");
        bytes.push(b'\n');
        bytes
    }

    pub fn digest(&self) -> String {
        hex::encode(Sha256::digest(self.bytes()))
    }
}

/// SHA-256 over each event's JSON, one line per event.
///
/// Stable because `serde_json`'s map order is sorted in this workspace and
/// every event type serializes its fields in declaration order.
pub fn log_digest(events: &[SessionEvent]) -> String {
    let mut hasher = Sha256::new();
    for event in events {
        hasher.update(serde_json::to_vec(event).expect("events are plain data"));
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// What the enumeration found besides the logs it read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Census {
    /// Marked sessions of the project.
    pub project_sessions: u64,
    /// Marked sessions of other projects, which this run does not read.
    pub other_projects: u64,
    /// Index members whose stored mark the store cannot read. Their project
    /// is unknowable, so they are excluded and named: an exclusion cause, not
    /// a silent drop.
    pub unreadable: Vec<SessionId>,
    /// Marked sessions of the project that a pinned cutoff does not name:
    /// marked after the manifest was written.
    pub after_cutoff: Vec<SessionId>,
}

/// The logs a run reads, the cutoff they make, and the enumeration census.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub logs: Vec<SessionLog>,
    pub input: InputManifest,
    pub census: Census,
}

impl Source {
    /// A source over logs already in hand, in byte order of their ids.
    pub fn from_logs(project: ProjectId, logs: Vec<SessionLog>) -> Self {
        let input = InputManifest::of(project, &logs);
        Self {
            census: Census {
                project_sessions: logs.len() as u64,
                ..Census::default()
            },
            logs,
            input,
        }
    }
}

/// Enumerate the project's marked sessions and read each log to its cutoff.
/// Reads only.
pub async fn read_source<S: SessionStore + ?Sized>(
    store: &S,
    project: &ProjectId,
    cutoff: Option<&[CutoffEntry]>,
) -> Result<Source, CalibrationError> {
    let page_size = NonZeroUsize::new(ENUMERATION_PAGE).expect("a positive page");
    let mut census = Census::default();
    let mut marked = Vec::new();
    let mut cursor: Option<LearningCursor> = None;
    loop {
        let page = store.learning_sessions(cursor.as_ref(), page_size).await?;
        census.unreadable.extend(page.unreadable);
        for session in page.sessions {
            if session.project == *project {
                marked.push(session.session_id);
            } else {
                census.other_projects += 1;
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    marked.sort();
    marked.dedup();
    census.unreadable.sort();
    census.unreadable.dedup();
    census.project_sessions = marked.len() as u64;

    let pins: Option<BTreeMap<&SessionId, &CutoffEntry>> =
        cutoff.map(|pins| pins.iter().map(|pin| (&pin.session, pin)).collect());
    let mut logs = Vec::with_capacity(marked.len());
    for session in marked {
        let pin = match &pins {
            Some(pins) => match pins.get(&session) {
                Some(pin) => Some(*pin),
                None => {
                    census.after_cutoff.push(session);
                    continue;
                }
            },
            None => None,
        };
        let events = read_log(store, &session, pin.map(|pin| pin.through_seq)).await?;
        if let Some(pin) = pin {
            let through = events.last().map_or(0, |event| event.seq);
            if through != pin.through_seq || log_digest(&events) != pin.sha256 {
                return Err(CalibrationError::Cutoff(format!(
                    "the log of `{session}` through seq {} does not match the manifest \
                     (read through seq {through})",
                    pin.through_seq
                )));
            }
        }
        logs.push(SessionLog { session, events });
    }
    if let Some(pins) = &pins {
        let read: Vec<&SessionId> = logs.iter().map(|log| &log.session).collect();
        if let Some(missing) = pins.keys().find(|pinned| !read.contains(pinned)) {
            return Err(CalibrationError::Cutoff(format!(
                "`{missing}` is in the manifest cutoff and is not a marked session of the project"
            )));
        }
    }
    let input = InputManifest::of(project.clone(), &logs);
    Ok(Source {
        logs,
        input,
        census,
    })
}

async fn read_log<S: SessionStore + ?Sized>(
    store: &S,
    session: &SessionId,
    through: Option<u64>,
) -> Result<Vec<SessionEvent>, CalibrationError> {
    let mut events = Vec::new();
    let mut after = 0;
    loop {
        let batch = store.read_events(session, after, READ_BATCH).await?;
        if batch.is_empty() {
            return Ok(events);
        }
        for event in batch {
            if through.is_some_and(|through| event.seq > through) {
                return Ok(events);
            }
            after = event.seq;
            events.push(event);
        }
    }
}

/// A finished calibration.
#[derive(Debug, Clone)]
pub struct Calibrated {
    /// The artifact bytes, exactly as written.
    pub artifact: Vec<u8>,
    /// The artifact as the server's loader parses it.
    pub parsed: Artifact,
    /// The cutoff the run read, to pin a rerun to.
    pub input: InputManifest,
    pub evidence: Evidence,
    pub report: Report,
}

/// One calibration run: enumerate, read, replay, check drift when a
/// point-in-time copy of the learner store is given, write, report.
///
/// `copy` must be a point-in-time copy. A live learner store changes while
/// the logs are read, so a comparison against it would report drift that is
/// only the read's own lag; without a copy the report says the check did not
/// run.
pub async fn calibrate<S: SessionStore + ?Sized>(
    config: &CalibrationConfig,
    store: &S,
    copy: Option<&dyn LearnerStore>,
    source_commit: &str,
) -> Result<Calibrated, CalibrationError> {
    let source = read_source(store, &config.project, config.cutoff.as_deref()).await?;
    let evidence = Evidence::extract(config, &source.logs);
    let drift = match copy {
        Some(copy) => DriftCheck::Ran(drift::check(copy, &config.project, &evidence).await?),
        None => DriftCheck::NotRun,
    };
    assemble(config, source, evidence, drift, source_commit)
}

/// The pure end of a run: the artifact from the evidence, and the report.
pub fn assemble(
    config: &CalibrationConfig,
    source: Source,
    evidence: Evidence,
    drift: DriftCheck,
    source_commit: &str,
) -> Result<Calibrated, CalibrationError> {
    let artifact = artifact_bytes(
        &config.strategies,
        &evidence.prior,
        &source.input.digest(),
        source_commit,
    );
    let parsed = Artifact::parse(&artifact)?;
    let report = Report::build(config, &source.census, &evidence, drift, &parsed);
    Ok(Calibrated {
        artifact,
        parsed,
        input: source.input,
        evidence,
        report,
    })
}
