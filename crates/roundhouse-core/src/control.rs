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
//! give. The choice is between an `Option<KeyId>` that is always `None` on the
//! open path and a type where the question cannot be asked. The second keeps
//! the invalid state unrepresentable, and it is what lets the open-mode default
//! below be an ordinary value rather than a special case threaded through the
//! engine.
//!
//! **[`PrincipalKey`] has an `Unattributed` arm and it is not a `None`.** Logs
//! written before the control plane existed carry no identity, and they are not
//! *missing* one — there was nobody to record. Folding them into a project's
//! row would inflate exactly the number a project is judged and billed on, so
//! they get a row of their own that no project can be confused with, and which
//! a reader can see the size of.

use std::fmt;

use serde::{Deserialize, Serialize};

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
string_id!(
    KeyId,
    "key",
    "Identifies one API key *record* — never the secret itself.\n\nThe secret is held only as a hash (see the control-plane config), so this is\nwhat an audit line, a revocation, or an admin listing refers to."
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
    /// Defined once, here, because two places have to agree on it exactly: the
    /// generator that mints a namespaced session id and the check that refuses
    /// a client-supplied id from outside the caller's namespace. Two spellings
    /// of the same convention is how a namespace stops being one.
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

/// What a presented key is allowed to do.
///
/// An enum rather than a `role` field beside an optional principal, because the
/// two arms carry genuinely different data: an admin key has no membership to
/// spend against, and a turn key has no business mutating tenancy. Matching on
/// this at the extractor is what makes "an admin key served a turn" a shape the
/// code cannot express, rather than a check somebody has to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyScope {
    /// Pays for turns as one membership.
    Turn(Principal),
    /// Reads and writes the control plane itself. Deliberately carries no
    /// principal: an admin acts on the deployment, not from inside a project.
    Admin,
}

impl KeyScope {
    /// The membership this key spends as, if it spends at all.
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            KeyScope::Turn(principal) => Some(principal),
            KeyScope::Admin => None,
        }
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
    Attributed { project: ProjectId, user: UserId },
    /// Usage from a log that names no principal.
    ///
    /// Ordered last so it sorts to the bottom of a report rather than into the
    /// middle of the projects.
    Unattributed,
}

impl PrincipalKey {
    /// The principal this key stands for, or `None` for [`Self::Unattributed`].
    pub fn principal(&self) -> Option<Principal> {
        match self {
            PrincipalKey::Attributed { project, user } => {
                Some(Principal::new(project.clone(), user.clone()))
            }
            PrincipalKey::Unattributed => None,
        }
    }
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
    fn an_admin_key_has_no_membership_to_spend_as() {
        assert_eq!(KeyScope::Admin.principal(), None);
        let principal = Principal::new("acme", "ada");
        assert_eq!(
            KeyScope::Turn(principal.clone()).principal(),
            Some(&principal)
        );
    }

    #[test]
    fn unattributed_sorts_after_every_project() {
        let mut keys = vec![
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
