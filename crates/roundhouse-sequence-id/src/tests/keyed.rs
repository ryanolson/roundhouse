// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use hex::encode as hex;
use roundhouse_core::item::Item;
use roundhouse_core::item::chain::Chain;
use serde_json::Value;

use super::labels::headers;
use roundhouse_core::sequence::TipKey;

use crate::{
    CodexHeaders, SequenceDigester, SequenceKey, Surface, TipKeyer, new_anchor, prefix_fingerprint,
    tools_digest,
};

const SECRET: &[u8] = b"deployment-secret";

#[test]
fn the_tools_digest_ignores_key_order() {
    let sorted: Value = serde_json::from_str(
        r#"[{"description":"list","input_schema":{"a":1,"b":2},"name":"ls"}]"#,
    )
    .unwrap();
    let shuffled: Value = serde_json::from_str(
        r#"[{"name":"ls","input_schema":{"b":2,"a":1},"description":"list"}]"#,
    )
    .unwrap();
    assert_eq!(
        tools_digest(Some(&sorted)).0,
        tools_digest(Some(&shuffled)).0
    );
    // Order *within the list* is a different declaration, and so is any edit.
    let renamed: Value = serde_json::from_str(
        r#"[{"description":"list","input_schema":{"a":1,"b":2},"name":"cat"}]"#,
    )
    .unwrap();
    assert_ne!(
        tools_digest(Some(&sorted)).0,
        tools_digest(Some(&renamed)).0
    );
    // None declared is the zero digest; a declared empty list is not.
    assert_eq!(tools_digest(None).0, [0; 32]);
    assert_eq!(tools_digest(Some(&Value::Null)).0, [0; 32]);
    assert_ne!(tools_digest(Some(&Value::Array(vec![]))).0, [0; 32]);
    // A tool change moves the tip, not the chain.
    let chain = Chain::over(&[Item::user_text("hi")]);
    let keyer = TipKeyer::new(SECRET, "");
    assert!(
        keyer.tip_keys(&chain, &tools_digest(Some(&sorted)))
            != keyer.tip_keys(&chain, &tools_digest(Some(&renamed)))
    );
}

#[test]
fn the_sequence_digest_separates_generations_labels_surfaces_and_principals() {
    let digester = SequenceDigester::new(SECRET);
    let base = SequenceKey {
        namespace: "tenant-a/",
        surface: Surface::OpenAiResponses,
        label: "thread-1",
        generation: 0,
    };
    let variants = [
        SequenceKey {
            generation: 1,
            ..base
        },
        SequenceKey {
            label: "thread-2",
            ..base
        },
        SequenceKey {
            surface: Surface::AnthropicMessages,
            ..base
        },
        SequenceKey {
            namespace: "tenant-b/",
            ..base
        },
        SequenceKey {
            namespace: "",
            ..base
        },
    ];
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    assert!(seen.insert(digester.digest(&base).0));
    for variant in &variants {
        assert!(
            seen.insert(digester.digest(variant).0),
            "a field did not separate"
        );
    }
    // Two keys whose fields join to one byte string,
    // `anthropic_messages` + `openai_responses` + `foo`, split at different
    // places. Only the length prefixes keep them apart: moving text between
    // the namespace and the label alone would not test this, because the
    // fixed surface name between them already breaks that ambiguity.
    let joined_a = SequenceKey {
        namespace: "",
        surface: Surface::AnthropicMessages,
        label: "openai_responsesfoo",
        generation: 0,
    };
    let joined_b = SequenceKey {
        namespace: "anthropic_messages",
        surface: Surface::OpenAiResponses,
        label: "foo",
        generation: 0,
    };
    assert!(
        digester.digest(&joined_a) != digester.digest(&joined_b),
        "two splits of one joined key named one sequence"
    );
    assert!(digester.digest(&base) == SequenceDigester::new(SECRET).digest(&base));
    assert!(digester.digest(&base) != SequenceDigester::new(b"other").digest(&base));

    let digest = digester.digest(&base);
    assert_eq!(digest.to_hex().len(), 32);
    assert_eq!(digest.short().len(), 8);
    assert!(digest.to_hex().starts_with(&digest.short()));

    // An invalidation names both ends, and is never the digest of either.
    let next = digester.digest(&SequenceKey {
        generation: 1,
        ..base
    });
    let closing = digester.invalidation_id(&digest, None);
    let superseding = digester.invalidation_id(&digest, Some(&next));
    assert_ne!(closing, superseding);
    assert_ne!(closing, digest.0);
    assert_ne!(superseding, next.0);
}

#[test]
fn a_codex_family_shares_a_session_but_never_a_sequence() {
    // Every sub-agent of one Codex root sends the root's `session-id`, and by
    // default its `prompt_cache_key` too; only `thread-id` is per thread.
    let parent = headers(&[("session-id", "root"), ("thread-id", "root")]);
    let child = headers(&[("session-id", "root"), ("thread-id", "child-1")]);
    let parent = CodexHeaders::read(&parent).unwrap();
    let child = CodexHeaders::read(&child).unwrap();
    assert_eq!(parent.session(), Some("root"));
    assert_eq!(parent.session(), child.session());

    let (parent_label, child_label) = (
        parent.label(Some("root")).unwrap(),
        child.label(Some("root")).unwrap(),
    );
    assert_ne!(parent_label, child_label);
    let digester = SequenceDigester::new(SECRET);
    let digest = |label: &str| {
        digester.digest(&SequenceKey {
            namespace: "",
            surface: Surface::OpenAiResponses,
            label,
            generation: 0,
        })
    };
    assert!(digest(&parent_label) != digest(&child_label));
}

/// Known-answer vectors for every keyed formula of §3.2, computed outside the
/// crate (Python `hmac`/`hashlib`) from the formulas as the plan writes them.
///
/// The values are compared across nodes and releases — tips in a shared store,
/// `S` in a deployment's session table — so a formula change orphans all of
/// them at once while every behavioural test still passes, both sides of each
/// comparison having moved together. Only a pinned vector sees that.
#[test]
fn the_keyed_formulas_are_pinned() {
    let chain = Chain::over(&[Item::system_text("be brief"), Item::user_text("hi")]);
    let tools: Value = serde_json::from_str(r#"[{"name":"ls","description":"list"}]"#).unwrap();
    let t = tools_digest(Some(&tools));
    assert_eq!(
        hex(t.0),
        "c0e1994d24ec7987b146a99f01d5dd7f65b3af70fe3ba2f9a4e1d57ceafdcad9"
    );

    let keyer = TipKeyer::new(SECRET, "tenant-a/");
    let tips: Vec<String> = keyer
        .tip_keys(&chain, &t)
        .iter()
        .map(|tip| hex(tip.0))
        .collect();
    assert_eq!(
        tips,
        [
            "81500c8203770a4752953552e8027a93",
            "bcf063d7b38dc26d2158a9725eac6188"
        ]
    );
    assert_eq!(
        hex(keyer.tip_key(&tools_digest(None), chain.link(0).unwrap()).0),
        "e4ddc80d8a5b6a7ad38be25f7211c1b0"
    );

    let digester = SequenceDigester::new(SECRET);
    let key = |generation| SequenceKey {
        namespace: "tenant-a/",
        surface: Surface::OpenAiResponses,
        label: "thread-1",
        generation,
    };
    let (s2, s3) = (digester.digest(&key(2)), digester.digest(&key(3)));
    assert_eq!(s2.to_hex(), "d0b4386618f38ac8b286fdbceda037ac");
    assert_eq!(s2.short(), "d0b43866");
    assert_eq!(s3.to_hex(), "84953ef479e1bca9aa9f595e93047dee");
    assert_eq!(
        hex(digester.invalidation_id(&s2, None)),
        "3a8268bc54910302871d4747f1c0b690"
    );
    assert_eq!(
        hex(digester.invalidation_id(&s2, Some(&s3))),
        "2eba8f2cc8917a72fe33f6be249ad0c1"
    );
}

#[test]
fn the_prefix_fingerprint_ends_at_the_first_user_item() {
    let items = [
        Item::system_text("be brief"),
        Item::user_text("hi"),
        Item::user_text("later"),
    ];
    let fingerprint = prefix_fingerprint(&items);
    assert_eq!(
        fingerprint,
        "9d409093ad1f59ca3eeba8a2d517f652a1dbc27fffced49d70365d9a46b32460"
    );
    // Later items do not move it; an earlier edit does.
    assert_eq!(prefix_fingerprint(&items[..2]), fingerprint);
    let edited = [Item::system_text("be long"), Item::user_text("hi")];
    assert_ne!(prefix_fingerprint(&edited), fingerprint);
    // No User item: through the last item. No items: the domain alone.
    let config = [Item::system_text("be brief")];
    assert_eq!(prefix_fingerprint(&config), prefix_fingerprint(&items[..1]));
    assert_eq!(
        prefix_fingerprint(&[]),
        "3953460c5cabd36f3509993492a6c72a831c14b2b166404a035240fd4f319924"
    );
}

/// Mutation 21e: `wire_name` enters the digest and is compared across
/// releases, so both spellings are literals. The known-answer vectors pin only
/// the Responses one.
#[test]
fn both_surface_wire_names_are_fixed_strings() {
    assert_eq!(Surface::AnthropicMessages.wire_name(), "anthropic_messages");
    assert_eq!(Surface::OpenAiResponses.wire_name(), "openai_responses");
}

/// Mutation 26j: with no `User` item the fingerprint runs through the last
/// item. The existing case compared two values that both hashed the domain
/// alone under the mutation, so it could not tell.
#[test]
fn a_config_only_prompt_fingerprints_its_last_item() {
    let a = [Item::system_text("be brief")];
    let b = [Item::system_text("be long")];
    let c = [Item::system_text("be brief"), Item::system_text("be kind")];
    assert_ne!(prefix_fingerprint(&a), prefix_fingerprint(&b));
    assert_ne!(prefix_fingerprint(&a), prefix_fingerprint(&c));
    assert_ne!(prefix_fingerprint(&a), prefix_fingerprint(&[]));
}

/// **The anchor is the last tip of the first prompt**, not the first: every
/// prompt of one client opens with the same system run, so a first-tip anchor
/// would put every session of a deployment into one lineage family. An empty
/// first prompt names no lineage at all.
#[test]
fn a_new_anchor_is_the_last_first_prompt_tip() {
    let first = TipKey([1; 16]);
    let last = TipKey([2; 16]);
    assert!(new_anchor(&[]).is_none());
    assert_eq!(
        new_anchor(&[first, last]).map(|anchor| anchor.0),
        Some(last.0)
    );
}
