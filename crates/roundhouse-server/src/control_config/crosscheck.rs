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
use roundhouse_core::routing::{Candidate, Tier};
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
/// One spelling for every check below. A digest tells an operator that two
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
/// **Cause-neutral wording, and it has to be.** Two configurations can leave a
/// spent window with nothing local to serve from — a deployment with no local
/// capacity at all, and one whose `tiers` recipe names none of the capacity it
/// has (M10 review G02) — and a sentence naming only the first would be false
/// for the second. One word carries it: *this key* has no local capacity, not
/// *this deployment*, because capacity a key's own recipe cannot select is
/// capacity that key does not have. Which of the two files to edit is said by
/// [`TIERS_NAME_NO_LOCAL_FALLBACK`], appended only when the recipe is the
/// difference.
///
/// [`FrontierCadence`]: roundhouse_core::control::FrontierCadence
const CADENCE_PROMISE: &str = "its frontier_cadence promises that a spent window serves locally \
     instead of failing, and this key has no local capacity to serve it";

/// What a degrade-mode [`Budget`] with the overflow valve off promises about a
/// limit it has spent.
///
/// [`Budget`]: roundhouse_core::control::Budget
const BUDGET_PROMISE: &str = "its budget degrades to local with overflow_when_local_saturated off, \
     which promises that an exhausted budget serves locally instead of failing, and this key has \
     no local capacity to serve it";

/// Which of the two files a broken spent-allowance promise sends an operator
/// to, when the answer is the recipe rather than the fleet.
///
/// Said as its own sentence rather than folded into the two constants above:
/// the promise and its cause have different remedies — add local capacity, or
/// name the capacity you have — and the module's whole argument for keeping
/// [`refuse_policies_that_admit_nothing`] separate from
/// [`refuse_promises_of_a_local_fallback`] is that an operator should never
/// have to work out which sentence to go and edit.
///
/// **Opens with "its", like every other constant in this list**, because
/// [`refuse_promises_of_a_local_fallback`] joins them with `"; and "` — a
/// sentence starting with its own conjunction renders as "; and and". It also
/// has to resolve what the sentence before it looks like a contradiction of:
/// the key has no local capacity *it can select*, while the deployment does
/// quote one.
const TIERS_NAME_NO_LOCAL_FALLBACK: &str = "its `tiers` recipe is what took it away -- this \
     deployment does quote a local target and neither tier of the recipe names it, so on exactly \
     the turns the spent allowance produces, the recipe describes nothing that can run";

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
    let mut spent_allowances = Vec::new();
    if admission.policy.frontier_cadence.is_some() {
        spent_allowances.extend(spent_allowance_promise(
            CADENCE_PROMISE,
            admission,
            reachable,
            |candidate| admission.policy.admits_when_spent(candidate),
        ));
    }
    if let Some(terms) = &admission.budget {
        // Only one exhaustion setting promises local service at all: `Refuse`
        // never made the promise, and the valve keeps it on frontier. See
        // `Exhaustion::promises_local_service`.
        if terms.budget.on_exhaustion.promises_local_service() {
            let spent = TurnBudget::exhausted(terms.budget.on_exhaustion);
            spent_allowances.extend(spent_allowance_promise(
                BUDGET_PROMISE,
                admission,
                reachable,
                |candidate| admission.policy.permits(candidate) && spent.admits(candidate),
            ));
        }
    }
    // The recipe note is one fact about one line of one file, so a key that
    // breaks both allowances on it reads it once. Deduplicating here rather
    // than threading a "said already" flag through the helper keeps that
    // helper answering one key's one allowance.
    for promise in spent_allowances {
        if !broken.contains(&promise) {
            broken.push(promise);
        }
    }
    broken
}

/// Whether this key's `tiers` recipe could select `candidate` at all.
///
/// **A recipe narrows what a turn may be routed to, so a promise about what
/// happens next has to be asked of the intersection.** `admits_when_spent` and
/// the exhausted-budget predicate answer for the *policy*; neither has any
/// notion of `admission.tiers`, so before this existed a local worker the
/// policy permitted satisfied both while a hosted-only recipe made it
/// unselectable at routing time (M10 review G02).
///
/// `None` — no recipe — is "every admissible candidate is selectable", which is
/// what leaves a deployment that configured no tiers unaffected by this whole
/// question.
fn a_recipe_could_select(admission: &Admission, candidate: &Candidate) -> bool {
    match &admission.tiers {
        None => true,
        Some(recipe) => recipe
            .names()
            .any(|named| named == candidate.target.policy_identity()),
    }
}

/// One spent-allowance promise, asked of what the allowance leaves *and* of
/// what the recipe could then select.
///
/// Both answers are needed because they carry different remedies: nothing
/// survives at all is a fleet this deployment does not have, while something
/// survives that the recipe does not name is a line in the control plane. The
/// promise is broken either way — this returns which sentences say so.
fn spent_allowance_promise(
    promise: &'static str,
    admission: &Admission,
    reachable: &[Candidate],
    survives_the_spent_allowance: impl Fn(&Candidate) -> bool,
) -> Vec<&'static str> {
    let survivors: Vec<&Candidate> = reachable
        .iter()
        .filter(|candidate| survives_the_spent_allowance(candidate))
        .collect();
    if survivors
        .iter()
        .any(|candidate| a_recipe_could_select(admission, candidate))
    {
        return Vec::new();
    }
    match survivors.is_empty() {
        true => vec![promise],
        false => vec![promise, TIERS_NAME_NO_LOCAL_FALLBACK],
    }
}

/// Refuse a `tiers` recipe naming a hosted model this deployment cannot route
/// to.
///
/// **The same question [`refuse_policies_that_admit_nothing`] asks of an
/// `allow` filter, asked of the other list an operator writes target
/// identities into.** `TierRecipe::new` validates the threshold, emptiness and
/// repeats — everything a recipe can be judged on without a catalog — and the
/// config loader that calls it has never seen one either, so a transposed
/// digit in a model id sails through boot and through every admin-plane write.
/// Its runtime symptom is not a failure but a *silent* one: the tier scores,
/// finds nothing, and the turn is served by the other tier at another price
/// (M10 review G09).
///
/// **Hosted names only, and the asymmetry is the fleet's.** `reachable` is
/// quoted from the catalog by `main.rs::reachable_candidates`; a local worker
/// joins through a different seam (`Engine::with_fleet`) and is not visible
/// here at all, so asking this of `local/...` would refuse every recipe that
/// names the fleet — including the shipped example's. What a `local/` entry
/// promises is checked where it can be: [`unkeepable_promises`], against the
/// allowance that would send a turn there.
///
/// Per key rather than per project for the reason
/// [`refuse_policies_that_admit_nothing`] gives: a turn arrives on a key, and
/// `configured_admissions` is the accessor that enumerates them.
pub fn refuse_tier_recipes_naming_absent_targets(
    plane: &ControlPlane,
    reachable: &[Candidate],
) -> anyhow::Result<()> {
    let mut refused: Vec<String> = plane
        .configured_admissions()
        .filter_map(|admission| {
            let recipe = admission.tiers.as_ref()?;
            let absent: Vec<String> = [Tier::Capable, Tier::Efficient]
                .into_iter()
                .flat_map(|tier| recipe.list(tier).iter().map(move |named| (tier, named)))
                .filter(|(_, named)| !named.starts_with("local/"))
                .filter(|(_, named)| {
                    !reachable
                        .iter()
                        .any(|candidate| &candidate.target.policy_identity() == *named)
                })
                // The *file's* word for the tier, not `Tier::label`'s. A label
                // is the audit vocabulary ("strong"/"weak"), stable across
                // deployments precisely because it does not name a config key;
                // an operator sent to grep their control plane for "strong"
                // finds nothing.
                .map(|(tier, named)| {
                    let field = match tier {
                        Tier::Capable => "capable",
                        Tier::Efficient => "efficient",
                    };
                    format!("`{named}` in {field}")
                })
                .collect();
            (!absent.is_empty())
                .then(|| format!("{} — tiers name {}", describe(admission), absent.join(", ")))
        })
        .collect();
    refused.sort();
    if !refused.is_empty() {
        anyhow::bail!(
            "these control-plane keys carry tier recipes naming hosted models this deployment \
             cannot route to, so the tier would score, find nothing, and hand the turn to the \
             other one at another price: {}",
            refused.join(" | ")
        );
    }
    Ok(())
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

    /// The fleet half of a stored document's fingerprint (M16.1, R-D9): every
    /// routing candidate this deployment's cross-checks were built from, as
    /// the identity a policy names it by, sorted and deduplicated.
    ///
    /// **The identities and not the quotes.** A [`Candidate`] carries an
    /// expected cost, an expected TTFT and a cache-adjusted prefill, all of
    /// which move between two calls to
    /// [`reachable_candidates`](https://docs.rs/roundhouse-server) on one
    /// unchanged deployment — a fingerprint over those would report every node
    /// as divergent from every other, including from itself a second later,
    /// which is a divergence check that has told an operator nothing. What the
    /// cross-checks actually read off a candidate is *which target it is*, and
    /// that is what a document is fingerprinted on.
    ///
    /// [`Target::policy_identity`] rather than [`Target::ledger_key`], because
    /// `ledger_key` carries a local worker's id: two nodes quoting the same
    /// model on two different workers would read as divergent fleets when they
    /// route to the same thing. `policy_identity` is the name a policy's
    /// `allow` list writes, which is the granularity a plane is compiled at.
    ///
    /// Sorted and deduplicated so the fingerprint is a property of the set,
    /// not of the order the catalog happened to quote it in: two nodes with
    /// identical fleets must produce identical vectors or the check fires on
    /// every document.
    ///
    /// The judge is deliberately not in here. It is a *catalog* identity —
    /// `ROUNDHOUSE_JUDGE_MODEL` resolved against the catalog — and R-D9's four
    /// axes name the file, the catalog, the fleet and the TTL; folding a fifth
    /// input into the fleet's list would report `Fleet` for a divergence that
    /// is not about the fleet at all.
    pub fn fingerprint(&self) -> Vec<String> {
        let mut identities: Vec<String> = self
            .reachable
            .iter()
            .map(|candidate| candidate.target.policy_identity())
            .collect();
        identities.sort();
        identities.dedup();
        identities
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
        // Last, and not because it matters least: the three above are the
        // refusals every deployment since M6 has met in this order, and a
        // configuration broken in two ways must keep reporting the sentence it
        // always reported first. A check that reordered them would change which
        // `422` an unchanged admin request answers with.
        refuse_tier_recipes_naming_absent_targets(plane, &self.reachable).map_err(|error| {
            CrossCheckRefusal {
                check: "refuse_tier_recipes_naming_absent_targets",
                detail: error.to_string(),
            }
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use roundhouse_core::control::{FrontierCadence, TurnPolicy};
    use roundhouse_core::routing::{PickerMode, Target, TierRecipe};

    use super::*;

    fn candidate(target: Target, quality_prior: f64) -> Candidate {
        Candidate {
            target,
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: 0.0,
            quality_prior,
            load: None,
        }
    }

    fn frontier(provider: &str, model: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn local(model: &str) -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: model.into(),
        }
    }

    /// M10 review G02: a hosted-only tier recipe defeats the cadence's
    /// degrade-to-local promise, and this boot check — the one written to
    /// catch exactly a promise a deployment cannot keep — is blind to it.
    ///
    /// `unkeepable_promises` asks `admission.policy.admits_when_spent`, which
    /// is a pure `TurnPolicy` question (allow filter, quality floor, is the
    /// candidate local) with no notion of `admission.tiers` at all. A local
    /// worker the policy permits satisfies it regardless of whether any tier
    /// in the recipe names that worker, so a recipe that names only hosted
    /// targets reads as a kept promise here even though `StagePolicy::choose`
    /// (`routing/stage.rs`) will find `entitled == [local]`,
    /// `tier_pool` empty for both tiers, and refuse the turn as
    /// `NoViableCandidate` the moment the cadence is spent.
    ///
    /// CONTROL:
    /// `the_boot_check_does_catch_a_cadence_promise_with_no_local_capacity_at_all`
    /// below is the same check with no `tiers` axis in play at all, and it
    /// does report `CADENCE_PROMISE` — proving the check's ordinary mechanism
    /// works and it is specifically the tiers axis this check never reads.
    #[test]
    fn a_hosted_only_tier_recipe_breaks_the_cadence_promise_the_boot_check_misses() {
        let policy = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 2,
                per_turns: 10,
            }),
            ..TurnPolicy::unrestricted()
        };
        // Both tiers name only a hosted target -- the recipe the finding
        // describes, where degrading to local has nothing to route to.
        let recipe = TierRecipe::new(
            vec!["openrouter/capable-m".to_string()],
            vec!["openrouter/efficient-m".to_string()],
            PickerMode::EfficientFirst,
            roundhouse_core::routing::stage::DEFAULT_CONFIDENCE_THRESHOLD,
        )
        .expect("a two-tier recipe at the shipped threshold");

        let admission = Admission {
            policy: Arc::new(policy),
            tiers: Some(Arc::new(recipe)),
            ..Admission::open()
        };

        // The deployment is live: both the hosted target and a local worker
        // are reachable, which is exactly what makes the cadence's promise
        // ("a spent window serves locally") one this fleet *could* keep --
        // if the recipe named the local worker.
        let reachable = vec![
            candidate(frontier("openrouter", "capable-m"), 0.9),
            candidate(frontier("openrouter", "efficient-m"), 0.7),
            candidate(local("llama"), 0.6),
        ];

        let broken = unkeepable_promises(&admission, &reachable, None);
        assert!(
            broken.contains(&CADENCE_PROMISE),
            "the tier recipe names no local target, so a spent cadence has \
             nothing to degrade to at runtime -- but `unkeepable_promises` \
             never reads `admission.tiers` and reports this configuration \
             clean: {broken:?}"
        );
    }

    /// CONTROL for the ignored test above: the same cadence, no `tiers` at
    /// all, and no local candidate anywhere in `reachable` -- the shape this
    /// check was written for. It still reports `CADENCE_PROMISE`, which is
    /// what proves the check's ordinary mechanism works and isolates the
    /// finding to the one axis (`admission.tiers`) it never reads.
    #[test]
    fn the_boot_check_does_catch_a_cadence_promise_with_no_local_capacity_at_all() {
        let policy = TurnPolicy {
            frontier_cadence: Some(FrontierCadence {
                max_frontier: 2,
                per_turns: 10,
            }),
            ..TurnPolicy::unrestricted()
        };
        let admission = Admission {
            policy: Arc::new(policy),
            tiers: None,
            ..Admission::open()
        };
        let reachable = vec![candidate(frontier("openrouter", "capable-m"), 0.9)];

        let broken = unkeepable_promises(&admission, &reachable, None);
        assert!(
            broken.contains(&CADENCE_PROMISE),
            "no local candidate is reachable at all, so the cadence's promise \
             cannot be kept and the check exists precisely to catch this: \
             {broken:?}"
        );
    }

    /// A one-key plane whose project carries `tiers`.
    ///
    /// The same shape `main.rs`'s own cross-check fixtures use, kept here
    /// rather than shared because these tests are about what a *recipe* says
    /// and those are about what a policy does.
    /// The same plane with a cadence on the policy and a hosted-only recipe:
    /// the configuration G02 is about, built through the config file so the
    /// fixture meets the same narrowing and validation a deployment does.
    fn plane_with_tiers_and_cadence() -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{
                "id": "acme",
                "policy": { "frontier_cadence": { "max_frontier": 2, "per_turns": 10 } },
                "tiers": {
                    "capable": ["openrouter/capable-m"],
                    "efficient": ["openrouter/efficient-m"],
                },
            }],
            "users": [{ "id": "ada" }],
            "keys": [{ "project": "acme", "user": "ada", "key_sha256": "a".repeat(64) }],
        })
        .to_string();
        ControlPlane::configured(
            super::super::ControlPlaneConfig::from_json(&json, "tier cross-check fixture")
                .expect("the fixture config must validate"),
        )
    }

    fn plane_with_tiers(tiers: serde_json::Value) -> ControlPlane {
        let json = serde_json::json!({
            "projects": [{ "id": "acme", "tiers": tiers }],
            "users": [{ "id": "ada" }],
            "keys": [{ "project": "acme", "user": "ada", "key_sha256": "a".repeat(64) }],
        })
        .to_string();
        ControlPlane::configured(
            super::super::ControlPlaneConfig::from_json(&json, "tier cross-check fixture")
                .expect("the fixture config must validate"),
        )
    }

    /// **M10 review G09.** A transposed digit in a tier entry has no runtime
    /// symptom an operator can see: the tier scores, `tier_pool` finds
    /// nothing carrying that identity, and the turn is quietly served by the
    /// other tier at another price. This is the one place both files are
    /// loaded, so it is the one place the typo is visible.
    #[test]
    fn a_tier_recipe_naming_a_model_this_deployment_cannot_route_to_is_refused() {
        let plane = plane_with_tiers(serde_json::json!({
            "capable": ["openrouter/capable-m"],
            "efficient": ["openrouter/efficient-typo"],
        }));
        let reachable = vec![
            candidate(frontier("openrouter", "capable-m"), 0.9),
            candidate(frontier("openrouter", "efficient-m"), 0.7),
        ];

        let error = refuse_tier_recipes_naming_absent_targets(&plane, &reachable)
            .expect_err("a recipe entry no catalog holds must not boot");
        let refusal = error.to_string();
        assert!(
            refusal.contains("openrouter/efficient-typo"),
            "the refusal names the entry to edit, not just the key: {refusal}"
        );
        assert!(
            refusal.contains("acme"),
            "and the key whose recipe holds it: {refusal}"
        );
    }

    /// CONTROL: the same recipe spelled correctly. A check that refused this
    /// would refuse every deployment that configured tiers at all, which is
    /// what makes the assertion above about the *typo*.
    #[test]
    fn a_tier_recipe_naming_only_reachable_models_boots() {
        let plane = plane_with_tiers(serde_json::json!({
            "capable": ["openrouter/capable-m"],
            "efficient": ["openrouter/efficient-m"],
        }));
        let reachable = vec![
            candidate(frontier("openrouter", "capable-m"), 0.9),
            candidate(frontier("openrouter", "efficient-m"), 0.7),
        ];
        refuse_tier_recipes_naming_absent_targets(&plane, &reachable)
            .expect("every entry is in the catalog");
    }

    /// The rendered refusal, which is the only artefact an operator ever sees.
    ///
    /// The tests above assert on the `Vec` [`unkeepable_promises`] returns, and
    /// a vector is not a sentence: `refuse_promises_of_a_local_fallback` joins
    /// its entries with `"; and "`, so a constant that opened with its own
    /// conjunction rendered "; and and" — invisible to every assertion in this
    /// module and visible in every boot log. Nothing else exercises a
    /// two-element join, because `main.rs`'s live fixtures carry no `tiers`.
    #[test]
    fn the_recipe_case_renders_both_sentences_and_reads_as_one() {
        let plane = plane_with_tiers_and_cadence();
        let reachable = vec![
            candidate(frontier("openrouter", "capable-m"), 0.9),
            candidate(local("llama"), 0.6),
        ];

        let error = refuse_promises_of_a_local_fallback(&plane, &reachable, None)
            .expect_err("a hosted-only recipe cannot keep this cadence's promise");
        let message = error.to_string();
        assert!(
            message.contains("spent window serves locally"),
            "the promise: {message}"
        );
        assert!(
            message.contains("`tiers` recipe is what took it away"),
            "and the cause, which is the half that says which file to edit: {message}"
        );
        assert!(
            !message.contains("and and"),
            "the joiner is `; and `, so a sentence may not open with one: {message}"
        );
    }

    /// The same misconfiguration through the door every write also uses.
    ///
    /// [`CrossChecks::refuse`] is what makes a check a *boot* refusal and an
    /// admin-plane `422` rather than a function nobody calls — and a check that
    /// is only tested directly stays green when its call site is deleted.
    /// Asserting the reported name also pins the append-last order this check
    /// was deliberately given.
    #[test]
    fn the_absent_target_check_is_wired_into_the_one_list() {
        let plane = plane_with_tiers(serde_json::json!({
            "capable": ["openrouter/capable-m"],
            "efficient": ["openrouter/efficient-typo"],
        }));
        let checks = CrossChecks::new(
            vec![candidate(frontier("openrouter", "capable-m"), 0.9)],
            None,
        );

        let refusal = checks
            .refuse(&plane)
            .expect_err("a boot and every admin write must ask this");
        assert_eq!(refusal.check, "refuse_tier_recipes_naming_absent_targets");
        assert!(
            refusal
                .detail
                .contains("`openrouter/efficient-typo` in efficient"),
            "and the detail names the entry and the tier field holding it: {}",
            refusal.detail
        );
    }

    /// CONTROL, and the asymmetry this check has to live with: `reachable` is
    /// quoted from the *catalog*, and a local worker joins through a different
    /// seam entirely. A `local/` entry is therefore absent from this list on
    /// every deployment, including the ones that have a fleet — refusing over
    /// it would refuse the shipped example, whose efficient tier is exactly
    /// this shape.
    #[test]
    fn a_local_tier_entry_is_not_a_catalog_question_and_is_not_refused() {
        let plane = plane_with_tiers(serde_json::json!({
            "capable": ["openrouter/capable-m"],
            "efficient": ["local/small"],
        }));
        let reachable = vec![candidate(frontier("openrouter", "capable-m"), 0.9)];
        refuse_tier_recipes_naming_absent_targets(&plane, &reachable)
            .expect("the fleet is not the catalog's to answer for");
    }

    /// **The fleet fingerprint is the set of identities, not the quotes**
    /// (M16.1, R-D9).
    ///
    /// Three claims in one test because they are one property, and each of
    /// them is a way the divergence check goes silently useless:
    ///
    /// - **sorted**, so two nodes whose catalogs quote the same models in a
    ///   different order agree. Unsorted, every node diverges from every other
    ///   and the warning is turned off within a day.
    /// - **identities only**, so a candidate's expected cost or TTFT — which
    ///   move between two quotes of one unchanged deployment — cannot get
    ///   into it. That is the version of this that diverges from *itself*.
    /// - **`policy_identity`, not `ledger_key`**, so two nodes whose fleets
    ///   scheduled the same model onto different workers agree about the model
    ///   they can both route to.
    #[test]
    fn the_fleet_fingerprint_is_the_sorted_identity_set_and_nothing_priced() {
        let checks = CrossChecks::new(
            vec![
                candidate(frontier("openrouter", "capable-m"), 0.9),
                candidate(local("small"), 0.4),
                candidate(frontier("anthropic", "big"), 0.95),
            ],
            None,
        );
        assert_eq!(
            checks.fingerprint(),
            vec![
                "anthropic/big".to_string(),
                "local/small".to_string(),
                "openrouter/capable-m".to_string(),
            ]
        );

        // The same fleet, quoted in another order and at other prices: one
        // fingerprint, because nothing that moves got into it.
        let requoted = CrossChecks::new(
            vec![
                Candidate {
                    expected_cost_usd: 42.0,
                    expected_ttft_ms: 999.0,
                    matched_prefix_tokens: 7,
                    ..candidate(frontier("anthropic", "big"), 0.95)
                },
                candidate(frontier("openrouter", "capable-m"), 0.9),
                candidate(local("small"), 0.4),
            ],
            None,
        );
        assert_eq!(checks.fingerprint(), requoted.fingerprint());

        // A worker id is not part of the identity: the same model on a second
        // worker is the same routing target as far as a compiled plane is
        // concerned, and a fingerprint that disagreed would report a fleet
        // rebalance as a config divergence.
        let rescheduled = CrossChecks::new(
            vec![candidate(
                Target::Local {
                    worker_id: 77,
                    dp_rank: 3,
                    model: "small".into(),
                },
                0.4,
            )],
            None,
        );
        assert_eq!(rescheduled.fingerprint(), vec!["local/small".to_string()]);

        // And a fleet that differs really does fingerprint differently, or
        // none of the above would be worth asserting.
        assert_ne!(checks.fingerprint(), rescheduled.fingerprint());
    }
}
