// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Who a turn belongs to.
//!
//! The vocabulary of the control plane, kept in core rather than in a crate of
//! its own because everything downstream borrows it: the router reads it out of
//! a session, the fold groups by it, the server resolves a credential into it.
//! A separate crate would have to be depended on by the one crate that already
//! sits below all three.
//!
//! Two decisions here are load-bearing enough to state before the types.
//!
//! **A [`Principal`] carries no key.** A resolved key knows which membership it
//! belongs to; the membership has no need to know which secret proved it. Were
//! the credential a field, every construction site would have to produce one —
//! and an unconfigured deployment, which authenticates nothing, has none to
//! give. The choice is between an optional key id that is always absent on the
//! open path and a type where the question cannot be asked. The second keeps
//! the invalid state unrepresentable, and it is what lets the open-mode default
//! below be an ordinary value rather than a special case threaded through the
//! engine.
//!
//! It follows that **no key vocabulary lives here at all.** What a presented
//! key is allowed to do is a property of the thing that resolves it, so the
//! scope enum sits beside `ControlPlane::resolve` in the server crate. A key
//! *record* — the id an audit line or a revocation names — has no producer yet;
//! it arrives with the admin plane, and it will arrive next to the resolver
//! too, not here.
//!
//! **[`PrincipalKey`] has an `Unattributed` arm and it is not a `None`.** Logs
//! written before the control plane existed carry no identity, and they are not
//! *missing* one — there was nobody to record. Folding them into a project's
//! row would inflate exactly the number a project is judged and billed on, so
//! they get a row of their own that no project can be confused with, and which
//! a reader can see the size of.
//!
//! What a resolved caller may *do* with a turn is the sibling module
//! [`policy`], not this one: identity is a fact about a request and a
//! [`TurnPolicy`] is a fact about configuration, and they are resolved
//! together at admission precisely because they are two answers rather than
//! one. What it may *spend* is a third answer on a third clock, and it gets
//! two modules of its own: [`budget`] for the ceilings an operator writes down
//! and [`spend`] for the durable counter they are checked against.
//!
//! What it may spend it *on* is a fourth answer, and it is the one that decides
//! the other three's units: [`credential`] holds the keys a turn authenticates
//! with, which providers are therefore reachable at all, and the handle that
//! renders as a fingerprint everywhere but the one seam that reveals it.
//! [`payer`] holds whose money that was — and the rule that stops roundhouse
//! naming a price it did not pay.

pub mod budget;
pub mod credential;
pub mod fair_use;
pub mod payer;
pub mod policy;
pub mod spend;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use budget::{
    Allocation, Budget, BudgetState, BudgetWindow, DEFAULT_WARN_AT, Exhaustion, TurnBudget,
};
pub use credential::{
    CredentialError, CredentialKind, CredentialMode, CredentialRef, ForwardedCredential,
    OauthEvidence, PresentedCredential, ProviderAccess, Reachable, Secret, TurnCredential,
    TurnCredentials,
};
pub use fair_use::{
    FairUseError, FairUseLedger, FairUseLimit, FairUseQuantity, FairUseRefusal, FairUseScope,
    FairUseTerms, FairUseWindow, MemoryFairUseLedger,
};
pub use payer::{Billing, BudgetCounts, Payer, SettledSpend};
pub use policy::{
    FilterError, FrontierCadence, FrontierHistory, PolicyOverrides, TargetFilter, TurnPolicy,
};
pub use spend::{
    Balance, BalanceQuery, BudgetTerms, Grant, GrantRequest, LedgerState, MemorySpendLedger,
    Settled, Settlement, SpendError, SpendLedger, window_start_ms,
};

use crate::ids::string_id;

string_id!(
    ProjectId,
    "proj",
    "Identifies one project: the unit a budget, a model-access filter, and a\nspend row attach to.\n\nOperator-chosen where a config file names it, which is why the id is also\nhalf of a session namespace — see [`Principal::namespace_prefix`]."
);
string_id!(
    UserId,
    "user",
    "Identifies one human: a stable handle (email or SSO subject), not a display\nname.\n\nA user is only ever encountered through a membership, never alone: the same\nperson on two projects is two [`Principal`]s, because their budget and their\nmodel access differ per project."
);
/// The resolved caller: one membership, and nothing about how it was proved.
///
/// Total by construction — there is no code path below the extractor that can
/// ask "which project is this?", because the answer arrived with the caller.
/// That is the whole reason the key is per `(project, user)` rather than per
/// user with a request-time project selector: a selector makes this an
/// `Option<ProjectId>` everywhere beneath it, and an unauthenticated one lets a
/// client choose whose budget to spend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Principal {
    pub project: ProjectId,
    pub user: UserId,
}

impl Principal {
    pub fn new(project: impl Into<ProjectId>, user: impl Into<UserId>) -> Self {
        Self {
            project: project.into(),
            user: user.into(),
        }
    }

    /// The single membership an unconfigured deployment resolves every request
    /// to.
    ///
    /// A named value rather than a `Default` impl: a principal that appeared
    /// because somebody wrote `..Default::default()` would attribute a turn to
    /// nobody in particular and look identical to one that was authenticated.
    /// Reaching for open mode should be a sentence a reader can find.
    pub fn default_open() -> Self {
        Self::new("default", "default")
    }

    /// The prefix every session id belonging to this principal starts with.
    ///
    /// **The one spelling of the namespace convention.** Everything else that
    /// touches it is built on this: the server's `ControlPlane::qualify` mints
    /// ids with it, `ControlPlane::contains` checks them against it, and the
    /// refusal that names the prefix a caller should have used reads it from
    /// here too. It stays in core rather than moving up beside those, because
    /// the shape `{project}/{user}/` is a fact about a [`Principal`] — the
    /// [`Display`](fmt::Display) impl below is the same string without the
    /// trailing slash — while *whether a deployment namespaces at all* is a
    /// fact about the deployment, and belongs with the control plane. Two
    /// spellings of one convention is how a namespace stops being one.
    ///
    /// Unambiguous because `/` cannot occur inside either id — project and user
    /// ids are validated as slugs where they are configured — so the prefix
    /// splits a namespaced id in exactly one place.
    pub fn namespace_prefix(&self) -> String {
        format!("{}/{}/", self.project, self.user)
    }
}

impl fmt::Display for Principal {
    /// `project/user`, the same shape as the session namespace, so a log line
    /// and a session id read as the same thing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.user)
    }
}

/// The grouping the metrics fold accumulates against.
///
/// Distinct from [`Principal`] because a fold must be total over every log it
/// is handed, including the ones written before any of this existed. See this
/// module's note on why those get their own row instead of `None` or a
/// best-guess project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrincipalKey {
    Attributed {
        project: ProjectId,
        user: UserId,
    },
    /// Usage from a log that names no principal.
    ///
    /// Ordered last so it sorts to the bottom of a report rather than into the
    /// middle of the projects.
    Unattributed,
}

impl From<&Principal> for PrincipalKey {
    fn from(principal: &Principal) -> Self {
        PrincipalKey::Attributed {
            project: principal.project.clone(),
            user: principal.user.clone(),
        }
    }
}

impl From<Principal> for PrincipalKey {
    fn from(principal: Principal) -> Self {
        PrincipalKey::Attributed {
            project: principal.project,
            user: principal.user,
        }
    }
}

impl fmt::Display for PrincipalKey {
    /// `project/user`, or `unattributed`.
    ///
    /// The two cannot be confused: an attributed key always contains a `/` and
    /// neither id may, so no project can spell itself the same way the
    /// unattributed row is spelled.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrincipalKey::Attributed { project, user } => write!(f, "{project}/{user}"),
            PrincipalKey::Unattributed => f.write_str("unattributed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_displays_as_its_session_namespace() {
        let principal = Principal::new("acme", "ada");
        assert_eq!(principal.to_string(), "acme/ada");
        assert_eq!(principal.namespace_prefix(), "acme/ada/");
    }

    #[test]
    fn a_principal_round_trips_through_json() {
        let principal = Principal::new("acme", "ada");
        let encoded = serde_json::to_string(&principal).unwrap();
        assert_eq!(encoded, r#"{"project":"acme","user":"ada"}"#);
        assert_eq!(
            serde_json::from_str::<Principal>(&encoded).unwrap(),
            principal
        );
    }

    #[test]
    fn the_unattributed_row_cannot_be_spelled_by_any_project() {
        // The one property the two Display arms have to keep: a project's row
        // and the marked row are distinguishable as strings, or a report can
        // merge them by accident.
        let attributed = PrincipalKey::from(&Principal::new("unattributed", "unattributed"));
        assert_ne!(
            attributed.to_string(),
            PrincipalKey::Unattributed.to_string()
        );
    }

    #[test]
    fn unattributed_sorts_after_every_project() {
        let mut keys = [
            PrincipalKey::Unattributed,
            PrincipalKey::from(&Principal::new("zeta", "zoe")),
            PrincipalKey::from(&Principal::new("acme", "ada")),
        ];
        keys.sort();
        assert_eq!(
            keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
            vec!["acme/ada", "zeta/zoe", "unattributed"]
        );
    }
}
