// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which candidates a principal can actually reach, and who pays for them.
//!
//! **The placement is the design.** Credential availability filters the
//! candidate set *before* `choose()` — before the slice ever becomes a
//! [`RoutingContext`](crate::routing::RoutingContext) — and not in the connect
//! branch where a first draft put it. Two things break at the later placement,
//! and they break in opposite directions:
//!
//! - [`Payer`] has to be stampable on the
//!   [`DecisionRecord`](crate::routing::DecisionRecord), which is written
//!   before the dispatch is attempted. A credential resolved at connect time is
//!   resolved after the record it belongs on.
//! - A saving must never be priced against a model the caller could not reach.
//!   `best_frontier_alternative` reads `decision.considered`
//!   (`metrics/fold.rs:264-274`), so a provider left in the candidate list
//!   because nobody had a key for it becomes the counterfactual every local
//!   turn is credited against — a dashboard number invented out of a missing
//!   credential.
//!
//! That second point is also why this filter is *not* the policy filter, which
//! deliberately leaves cadence- and budget-excluded candidates in `considered`:
//! a rationed model is reachable next turn, so its counterfactual is true. A
//! provider this principal holds no credential for is not reachable at all, and
//! pricing against it is a claim about money that could never have been spent.
//!
//! **A missing credential degrades, it does not fail.** Local candidates need
//! no credential, so a principal with none simply loses the hosted half of its
//! pool and serves from the fleet — the same shape as budget exhaustion, a
//! served turn plus a marker rather than a 500. The marker is
//! [`Reachable::withheld_providers`], and it exists because the alternative is
//! the silent failure this milestone's auth ruling found in codex: a deployment
//! whose credential variable was never set looks exactly like one that simply
//! prefers its own workers.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use super::forwarded::PresentedCredential;
use super::secret::{Secret, TurnCredential};
use super::{CredentialError, CredentialMode};
use crate::control::payer::Payer;
use crate::routing::{Candidate, Target};

/// Provider name to key, as a tier holds them.
///
/// `BTreeMap` rather than `HashMap` because [`Reachable::withheld_providers`]
/// is compared and logged: a set whose iteration order changed per process
/// would make one deployment's marker unstable across restarts.
pub type ProviderKeys = BTreeMap<String, Secret>;

/// How one provider is reached this turn, and whose money it is.
#[derive(Debug, Clone)]
pub struct ProviderAccess {
    pub credential: TurnCredential,
    pub payer: Payer,
}

/// The candidate set a principal can reach, and what was taken out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reachable {
    /// What survives. Every local candidate, plus the hosted ones whose
    /// provider resolved a credential.
    pub candidates: Vec<Candidate>,
    /// Providers that were quoted and dropped for want of a credential, sorted
    /// and deduplicated.
    ///
    /// **The marker.** Empty on every ordinary turn, which is what lets it be
    /// skipped on the wire and keeps a pre-M7 log's decisions byte-identical.
    pub withheld_providers: Vec<String>,
}

/// The credentials one admission may draw on, and the rule for choosing among
/// them.
///
/// Three shapes, and the enum is what makes Switchyard's configuration rule a
/// type rule: **forwarding has no field a stored key could sit in.** Switchyard
/// refuses `forward_auth` beside `api_key_env` at parse time
/// (`crates/switchyard-server/src/config.rs:873-877` @ `5341f71`); here the
/// pair cannot be built. That matters more than a validator would, because the
/// failure it prevents is silent on both sides of the wire: codex resolves
/// `env_key` before any first-party auth, so a route configured with both
/// forwards nothing and reports nothing.
#[derive(Debug, Clone)]
pub struct TurnCredentials {
    /// Shared rather than owned, and it buys two things at once. An
    /// [`Admission`](../../../roundhouse_server/control_config/struct.Admission.html)
    /// is cloned out of the control plane's table on **every request**, so an
    /// owned `Resolution` would copy all three tiers -- every provider key this
    /// deployment holds -- per turn. That is both the hot-path cost and, more
    /// to the point, a fresh copy of every plaintext secret on the heap per
    /// request; fewer live copies is the cheap half of not leaking one, which
    /// is the same argument [`Self::reaches`] is written out for.
    ///
    /// Sound because a resolution is immutable: [`Self::with_forwarded`] does
    /// not mutate one, it selects a different one.
    inner: Arc<Resolution>,
}

/// The one [`Resolution::Unrestricted`], shared.
///
/// `Admission::open` is called on every request of an unconfigured deployment,
/// and an `Arc::new` per call would be an allocation per turn to say "no gate".
static UNRESTRICTED: LazyLock<Arc<Resolution>> =
    LazyLock::new(|| Arc::new(Resolution::Unrestricted));

#[derive(Debug, Clone)]
enum Resolution {
    /// No credential gate at all.
    Unrestricted,
    Stored {
        mode: CredentialMode,
        deployment: ProviderKeys,
        project: ProviderKeys,
        user: ProviderKeys,
    },
    Forwarding {
        /// What the request edge captured, or `None` when this request carried
        /// no forwardable credential. `Option` rather than a `bool` beside a
        /// map: "we know a credential arrived" and "here it is" are the same
        /// fact, and two fields could disagree about it.
        presented: Option<PresentedCredential>,
    },
}

impl TurnCredentials {
    /// The value a deployment that does not gate on credentials resolves to.
    ///
    /// Routing under this is byte-identical to routing before M7 existed: every
    /// quoted provider stays in the candidate set and the transport
    /// authenticates itself, which is what an offline stub and every pre-M7
    /// deployment do. A named value rather than a `Default`, the same argument
    /// [`TurnPolicy::unrestricted`](crate::control::TurnPolicy::unrestricted)
    /// and [`Principal::default_open`](crate::control::Principal::default_open)
    /// are written out for: the most permissive value in a security-shaped
    /// module should be a sentence a reader can find, not something a
    /// `..Default::default()` can produce by accident.
    ///
    /// It is permissive about *reachability* and not about secrets: the
    /// credential it hands out is [`TurnCredential::Absent`], so a real
    /// provider client still refuses rather than sending an unauthenticated
    /// request.
    pub fn unrestricted() -> Self {
        Self {
            inner: Arc::clone(&UNRESTRICTED),
        }
    }

    /// What a project's configuration resolves to: a mode and its three tiers.
    ///
    /// **One entry point for every mode, including pass-through, and that is
    /// what makes the mutual-exclusion check unskippable.** A constructor that
    /// refused `PassThrough` and sent the caller to [`Self::forwarding`] would
    /// read more tidily and would put the branch in the *caller* — where the
    /// pass-through arm never looks at the maps, so a project configured with
    /// both a forwarded route and a stored key would sail past the check that
    /// exists for exactly that pair. Taking the maps here even for the mode
    /// that cannot use them is what forces them to be looked at.
    ///
    /// Whether a given request actually carried a forwarded credential is a
    /// different clock — configuration cannot know it — and arrives through
    /// [`Self::with_forwarded`].
    ///
    /// **The exclusion covers the project's and the member's keys and not the
    /// deployment's**, and the line is where it is because of what the rule is
    /// *for*. Switchyard refuses `forward_auth` beside `api_key_env` on one
    /// client because the two are two answers to "how does *this route*
    /// authenticate", and whichever resolves first wins silently
    /// (`crates/switchyard-server/src/config.rs:873-877` @ `5341f71`). A key
    /// written on the project or on the member is exactly that: a second answer
    /// for this route, and it is refused. The deployment's tier is a different
    /// statement — it is this process's inventory, held for the *other*
    /// projects it serves — and refusing it would make pass-through
    /// unavailable to any deployment that also serves a BYOK project, which is
    /// the mixed enterprise this mode exists for.
    ///
    /// Nothing is lost by allowing it: `PassThrough` resolves to
    /// [`Resolution::Forwarding`], which reads no tier at all, so a deployment
    /// key is unreachable from a pass-through project either way. What an
    /// operator who expected it as a fallback gets instead is a served turn
    /// that degraded to local with [`Reachable::withheld_providers`] naming the
    /// provider — and, on a deployment with no local capacity, a boot refusal,
    /// because that arrangement keeps no promise.
    pub fn configured(
        mode: CredentialMode,
        deployment: ProviderKeys,
        project: ProviderKeys,
        user: ProviderKeys,
    ) -> Result<Self, CredentialError> {
        if mode == CredentialMode::PassThrough
            && let Some(provider) = project.keys().chain(user.keys()).next()
        {
            return Err(CredentialError::PassThroughWithStoredCredential {
                provider: provider.clone(),
            });
        }
        Ok(Self {
            inner: Arc::new(match mode {
                CredentialMode::PassThrough => Resolution::Forwarding { presented: None },
                _ => Resolution::Stored {
                    mode,
                    deployment,
                    project,
                    user,
                },
            }),
        })
    }

    /// Pass-through: the client's own credential travels with the request.
    ///
    /// `presented` is what the request edge captured. `None` makes every hosted
    /// provider unreachable and the turn degrades to local, which is
    /// deliberately *not* what codex does on the same mistake — with
    /// `requires_openai_auth` unset it sends an anonymous request and reports
    /// nothing. A degrade plus a marker is the loudest honest answer available
    /// on a turn a client is already waiting for.
    pub fn forwarding(presented: Option<PresentedCredential>) -> Self {
        Self {
            inner: Arc::new(Resolution::Forwarding { presented }),
        }
    }

    /// The same resolution, handed what *this request* carried.
    ///
    /// A no-op except under pass-through, and the no-op is the point: the two
    /// facts answer to different clocks. A mode and its tiers are configuration
    /// — resolved once at admission, immutable for the turn, the same reasoning
    /// [`TurnPolicy`](crate::control::TurnPolicy) is resolved under. What
    /// headers arrived is a property of one request, and a configuration that
    /// claimed to know it would be answering a question it cannot see.
    ///
    /// A stored resolution therefore *drops* the capture on the floor, which is
    /// the second half of the no-op and the more important half: a project that
    /// pays with its own key never forwards a client's credential anywhere,
    /// however the client chose to authenticate to roundhouse.
    pub fn with_forwarded(self, presented: Option<PresentedCredential>) -> Self {
        match matches!(*self.inner, Resolution::Forwarding { .. }) {
            true => Self::forwarding(presented),
            false => self,
        }
    }

    /// Whether this admission forwards rather than authenticates.
    ///
    /// Read at settle time, where the question is not "which key" but "may
    /// roundhouse put a dollar figure on this turn at all" — a forwarded seat is
    /// a subscription with no rate card, and pricing it from the catalog would
    /// invent a bill nobody issued. See
    /// [`SettledSpend`](crate::control::SettledSpend).
    ///
    /// True whether or not a credential was actually presented: a turn under a
    /// pass-through project is a pass-through turn even when it degraded to
    /// local for want of one, and a local turn is free either way.
    pub fn is_forwarding(&self) -> bool {
        matches!(*self.inner, Resolution::Forwarding { .. })
    }

    /// Whether `provider` is reachable at all, without copying a key to find
    /// out.
    ///
    /// [`Self::access`] clones the secret it finds, which is right for a caller
    /// that is about to authenticate with it and wrong for one that only wants
    /// to filter a candidate list — that would put a copy of every configured
    /// key on the heap on every turn, for the sake of a `bool`. Fewer live
    /// copies of plaintext is the cheap half of not leaking one.
    pub fn reaches(&self, provider: &str) -> bool {
        match &*self.inner {
            Resolution::Unrestricted => true,
            Resolution::Forwarding { presented } => presented
                .as_ref()
                .is_some_and(|presented| presented.covers(provider)),
            Resolution::Stored { .. } => self.tiers().iter().any(|(keys, _)| {
                keys.as_ref()
                    .is_some_and(|keys| keys.contains_key(provider))
            }),
        }
    }

    /// The tiers this mode reads, in the order it reads them.
    ///
    /// Fixed-width and `Option`-padded rather than a slice of temporaries,
    /// because a borrow of the match arm's own array does not outlive the arm
    /// and two callers need this list. `UserOnly` names one tier and no
    /// fallback — see [`CredentialMode`].
    fn tiers(&self) -> [(Option<&ProviderKeys>, Payer); 3] {
        // The padding for a mode that reads fewer than three tiers, and for the
        // two resolutions that read none. `Payer::Deployment` beside a `None`
        // is never observed — both callers skip an absent tier before they
        // reach the payer — so it is filler rather than a claim.
        const NONE: (Option<&ProviderKeys>, Payer) = (None, Payer::Deployment);
        let Resolution::Stored {
            mode,
            deployment,
            project,
            user,
        } = &*self.inner
        else {
            return [NONE; 3];
        };
        match mode {
            CredentialMode::ProjectOnly => [
                (Some(project), Payer::Project),
                (Some(deployment), Payer::Deployment),
                NONE,
            ],
            CredentialMode::PreferUser => [
                (Some(user), Payer::User),
                (Some(project), Payer::Project),
                (Some(deployment), Payer::Deployment),
            ],
            CredentialMode::UserOnly => [(Some(user), Payer::User), NONE, NONE],
            // Unreachable: `configured` turns this mode into `Forwarding`, and
            // it is the only way into the `Stored` arm.
            CredentialMode::PassThrough => [NONE; 3],
        }
    }

    /// How `provider` is reached this turn, or `None` when it is not.
    pub fn access(&self, provider: &str) -> Option<ProviderAccess> {
        match &*self.inner {
            Resolution::Unrestricted => Some(ProviderAccess {
                credential: TurnCredential::Absent,
                payer: Payer::Deployment,
            }),
            Resolution::Forwarding { presented } => presented
                .as_ref()
                // The narrowing to this provider's allowlist happens here and
                // only here, because here is the first moment a provider is
                // named. A capture is what the client sent; what one upstream is
                // offered is a different, smaller thing.
                .and_then(|presented| presented.for_provider(provider))
                .map(|credential| ProviderAccess {
                    credential: TurnCredential::Forwarded(credential),
                    // The seat the client logged in with pays for the turn, so
                    // the user is the payer even though roundhouse never held
                    // the key.
                    payer: Payer::User,
                }),
            Resolution::Stored { .. } => self.tiers().iter().find_map(|(keys, payer)| {
                keys.and_then(|keys| keys.get(provider))
                    .map(|secret| ProviderAccess {
                        credential: TurnCredential::Stored(secret.clone()),
                        payer: *payer,
                    })
            }),
        }
    }

    /// The credential and payer for one chosen target.
    ///
    /// `None` for a hosted target this admission cannot reach — which
    /// [`Self::reachable`] has already made unchoosable, so a `None` here is a
    /// caller that skipped the filter rather than a state to fall back from.
    /// Deliberately not an infallible call with a default payer: a default
    /// would book somebody else's spend under the deployment's name.
    ///
    /// A local target always resolves, to no credential and the deployment as
    /// payer — literally true, because local capacity is the deployment's own
    /// and a locally routed turn never touches a credential at all.
    pub fn access_for(&self, target: &Target) -> Option<ProviderAccess> {
        match target {
            Target::Local { .. } => Some(ProviderAccess {
                credential: TurnCredential::Absent,
                payer: Payer::Deployment,
            }),
            Target::Frontier { provider, .. } => self.access(provider),
        }
    }

    /// Take out of `candidates` every hosted option this principal cannot
    /// reach.
    ///
    /// Consumes and returns the vector rather than borrowing, so there is no
    /// moment where an unfiltered set and a filtered one both exist for a
    /// caller to pass the wrong one of.
    pub fn reachable(&self, candidates: Vec<Candidate>) -> Reachable {
        let mut withheld: Vec<String> = Vec::new();
        let kept = candidates
            .into_iter()
            .filter(|candidate| match &candidate.target {
                // Local capacity needs no credential, which is the whole reason
                // a missing one degrades instead of failing.
                Target::Local { .. } => true,
                Target::Frontier { provider, .. } => match self.reaches(provider) {
                    true => true,
                    false => {
                        withheld.push(provider.clone());
                        false
                    }
                },
            })
            .collect();
        withheld.sort();
        withheld.dedup();
        Reachable {
            candidates: kept,
            withheld_providers: withheld,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::LazyLock;

    use crate::control::{FrontierHistory, TurnBudget, TurnPolicy};
    use crate::ids::SessionId;
    use crate::routing::{CacheLedger, RoutingContext};

    static SESSION: LazyLock<SessionId> = LazyLock::new(|| SessionId::new("acme/ada/s1"));
    static LEDGER: LazyLock<CacheLedger> = LazyLock::new(CacheLedger::new);
    static UNRESTRICTED: LazyLock<TurnPolicy> = LazyLock::new(TurnPolicy::unrestricted);
    static NO_HISTORY: LazyLock<FrontierHistory> = LazyLock::new(FrontierHistory::default);

    /// A JWT, because a device login produces one — and therefore exactly the
    /// shape a stored credential is refused for.
    const FORWARDED_BEARER: &str = "Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJYIn0.pass-through-only";

    /// What the request edge captures on a pass-through turn.
    fn presented() -> Option<PresentedCredential> {
        PresentedCredential::captured(|name| match name {
            "authorization" => Some(FORWARDED_BEARER.to_string()),
            _ => None,
        })
    }

    fn keys(entries: &[(&str, &str)]) -> ProviderKeys {
        entries
            .iter()
            .map(|(provider, secret)| {
                (
                    (*provider).to_string(),
                    Secret::api_key(*secret).expect("an ordinary API key"),
                )
            })
            .collect()
    }

    fn candidate(target: Target) -> Candidate {
        Candidate {
            target,
            expected_prefill_tokens: 1_000.0,
            matched_prefix_tokens: 0,
            expected_ttft_ms: 100.0,
            expected_cost_usd: 1.0,
            quality_prior: 0.9,
            load: None,
        }
    }

    fn frontier(provider: &str) -> Target {
        Target::Frontier {
            provider: provider.into(),
            model: "flagship".into(),
        }
    }

    fn worker() -> Target {
        Target::Local {
            worker_id: 7,
            dp_rank: 0,
            model: "llama".into(),
        }
    }

    fn pool() -> Vec<Candidate> {
        vec![
            candidate(worker()),
            candidate(frontier("anthropic")),
            candidate(frontier("openai")),
        ]
    }

    /// The router's own view of a candidate set, under a policy and a budget
    /// that refuse nothing.
    ///
    /// Both are the permissive values on purpose: this module's subject is the
    /// credential axis, and a policy or a budget that also excluded something
    /// would leave the test unable to say which axis emptied the pool.
    fn context<'a>(candidates: &'a [Candidate]) -> RoutingContext<'a> {
        RoutingContext {
            session_id: &SESSION,
            turn_index: 0,
            isl_tokens: 1_000,
            candidates,
            ledger: &LEDGER,
            turn_policy: &UNRESTRICTED,
            frontier_history: &NO_HISTORY,
            budget: &TurnBudget::Unlimited,
            signals: None,
            tiers: None,
        }
    }

    fn providers(reached: &Reachable) -> Vec<String> {
        reached
            .candidates
            .iter()
            .map(|candidate| candidate.target.policy_identity())
            .collect()
    }

    #[test]
    fn a_principal_without_a_credential_for_a_provider_never_sees_it_in_candidates() {
        // PROBE: one key, two hosted providers quoted. The provider with no key
        // must not survive into the set `choose()` is offered -- and therefore
        // not into `DecisionRecord::considered`, which is what
        // `best_frontier_alternative` prices a local turn's saving against.
        let one_key = TurnCredentials::configured(
            CredentialMode::PreferUser,
            keys(&[("anthropic", "sk-ant-api03-AAAA")]),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .unwrap();
        let reached = one_key.reachable(pool());
        assert_eq!(
            providers(&reached),
            vec!["local/llama", "anthropic/flagship"],
            "the provider with no credential is unreachable, so it is not a candidate"
        );
        assert_eq!(
            reached.withheld_providers,
            vec!["openai".to_string()],
            "and the fact it was dropped exists nowhere else in the log"
        );

        // CONTROL: the same pool with both keys present keeps everything, so
        // the assertion above is about the missing credential and not about the
        // filter dropping hosted models generally.
        let both = TurnCredentials::configured(
            CredentialMode::PreferUser,
            keys(&[
                ("anthropic", "sk-ant-api03-AAAA"),
                ("openai", "sk-proj-BBBB"),
            ]),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .unwrap();
        let all = both.reachable(pool());
        assert_eq!(all.candidates.len(), 3);
        assert!(all.withheld_providers.is_empty());

        // CONTROL: an ungated deployment routes exactly as it did before M7.
        let before = TurnCredentials::unrestricted().reachable(pool());
        assert_eq!(before.candidates, pool());
        assert!(before.withheld_providers.is_empty());
    }

    #[test]
    fn a_principal_with_no_credential_at_all_degrades_to_local_rather_than_failing() {
        // `UserOnly` with no user credential: §3's stated case. Every hosted
        // candidate goes, the local one stays, and the turn serves -- the same
        // mechanism as budget exhaustion, which also leaves a non-empty pool
        // because local is priced at zero.
        let user_only = TurnCredentials::configured(
            CredentialMode::UserOnly,
            keys(&[("anthropic", "sk-ant-deployment-key")]),
            keys(&[("openai", "sk-proj-project-key")]),
            ProviderKeys::new(),
        )
        .unwrap();
        let reached = user_only.reachable(pool());
        assert_eq!(
            providers(&reached),
            vec!["local/llama"],
            "UserOnly does not fall back to the project's or the deployment's key"
        );
        assert_eq!(
            reached.withheld_providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );
        // "Degrades" is a claim about the router, so it is asserted against the
        // router rather than against the length of a vector. The filtered set
        // resolves to a served local turn; nothing here produces a
        // `RoutingError`, which is what "never a 500" means.
        let admitted = context(&reached.candidates)
            .admissible(None)
            .expect("a local candidate survives, so the turn serves");
        assert_eq!(admitted.pool().len(), 1);
        assert!(admitted.pool()[0].target.is_local());

        // And the counterfactual that says why the filter belongs *here*,
        // upstream of the router, rather than at the connect branch: handed the
        // unfiltered pool, the very same policy and budget admit two hosted
        // candidates this principal cannot authenticate to. Chosen, either
        // would fail the turn at dispatch -- and would have been priced into
        // `considered` as the saving a local turn is credited against.
        let quoted = pool();
        let unfiltered = context(&quoted)
            .admissible(None)
            .expect("the policy admits everything; that is the point");
        assert_eq!(unfiltered.pool().len(), 3);

        // The same member, once they attach a key: the mode is a resolution
        // order and not a refusal.
        let attached = TurnCredentials::configured(
            CredentialMode::UserOnly,
            ProviderKeys::new(),
            ProviderKeys::new(),
            keys(&[("anthropic", "sk-ant-users-own-key")]),
        )
        .unwrap();
        assert_eq!(
            providers(&attached.reachable(pool())),
            vec!["local/llama", "anthropic/flagship"]
        );
    }

    #[test]
    fn the_resolution_order_decides_the_payer() {
        let all_three = |mode| {
            TurnCredentials::configured(
                mode,
                keys(&[("anthropic", "sk-ant-deployment")]),
                keys(&[("anthropic", "sk-ant-project")]),
                keys(&[("anthropic", "sk-ant-user")]),
            )
            .unwrap()
        };
        let resolved = |mode| {
            let access = all_three(mode)
                .access("anthropic")
                .expect("a key in every tier");
            (
                access.payer,
                access
                    .credential
                    .require_api_key("anthropic")
                    .unwrap()
                    .to_string(),
            )
        };

        assert_eq!(
            resolved(CredentialMode::PreferUser),
            (Payer::User, "sk-ant-user".to_string())
        );
        assert_eq!(
            resolved(CredentialMode::ProjectOnly),
            (Payer::Project, "sk-ant-project".to_string()),
            "ProjectOnly never reaches a member's own key"
        );
        assert_eq!(
            resolved(CredentialMode::UserOnly),
            (Payer::User, "sk-ant-user".to_string())
        );

        // The fallbacks, one tier at a time.
        let project_then_deployment = TurnCredentials::configured(
            CredentialMode::ProjectOnly,
            keys(&[("anthropic", "sk-ant-deployment")]),
            ProviderKeys::new(),
            keys(&[("anthropic", "sk-ant-user")]),
        )
        .unwrap();
        assert_eq!(
            project_then_deployment.access("anthropic").unwrap().payer,
            Payer::Deployment,
            "a deployment-wide key is not a member's key, so ProjectOnly may use it"
        );

        // A local target always resolves, to the deployment and to no
        // credential: local capacity is ours and a local turn never touches a
        // key at all.
        let local = all_three(CredentialMode::UserOnly)
            .access_for(&worker())
            .unwrap();
        assert_eq!(local.payer, Payer::Deployment);
        assert!(matches!(local.credential, TurnCredential::Absent));

        // An unreachable hosted target is `None` rather than a defaulted payer:
        // a default here would book somebody else's spend under the
        // deployment's name.
        let nothing = TurnCredentials::configured(
            CredentialMode::UserOnly,
            ProviderKeys::new(),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .unwrap();
        assert!(nothing.access_for(&frontier("anthropic")).is_none());
    }

    #[test]
    fn pass_through_forwards_the_callers_credential_and_cannot_hold_a_stored_key() {
        let forwarding = TurnCredentials::forwarding(presented());
        let access = forwarding.access("openai").expect("a presented credential");
        assert!(access.credential.is_forwarded());
        assert_eq!(
            access
                .credential
                .forwarded()
                .unwrap()
                .headers()
                .collect::<Vec<_>>(),
            vec![("authorization", FORWARDED_BEARER)],
            "the quote carries the caller's own credential, already narrowed to \
             this provider's allowlist"
        );
        assert_eq!(
            access.payer,
            Payer::User,
            "the seat the client logged in with pays"
        );
        // Nothing to *reveal*: a client asking for a stored key under
        // pass-through has misread its own mode.
        assert!(access.credential.require_api_key("openai").is_err());

        // A provider with no allowlist row is unreachable rather than reachable
        // anonymously -- the fail-closed direction, and the reason the table in
        // `forwarded.rs` carries only rows somebody has tested. The fixture
        // moved with M11.0: `anthropic` used to be the rowless provider and now
        // has one, so the claim is made against a name no row will ever carry.
        assert!(forwarding.access("some-new-vendor").is_none());

        // And the row that landed with M11.0's client is reachable from here,
        // which is the other half of the same claim: the table is what decides,
        // not this module. Without this line the assertion above would still
        // pass on a build whose narrowing had stopped working entirely.
        assert!(
            forwarding
                .access("anthropic")
                .is_some_and(|access| access.credential.is_forwarded())
        );

        // No credential presented: every hosted provider goes and the turn
        // degrades, rather than reaching the upstream anonymously.
        let missing = TurnCredentials::forwarding(None).reachable(pool());
        assert_eq!(providers(&missing), vec!["local/llama"]);
        assert_eq!(
            missing.withheld_providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );

        // The mutual exclusion, refused at construction rather than ordered.
        // Both tiers that name a key *for this route* are covered.
        for (project, user) in [
            (keys(&[("openai", "sk-proj-stored")]), ProviderKeys::new()),
            (ProviderKeys::new(), keys(&[("openai", "sk-proj-members")])),
        ] {
            let both = TurnCredentials::configured(
                CredentialMode::PassThrough,
                ProviderKeys::new(),
                project,
                user,
            );
            assert_eq!(
                both.unwrap_err().code(),
                "pass_through_with_stored_credential"
            );
        }
        // But a deployment-wide key is this process's inventory for its *other*
        // projects, not a second answer for this route -- refusing it would make
        // pass-through unavailable to any deployment that also serves a BYOK
        // project. It is unreachable from here either way: the forwarding
        // resolution reads no tier.
        let beside_a_deployment_key = TurnCredentials::configured(
            CredentialMode::PassThrough,
            keys(&[("openai", "sk-proj-the-deployments-own")]),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .expect("a deployment's own inventory is not a second answer for this route");
        assert!(
            !beside_a_deployment_key.reaches("openai"),
            "and it stays unreachable: no request has presented a credential yet"
        );
        // The control: pass-through with no stored key anywhere is the ordinary
        // configuration and builds -- and it resolves to the forwarding arm,
        // which is what makes the refusal above about the *pair* rather than
        // about the mode.
        let configured = TurnCredentials::configured(
            CredentialMode::PassThrough,
            ProviderKeys::new(),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .expect("pass-through with nothing stored is the ordinary configuration");

        // Configuration cannot know whether *this* request carried a
        // credential, so it starts out saying it did not -- the fail-closed
        // direction -- and the per-request fact arrives separately.
        assert!(!configured.reaches("openai"));
        assert!(
            configured
                .clone()
                .with_forwarded(presented())
                .reaches("openai"),
            "the request's own credential is the other clock"
        );
        assert!(
            !configured
                .with_forwarded(presented())
                .with_forwarded(None)
                .reaches("openai")
        );
    }

    #[test]
    fn noting_a_forwarded_credential_does_nothing_to_a_stored_resolution() {
        // The no-op is the point: a stored mode has no forwarded credential to
        // note, so an extractor that calls this on every admission cannot
        // accidentally turn a project's own key into a pass-through route --
        // or, in the other direction, gate a stored key on a header that was
        // never going to arrive.
        let stored = TurnCredentials::configured(
            CredentialMode::PreferUser,
            keys(&[("anthropic", "sk-ant-api03-AAAA")]),
            ProviderKeys::new(),
            ProviderKeys::new(),
        )
        .unwrap();
        for capture in [presented(), None] {
            let noted = stored.clone().with_forwarded(capture);
            assert!(noted.reaches("anthropic"));
            assert!(!noted.reaches("openai"));
            assert!(matches!(
                noted.access("anthropic").unwrap().credential,
                TurnCredential::Stored(_)
            ));
        }

        // And an ungated deployment stays ungated, which is the compatibility
        // promise: turning the extractor on must not re-route a pre-M7
        // workload.
        for capture in [presented(), None] {
            assert!(
                TurnCredentials::unrestricted()
                    .with_forwarded(capture)
                    .reaches("anthropic")
            );
        }
    }

    #[test]
    fn reaches_and_access_agree_on_every_shape() {
        // Two answers to one question -- `reaches` exists only so a candidate
        // filter does not clone a key per turn to learn a `bool` -- so they
        // have to be the same answer. A drift here would filter a provider out
        // of the candidate set and then resolve a credential for it, or the
        // reverse.
        let shapes = [
            TurnCredentials::unrestricted(),
            TurnCredentials::forwarding(presented()),
            TurnCredentials::forwarding(None),
            TurnCredentials::configured(
                CredentialMode::PreferUser,
                keys(&[("anthropic", "sk-ant-deployment")]),
                ProviderKeys::new(),
                keys(&[("openai", "sk-proj-user")]),
            )
            .unwrap(),
            TurnCredentials::configured(
                CredentialMode::UserOnly,
                keys(&[("anthropic", "sk-ant-deployment")]),
                ProviderKeys::new(),
                ProviderKeys::new(),
            )
            .unwrap(),
            TurnCredentials::configured(
                CredentialMode::ProjectOnly,
                ProviderKeys::new(),
                keys(&[("anthropic", "sk-ant-project")]),
                keys(&[("openai", "sk-proj-user")]),
            )
            .unwrap(),
        ];
        for credentials in shapes {
            for provider in ["anthropic", "openai", "never-configured"] {
                assert_eq!(
                    credentials.reaches(provider),
                    credentials.access(provider).is_some(),
                    "{credentials:?} disagrees with itself about `{provider}`"
                );
            }
        }
    }
}
