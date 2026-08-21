// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The checks neither file can make alone, asked at boot and at every write.
//!
//! `ROUNDHOUSE_CATALOG` and `ROUNDHOUSE_CONTROL_PLANE` are loaded by two
//! loaders that cannot see each other: a [`TargetFilter`] cannot tell at parse
//! time that its patterns name no model this deployment has, and a quality
//! floor cannot tell that it sits above every prior in the catalog. Only where
//! both are loaded can the question be asked, which used to make the
//! composition root the only place it *could* be asked — and so these functions
//! lived in `main.rs`.
//!
//! They live here now because the admin plane made "boot" the wrong noun. A
//! runtime-minted key must be exactly as validated as a boot-loaded one, or the
//! first `POST /v1/admin/...` becomes a way to write a configuration the
//! process would have refused to start under. [`CrossChecks`] is the value that
//! carries the catalog half so a write can re-ask; `main.rs` builds one and
//! [`ControlDirectory`](super::directory::ControlDirectory) holds it.
//!
//! **The list is one list.** A fourth check added below is asked at boot and at
//! every write on the same line, because the failure being designed out is a
//! deployment that refuses to *start* under a configuration an operator can
//! nevertheless `PATCH` it into.
//!
//! [`TargetFilter`]: roundhouse_core::control::TargetFilter

use anyhow::Result;
use roundhouse_core::control::TurnBudget;
use roundhouse_core::routing::Candidate;
use roundhouse_fleet::FrontierModelSpec;

use super::{Admission, ControlPlane};

/// Refuse to serve a key whose policy admits nothing this process can route to.
///
/// The catalog and the control plane are separate files, so neither loader can
/// see the other: a `TargetFilter` cannot tell at parse time that its patterns
/// name no model, and a quality floor cannot tell that it sits above every
/// model in the catalog. Here both are loaded, which makes this the one place
/// the question can be asked.
///
/// Asking it is the same load-or-die posture both loaders already take. A
/// policy that admits nothing does not degrade — every turn it serves ends in
/// `policy_refused` — so starting anyway would turn one mistyped pattern into
/// a tenant whose every request fails, discovered by the tenant.
///
/// Per key rather than per project, and that is not a shortcut: a key's
/// effective policy is its project's narrowed by its own overrides, so a
/// project whose filter is fine can still hold a key whose override intersects
/// it down to nothing — and a turn arrives on a key.
///
/// The question is [`TurnPolicy::permits`] and deliberately not
/// [`TurnPolicy::admits`]: this asks whether a target is reachable *at all*
/// under the policy's history-independent axes, and a cadence-rationed model
/// is reachable on some turns. Feeding `admits` a synthetic unspent window to
/// get the same answer is how this used to be written, and it left the reader
/// to work out from a fabricated [`FrontierHistory`] which question was being
/// asked. What a *spent* window leaves is the separate question
/// [`refuse_promises_of_a_local_fallback`] asks, one call below.
///
/// [`TurnPolicy::permits`]: roundhouse_core::control::TurnPolicy::permits
/// [`TurnPolicy::admits`]: roundhouse_core::control::TurnPolicy::admits
/// [`FrontierHistory`]: roundhouse_core::control::FrontierHistory
pub fn refuse_policies_that_admit_nothing(
    plane: &ControlPlane,
    reachable: &[Candidate],
) -> anyhow::Result<()> {
    // Collected and sorted rather than reported on the first hit: the table is
    // a hash map, so a deployment with two bad entries would otherwise be told
    // about a different one on each restart. `configured_admissions` yields
    // nothing in open mode, which is the accurate answer — every request there
    // resolves to the unrestricted policy, and there is nothing to disagree
    // with.
    let mut refused: Vec<String> = plane
        .configured_admissions()
        .filter(|admission| {
            !reachable
                .iter()
                .any(|candidate| admission.policy.permits(candidate))
        })
        .map(describe)
        .collect();
    refused.sort();
    if !refused.is_empty() {
        anyhow::bail!(
            "these control-plane keys admit none of the {} model(s) this deployment can route to, \
             so every one of their turns would fail: {}",
            reachable.len(),
            refused.join("; ")
        );
    }
    Ok(())
}

/// How a refusal names the key an operator has to go and edit.
///
/// One spelling for both checks below. A digest tells an operator that two
/// keys differ and never which one they mistyped, so the patterns go in beside
/// it.
fn describe(admission: &Admission) -> String {
    format!(
        "project `{}`, user `{}` (policy {}, allow {})",
        admission.principal.project,
        admission.principal.user,
        admission.policy.digest(),
        admission.policy.allow,
    )
}

/// What a [`FrontierCadence`] promises about a window it has spent.
///
/// [`FrontierCadence`]: roundhouse_core::control::FrontierCadence
const CADENCE_PROMISE: &str = "its frontier_cadence promises that a spent window serves locally \
     instead of failing, and this deployment has no local capacity to serve it";

/// What a degrade-mode [`Budget`] with the overflow valve off promises about a
/// limit it has spent.
///
/// [`Budget`]: roundhouse_core::control::Budget
const BUDGET_PROMISE: &str = "its budget degrades to local with overflow_when_local_saturated off, \
     which promises that an exhausted budget serves locally instead of failing, and this \
     deployment has no local capacity to serve it";

/// What a *stored*-key credential mode promises about a member who has
/// attached nothing.
///
/// Stored only, and pass-through is exempt for a reason written out at the
/// check: a mode that reads a tier the file leaves empty is unreachable as a
/// structural fact this boot can see, while a mode whose credential arrives on
/// the request is unreachable only until a request arrives.
///
/// [`CredentialMode`]: roundhouse_core::control::CredentialMode
const CREDENTIAL_PROMISE: &str = "its credential mode reaches no hosted provider on this \
     deployment -- either every provider its keys name is one this process cannot route to, or \
     its mode leaves it with no key at all -- which promises that an unreachable provider serves \
     locally instead of failing, and this deployment has no local capacity to serve it";

/// What a project's `"validate"` block promises, and what keeping it needs.
///
/// [`ValidationTerms`]: roundhouse_server::ValidationTerms
const VALIDATION_PROMISE: &str = "its validate block enrols this project's sessions in the validate/steer loop, which \
     needs a judge -- and no reachable catalog model is named by ROUNDHOUSE_JUDGE_MODEL, so \
     every validation would be skipped as unavailable and the arm comparison the enrolment \
     exists to produce would be empty";
/// Every promise this key's configuration makes about a *spent* allowance that
/// this deployment cannot keep.
///
/// **Two configurations, one promise, one check.** A cadence spends a
/// per-session ration and a degrade-mode budget spends money, but both say the
/// same sentence when their allowance runs out — *the hosted options go
/// inadmissible and the turn serves locally instead of failing* — and both say
/// it in a file that cannot see a fleet. Whether the sentence is true depends
/// on one fact either way: is anything this key may reach still admissible
/// once the allowance is gone? Asking it twice, in two functions with two
/// lookalike error messages, would be two spellings of one question, and the
/// second one is where the answers start to differ.
///
/// Each promise is asked in the vocabulary of the thing that made it, through
/// the same predicate the router will apply at runtime rather than a
/// restatement of it — [`TurnPolicy::admits_when_spent`] for the cadence, and
/// [`TurnBudget::exhausted`] plus [`TurnPolicy::permits`] for the budget.
/// A key that makes neither promise is asked nothing, which is why a
/// deployment with no cadences and no budgets is unaffected by this check.
///
/// The budget half asks `permits` and not `admits_when_spent`: an exhausted
/// budget and a spent cadence are separate allowances, and a key that has run
/// out of one has not necessarily run out of the other. Where a key really does
/// exhaust both, the cadence half of this same list has already refused it.
///
/// [`TurnPolicy::admits_when_spent`]: roundhouse_core::control::TurnPolicy::admits_when_spent
/// [`TurnPolicy::permits`]: roundhouse_core::control::TurnPolicy::permits
fn unkeepable_promises(
    admission: &Admission,
    reachable: &[Candidate],
    judge: Option<&FrontierModelSpec>,
) -> Vec<&'static str> {
    let mut broken = Vec::new();
    // The credential half, and it is here rather than at the config boundary
    // for the reason the other three are: `config.rs` refuses a *variable this
    // process does not have* -- an unset `env_var` stops the boot naming the
    // variable, which is the loud half and needs no catalog. What it cannot see
    // is whether the providers a key can authenticate to are providers this
    // deployment can route to at all, because that is the catalog's half, and
    // the two files cannot see each other. Only here are both loaded.
    //
    // Asked through the same `reachable` the router will apply at runtime
    // rather than a restatement of it, exactly as the cadence and budget halves
    // are.
    //
    // **Of stored modes only, because pass-through's answer is not a
    // boot-knowable one.** A stored mode names tiers a file either fills or
    // does not, so "this key reaches nothing" is a fact about the file and this
    // is the first and last moment anything can see it. A forwarding mode holds
    // no key at all: the credential is the caller's and arrives on the request,
    // so `Resolution::Forwarding { presented: None }` is what *every*
    // pass-through admission looks like before any request exists. Asked here,
    // it reads as "reaches nothing" and refuses every pass-through project on
    // every deployment — the mode this milestone exists for, undeployable, with
    // the boot check as the only thing stopping it.
    //
    // What is boot-knowable for pass-through is whether there is any hosted
    // provider a forwarded credential *could* cover, and a non-empty catalog is
    // that question already answered — the same `!reachable.is_empty()` guard
    // this check already carries. The per-request half is asked per request, by
    // the same filter, and a turn whose caller presented nothing degrades to
    // local with `withheld_providers` naming the provider.
    if !admission.credentials.is_forwarding()
        && admission
            .credentials
            .reachable(reachable.to_vec())
            .candidates
            .is_empty()
        && !reachable.is_empty()
    {
        broken.push(CREDENTIAL_PROMISE);
    }
    // The promise that is not about a *spent* allowance — it is in this list
    // anyway because it is the same sentence with the same remedy shape: a
    // config says something will happen, and this is the first moment the
    // *catalog* can be compared against it. Splitting it into its own boot
    // check would give an operator two lookalike refusals to tell apart, which
    // is exactly what folding the cadence and the budget into one list already
    // refused to do.
    //
    // **What is checked is that a judge resolves, and that is all.** Whether
    // the side call it makes can *authenticate* is not asked here and is
    // deliberately not a boot promise: `FleetJudge` resolves no credential of
    // its own — see `judge.rs`, where `TurnCredential::Absent` is written out
    // as the honest state rather than an oversight — so on a deployment
    // composing a real provider client the checks are refused at dispatch. That
    // is a **runtime fail-open**, and it is the M6 interject contract holding
    // rather than a gap this check should close: an unreachable judge abandons
    // its side call as `Unreachable` and the turn it was checking proceeds
    // unchanged, because the checker never breaks the checked. Refusing to boot
    // over it would take a deployment down for a check that costs it nothing.
    // See `tests/validate_loop.rs`:
    // `a_judge_that_cannot_authenticate_abandons_its_check_and_the_turn_proceeds`.
    if admission.validation.is_some() && judge.is_none() {
        broken.push(VALIDATION_PROMISE);
    }
    if admission.policy.frontier_cadence.is_some()
        && !reachable
            .iter()
            .any(|candidate| admission.policy.admits_when_spent(candidate))
    {
        broken.push(CADENCE_PROMISE);
    }
    if let Some(terms) = &admission.budget {
        // Only one exhaustion setting promises local service at all: `Refuse`
        // never made the promise, and the valve keeps it on frontier. See
        // `Exhaustion::promises_local_service`.
        if terms.budget.on_exhaustion.promises_local_service() {
            let spent = TurnBudget::exhausted(terms.budget.on_exhaustion);
            if !reachable
                .iter()
                .any(|candidate| admission.policy.permits(candidate) && spent.admits(candidate))
            {
                broken.push(BUDGET_PROMISE);
            }
        }
    }
    broken
}

/// Refuse to serve a key that promises a local fallback this deployment cannot
/// provide.
///
/// The promise is checked where the fleet is finally visible, which is the
/// same place [`refuse_policies_that_admit_nothing`] checks the other half.
/// Those two stay separate functions because they are separate questions with
/// separate remedies: that one says "this policy names nothing at all", this
/// one says "this configuration names something for as long as an allowance
/// lasts". Reported together would leave an operator unsure which sentence to
/// go and edit. What is *not* separate is the pair of promises inside this
/// one — see [`unkeepable_promises`].
pub fn refuse_promises_of_a_local_fallback(
    plane: &ControlPlane,
    reachable: &[Candidate],
    judge: Option<&FrontierModelSpec>,
) -> anyhow::Result<()> {
    let mut refused: Vec<String> = plane
        .configured_admissions()
        .filter_map(|admission| {
            let broken = unkeepable_promises(admission, reachable, judge);
            (!broken.is_empty())
                .then(|| format!("{} — {}", describe(admission), broken.join("; and ")))
        })
        .collect();
    refused.sort();
    if !refused.is_empty() {
        anyhow::bail!(
            "these control-plane keys promise this deployment something it cannot deliver, so \
             their turns would fail or their configuration would silently do nothing: {}",
            refused.join(" | ")
        );
    }
    Ok(())
}

/// The catalog half of every cross-check, carried so a *write* can re-ask what
/// the boot asked.
///
/// A value rather than two arguments threaded through the admin plane, because
/// the two travel together by definition: `reachable` is what this deployment
/// can route to and `judge` is which of those models checks the others, and a
/// call site holding one without the other would be asking half the question.
///
/// Cheap to clone once, expensive to recompute — the list is quoted out of the
/// catalog at startup and does not move afterwards, since this process attaches
/// no fleet that could join or leave. A deployment that grows one rebuilds this
/// value where it builds the fleet, at the same site
/// `main.rs::reachable_candidates` is called; the alternative — re-quoting per
/// write — would let two admin writes seconds apart be judged against two
/// different catalogs with nothing recording that they were.
#[derive(Debug, Clone)]
pub struct CrossChecks {
    reachable: Vec<Candidate>,
    judge: Option<FrontierModelSpec>,
}

/// Which check refused, and the sentence an operator reads.
///
/// The pair rather than one string: at boot the detail *is* the message and the
/// name would be noise, while an admin write answers `422` and the name is what
/// tells a caller which of several checks their body tripped. Producing both
/// and letting each surface use what it needs is what keeps the two from being
/// two differently-worded refusals of the same configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossCheckRefusal {
    /// The check's own name, as a caller would grep for it.
    pub check: &'static str,
    /// What it said, verbatim — the same words the boot refusal prints.
    pub detail: String,
}

impl CrossChecks {
    pub fn new(reachable: Vec<Candidate>, judge: Option<FrontierModelSpec>) -> Self {
        Self { reachable, judge }
    }

    /// Every target this deployment can route to, as the router prices them.
    ///
    /// Borrowed rather than cloned: the admin plane's reconciliation view reads
    /// the same list, and a copy per read would be a second answer to "what can
    /// this deployment reach" for a caller to hold past its refresh.
    pub fn reachable(&self) -> &[Candidate] {
        &self.reachable
    }

    /// Refuse a plane no boot of this deployment would have started under.
    ///
    /// **The one list**, called from the composition root at startup and from
    /// [`ControlDirectory`](super::directory::ControlDirectory) after every
    /// mutation compiles. The order is the order an operator meets them in a
    /// startup log, and it is preserved so the same misconfiguration reports the
    /// same sentence whichever door it came in through.
    ///
    /// The third check is read out of [`mcp_api`](crate::mcp_api) rather than
    /// re-spelled here: it is the control surface's own sentence about a
    /// membership it cannot describe, and a second wording of it would be a
    /// second refusal for an operator to tell apart.
    pub fn refuse(&self, plane: &ControlPlane) -> Result<(), CrossCheckRefusal> {
        refuse_policies_that_admit_nothing(plane, &self.reachable).map_err(|error| {
            CrossCheckRefusal {
                check: "refuse_policies_that_admit_nothing",
                detail: error.to_string(),
            }
        })?;
        refuse_promises_of_a_local_fallback(plane, &self.reachable, self.judge.as_ref()).map_err(
            |error| CrossCheckRefusal {
                check: "refuse_promises_of_a_local_fallback",
                detail: error.to_string(),
            },
        )?;
        if let Some(detail) = crate::mcp_api::describe_ambiguous_memberships(plane) {
            return Err(CrossCheckRefusal {
                check: "ambiguous_memberships",
                detail,
            });
        }
        Ok(())
    }
}
