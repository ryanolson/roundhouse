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
//! **Rejected when empty, and rejected when it would redefine the key
//! format's own delimiters** (M14.2 review, F6), at the one place a
//! namespace is minted ([`KeyNamespace::new`]): an empty string is not "no
//! namespace", it is a namespace that collides with every other empty one,
//! and a namespace carrying a Redis Cluster hash-tag brace, this crate's
//! `:` separator, or whitespace would silently change what those
//! characters mean in every key any family builds rather than naming a
//! boundary between deployments — a hash-tag namespace in particular
//! overrides every family's own tag and collapses the whole deployment
//! onto one Cluster slot, because Cluster hashes on the *first* `{...}`
//! pair it sees.
//!
//! **No migration.** The version segment (now [`KeyFamily::version`] per
//! family, M14.2 review, F10) is already in the correlation maps' keys as
//! of M14.1; the three older families gain it with this rung, changing
//! their key shape. No deployment holds a pre-rule key of any of the three
//! — none has shipped yet — so there is nothing to convert and no test
//! that could prove a converter right.

use std::fmt;

/// The closed set of key families this crate serves (M14.2 review, F7).
///
/// A `family: &str` at `build_key`'s call sites was four literals typed at
/// eleven places with nothing to check them against — a typo or a fifth
/// family both compiled. The variant is the check: a family that is not one
/// of these four does not compile, and the module-doc table test
/// (`tests/key_builder_convention.rs`) reads every name this enum's own
/// `name` method produces out of `keys.rs`'s source rather than a second
/// hand-copied list, so a doc-table row cannot drift from the variant it
/// describes without the parse itself changing shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyFamily {
    Session,
    Spend,
    FairUse,
    Correlation,
}

impl KeyFamily {
    /// Every variant — `#[cfg(test)]` because nothing outside this crate's
    /// own tests needs to enumerate them; the doc-table test in
    /// `tests/key_builder_convention.rs` reads the variants back out of
    /// `name`'s match arms instead, since an integration test cannot see a
    /// `pub(crate)` item.
    #[cfg(test)]
    const ALL: [KeyFamily; 4] = [
        KeyFamily::Session,
        KeyFamily::Spend,
        KeyFamily::FairUse,
        KeyFamily::Correlation,
    ];

    /// The family segment [`build_key`] writes.
    pub(crate) fn name(self) -> &'static str {
        match self {
            KeyFamily::Session => "sess",
            KeyFamily::Spend => "spend",
            KeyFamily::FairUse => "fairuse",
            KeyFamily::Correlation => "corr",
        }
    }

    /// This family's own schema version (M14.2 review, F10).
    ///
    /// One crate-wide `SCHEMA_VERSION` constant meant a v2 of *any* family's
    /// value encoding had no way to move without also moving every other
    /// family's key space — bumping the shared constant would have orphaned
    /// the durable session log to change a correlation binding's shape.
    /// Every family keeps `v1` today; the day `correlation`'s own module doc
    /// promise (a hash-shaped call binding is "a different key space") comes
    /// due, this is the one match arm that changes.
    pub(crate) fn version(self) -> &'static str {
        match self {
            KeyFamily::Session => "v1",
            KeyFamily::Spend => "v1",
            KeyFamily::FairUse => "v1",
            KeyFamily::Correlation => "v1",
        }
    }
}

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

/// What [`KeyNamespace::new`] refuses, and why.
///
/// Kept as one type under its original name — an external caller matches
/// `Result<KeyNamespace, EmptyNamespace>` at the composition root
/// (`roundhouse-server`'s `shared_backend`), and that boundary does not
/// change shape here — but the reason it carries widened with M14.2's F6:
/// blank is still refused, and now so is any namespace that redefines what
/// this crate's own key format already gives meaning to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyNamespace(Reason);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// Nothing to build a key from — reads identically to "not configured"
    /// if it reached a key unrejected.
    Blank,
    /// A namespace containing this character would change what every
    /// family's key means instead of refusing to build one. `{`/`}` are
    /// Redis Cluster's hash-tag delimiters (F6): Cluster hashes a key on
    /// its *first* `{...}` pair, so a namespace carrying one overrides
    /// every family's own tag and collapses the whole deployment onto one
    /// slot, silently. `:` is this crate's own field separator — a
    /// namespace containing one is indistinguishable from an extra
    /// segment. Whitespace is a key nobody will type back correctly.
    Forbidden(char),
}

impl fmt::Display for EmptyNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Reason::Blank => formatter.write_str(
                "the Redis key namespace must not be empty — an empty namespace is not \
                 \"no namespace\", it is one every other empty deployment shares",
            ),
            Reason::Forbidden(ch) => write!(
                formatter,
                "the Redis key namespace must not contain {ch:?} — a namespace \
                 carrying a Redis Cluster hash-tag brace, this crate's own `:` \
                 key separator, or whitespace changes what every family's keys \
                 mean instead of naming a boundary between deployments",
            ),
        }
    }
}

impl std::error::Error for EmptyNamespace {}

impl KeyNamespace {
    /// Validate a namespace an operator named. `Err` for empty or
    /// all-whitespace, which reads identically to "not configured" if it
    /// reached a key unrejected, and `Err` for a namespace that contains a
    /// character this crate's own key format already gives meaning to —
    /// `{`, `}`, `:`, or any whitespace — rather than accepting it verbatim
    /// and letting it silently override what that character means to
    /// every family that builds a key from it (M14.2 review, F6). See the
    /// module doc.
    pub fn new(raw: impl Into<String>) -> Result<Self, EmptyNamespace> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(EmptyNamespace(Reason::Blank));
        }
        if let Some(ch) = raw
            .chars()
            .find(|ch| matches!(ch, '{' | '}' | ':') || ch.is_whitespace())
        {
            return Err(EmptyNamespace(Reason::Forbidden(ch)));
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

/// Build one shared-store key: `<namespace>:<family's version>:<family>[:<part>]...`.
///
/// **The one function every family calls.** `family` is the closed
/// [`KeyFamily`] this key belongs to, whose own [`KeyFamily::version`] is
/// what lands in the key — not a crate-wide constant a family has no way to
/// move independently of the other three (M14.2 review, F10). `parts` are
/// whatever that family's own key functions already assemble — including a
/// Redis Cluster hash tag (`{...}`) wherever a family needs one for
/// multi-key atomicity, which this function has no opinion about and simply
/// concatenates like any other segment.
pub(crate) fn build_key(namespace: &KeyNamespace, family: KeyFamily, parts: &[&str]) -> String {
    let mut key = format!(
        "{}:{}:{}",
        namespace.as_str(),
        family.version(),
        family.name()
    );
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
            build_key(
                &namespace,
                KeyFamily::Correlation,
                &["gen", "{acme/ada/main}"]
            ),
            "rh:v1:corr:gen:{acme/ada/main}"
        );
        assert_eq!(build_key(&namespace, KeyFamily::Session, &[]), "rh:v1:sess");
    }

    #[test]
    fn two_namespaces_never_build_the_same_key() {
        let a = KeyNamespace::new("a").unwrap();
        let b = KeyNamespace::new("b").unwrap();
        assert_ne!(
            build_key(&a, KeyFamily::Correlation, &["gen", "{k}"]),
            build_key(&b, KeyFamily::Correlation, &["gen", "{k}"])
        );
    }

    /// `KeyFamily::ALL` is what the module-doc table test and
    /// `every_key_family_has_a_row_in_the_module_doc_table` both promise to
    /// keep in step with the enum — worth one direct check here that no two
    /// variants share a name segment, since two families landing on one
    /// `build_key` output would silently merge their keyspaces.
    #[test]
    fn every_family_has_a_distinct_name() {
        let names: Vec<&str> = KeyFamily::ALL.iter().map(|family| family.name()).collect();
        let mut deduped = names.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            names.len(),
            "two KeyFamily variants share one name segment: {names:?}"
        );
    }

    /// M14.2 thermo-nuclear review, F6 — was the red assertion, now closed.
    ///
    /// `how_to_prove` named exactly this: `KeyNamespace::new("{acme}")`
    /// should be refused, because Redis Cluster hashes a key on its first
    /// `{...}` pair, and a namespace carrying one silently overrides every
    /// family's own hash tag for the whole deployment. Before this fix `new`
    /// checked only `trim().is_empty()`, so this was `Ok`; `new` now scans
    /// for the characters this crate's own key format already gives meaning
    /// to, brace pairs included.
    #[test]
    fn key_namespace_new_rejects_a_namespace_containing_a_hash_tag() {
        assert!(
            KeyNamespace::new("{acme}").is_err(),
            "F6: a namespace that is itself (or contains) a Redis Cluster \
             hash tag should be refused at construction, before it can \
             override every family's own tag"
        );
    }

    /// F6 correction: the ruling named three forbidden shapes, not only the
    /// hash tag the finding's own `how_to_prove` covered — `:` collides with
    /// this crate's own field separator, and embedded whitespace is a key
    /// nobody will type back correctly.
    #[test]
    fn a_namespace_containing_a_colon_or_embedded_whitespace_is_also_refused() {
        assert!(KeyNamespace::new("acme:prod").is_err());
        assert!(KeyNamespace::new("acme prod").is_err());
        assert!(KeyNamespace::new("}acme").is_err());
    }
}
