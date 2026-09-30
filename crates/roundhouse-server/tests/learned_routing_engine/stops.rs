// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Delivery stops: a refusal no retry fixes stops the one session whose page
//! holds the refused entry, for as long as the process runs, and no other.
//!
//! **Per session, whatever the epoch.** A refused entry stays in its
//! session's page under the epoch it was written in. A stop keyed by the
//! project and the admission's epoch would be met again under the next
//! epoch, when the same page is resent and refused again, and it would stop
//! every other session of the project with it.

use roundhouse_core::ids::SessionId;
use roundhouse_core::learn_store::{LearnerError, LearnerStore};
use roundhouse_core::routing::learn::{EpochId, LearnerMode, LearnerTerms};

use crate::rig::{ApplyScript, Rig, RigConfig, admission, delivery, project, terms};

fn under(epoch: EpochId) -> LearnerTerms {
    LearnerTerms {
        epoch,
        ..terms(LearnerMode::Shadow)
    }
}

fn malformed() -> LearnerError {
    LearnerError::Malformed {
        reason: "an entry no store takes".into(),
    }
}

/// A session refused `Malformed` under one epoch is resent under the next,
/// refused again, and stops only itself: another session of the project
/// delivers under the new epoch.
#[tokio::test]
async fn a_malformed_session_under_a_new_epoch_stops_only_itself() {
    let rig = Rig::new(RigConfig::default());
    let stuck = SessionId::new("broken/ada/s");
    rig.learner.refuse_session(&stuck, malformed());
    rig.turn(
        &stuck,
        "t1",
        &admission("broken", Some(under(EpochId::new([0xa1; 16])))),
    )
    .await
    .expect("served");
    assert_eq!(rig.learner.applies(), 1);

    let renewed = admission("broken", Some(under(EpochId::new([0xb2; 16]))));
    rig.turn(&stuck, "t2", &renewed).await.expect("served");
    let other = SessionId::new("broken/ada/other");
    rig.turn(&other, "t1", &renewed).await.expect("served");
    assert_eq!(
        rig.acknowledged(&other).await.len(),
        1,
        "the other session delivered under the new epoch"
    );
    assert!(
        rig.learner
            .inner
            .watermark(&project("broken"), &other)
            .await
            .expect("a memory watermark")
            > 0,
        "the store applied the other session's entry"
    );
    assert_eq!(
        rig.learner.applies(),
        2,
        "the stopped session was not resent under the new epoch"
    );
    assert_eq!(delivery(&rig).stopped, 1);
}

/// A session stopped on `Malformed` stays stopped on every later turn, and
/// its entries stay owed.
#[tokio::test]
async fn a_malformed_stop_holds_across_later_turns() {
    let rig = Rig::new(RigConfig::default());
    let session = SessionId::new("held/ada/s");
    let admission = admission("held", Some(terms(LearnerMode::Shadow)));
    rig.learner.script(ApplyScript::Refuse(malformed()));
    rig.turn(&session, "t1", &admission).await.expect("served");
    assert_eq!(rig.learner.applies(), 1);
    // Two more turns: a stop that is spent by the first check after it would
    // let the third call the store again.
    rig.turn(&session, "t2", &admission).await.expect("served");
    rig.turn(&session, "t3", &admission).await.expect("served");
    assert_eq!(
        rig.learner.applies(),
        1,
        "the stopped session calls the store no more"
    );
    assert!(rig.acknowledged(&session).await.is_empty());
    assert_eq!(delivery(&rig).stopped, 1);
}
