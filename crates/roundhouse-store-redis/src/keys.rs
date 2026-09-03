// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The one function every family in this crate builds its shared-store keys
//! from, and the type that carries the deployment namespace those keys start
//! with.
//!
//! # R-S3 — one namespace, one builder, every family audited
//!
//! Before this rung, four families each spelled their own key format —
//! [`crate`]'s session store hard-coded a bare `"rh"` prefix, `spend` and
//! `fair_use` copied it, and `correlation` alone carried a schema version —
//! so a deployment sharing one Redis with another roundhouse deployment had
//! no way to keep the two apart, and a family's key shape was pinned by
//! nothing but its own author remembering the last one's convention. Every
//! family now builds every key through [`build_key`], from a
//! [`KeyNamespace`] the composition root reads once and threads through
//! [`crate::RedisSessionStore::connect_namespaced`] and its three siblings —
//! see each family's module doc for its own table row.
//!
//! **Rejected when empty**, at the one place a namespace is minted
//! ([`KeyNamespace::new`]): an empty string is not "no namespace", it is a
//! namespace that collides with every other empty one, and a shared Redis
//! whose keys have no boundary segment is one deployment sharing a keyspace
//! with the next operator to make the same mistake.
//!
//! **No migration.** The version segment ([`SCHEMA_VERSION`]) is already in
//! the correlation maps' keys as of M14.1; the three older families gain it
//! with this rung, changing their key shape. No deployment holds a pre-rule
//! key of any of the three — none has shipped yet — so there is nothing to
//! convert and no test that could prove a converter right.

use std::fmt;

/// This crate's schema version, in every key every family writes.
///
/// One constant for all four families rather than one per family: nothing
/// has needed a second version since the correlation maps introduced the
/// first one (M14.1), and a family whose *value* encoding changes gains its
/// own constant then, the way `correlation`'s own module doc already
/// promises for a call binding that became a hash. Until that day, one
/// version is one fact instead of four that happen to agree by copy-paste.
pub(crate) const SCHEMA_VERSION: &str = "v1";

/// The deployment namespace every family's keys are built from.
///
/// **One type, so "is this namespace usable" is answered once, at
/// construction, rather than by four families each re-checking a bare
/// `String`** (R-S3). Every other operation this crate does with a namespace
/// — read it into a key — cannot fail, which is what makes accepting only a
/// validated [`KeyNamespace`] past this point sufficient rather than merely
/// convenient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNamespace(String);

/// What [`KeyNamespace::new`] refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyNamespace;

impl fmt::Display for EmptyNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "the Redis key namespace must not be empty — an empty namespace is not \
             \"no namespace\", it is one every other empty deployment shares",
        )
    }
}

impl std::error::Error for EmptyNamespace {}

impl KeyNamespace {
    /// Validate a namespace an operator named. `Err` for empty or
    /// all-whitespace, which reads identically to "not configured" if it
    /// reached a key unrejected — see the module doc.
    pub fn new(raw: impl Into<String>) -> Result<Self, EmptyNamespace> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(EmptyNamespace);
        }
        Ok(Self(raw))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for KeyNamespace {
    /// `rh`, the namespace every deployment and every test in this crate has
    /// used since before this rung named the concept. [`RedisSessionStore::connect`]
    /// and its three siblings still build this by default, so a caller that
    /// has not opted into an explicit namespace keeps exactly today's keys.
    ///
    /// [`RedisSessionStore::connect`]: crate::RedisSessionStore::connect
    fn default() -> Self {
        Self("rh".to_string())
    }
}

impl fmt::Display for KeyNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Build one shared-store key: `<namespace>:<version>:<family>[:<part>]...`.
///
/// **The one function every family calls.** `family` names which of the four
/// this key belongs to (`sess`, `spend`, `fairuse`, `corr`); `parts` are
/// whatever that family's own key functions already assemble — including a
/// Redis Cluster hash tag (`{...}`) wherever a family needs one for
/// multi-key atomicity, which this function has no opinion about and simply
/// concatenates like any other segment.
pub(crate) fn build_key(namespace: &KeyNamespace, family: &str, parts: &[&str]) -> String {
    let mut key = format!("{}:{SCHEMA_VERSION}:{family}", namespace.as_str());
    for part in parts {
        key.push(':');
        key.push_str(part);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_blank_namespace_is_refused() {
        assert!(KeyNamespace::new("").is_err());
        assert!(KeyNamespace::new("   ").is_err());
        assert!(KeyNamespace::new("\t\n").is_err());
    }

    #[test]
    fn a_non_empty_namespace_is_accepted_verbatim() {
        let namespace = KeyNamespace::new("acme-prod").unwrap();
        assert_eq!(namespace.to_string(), "acme-prod");
    }

    #[test]
    fn the_default_namespace_is_rh() {
        assert_eq!(KeyNamespace::default().to_string(), "rh");
    }

    #[test]
    fn build_key_joins_namespace_version_family_and_parts_with_colons() {
        let namespace = KeyNamespace::new("rh").unwrap();
        assert_eq!(
            build_key(&namespace, "corr", &["gen", "{acme/ada/main}"]),
            "rh:v1:corr:gen:{acme/ada/main}"
        );
        assert_eq!(build_key(&namespace, "sess", &[]), "rh:v1:sess");
    }

    #[test]
    fn two_namespaces_never_build_the_same_key() {
        let a = KeyNamespace::new("a").unwrap();
        let b = KeyNamespace::new("b").unwrap();
        assert_ne!(
            build_key(&a, "corr", &["gen", "{k}"]),
            build_key(&b, "corr", &["gen", "{k}"])
        );
    }
}
