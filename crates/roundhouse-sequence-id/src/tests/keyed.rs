// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use roundhouse_core::item::Item;
use roundhouse_core::item::chain::Chain;
use serde_json::Value;

use super::fixtures::{body, canonical};
use super::labels::{headers, view};
use crate::{
    Labeled, RequestView, SequenceDigester, SequenceKey, Surface, TipKey, TipKeyer, client_session,
    label, new_anchor, prefix_fingerprint, tools_digest,
};

const SECRET: &[u8] = b"deployment-secret";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fixture_chain(name: &str) -> Chain {
    Chain::over(&canonical(&body(name)))
}

#[test]
fn two_principals_never_share_a_tip_key() {
    let chain = fixture_chain("claude-2.1.257-turn-3-continue.json");
    let tools = tools_digest(body("claude-2.1.257-turn-3-continue.json").get("tools"));
    let principals = ["", "tenant-a/", "tenant-b/", "tenant-a"];
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    for principal in principals {
        let keys = TipKeyer::new(SECRET, principal).tip_keys(&chain, &tools);
        assert_eq!(keys.len(), chain.len());
        for key in keys {
            assert!(
                seen.insert(key.0),
                "a tip key crossed principals at {principal:?}"
            );
        }
    }
    // Control: one principal keys one chain the same way twice, so the
    // distinctness above is about the principal and not about randomness.
    let a = TipKeyer::new(SECRET, "tenant-a/").tip_keys(&chain, &tools);
    let b = TipKeyer::new(SECRET, "tenant-a/").tip_keys(&chain, &tools);
    assert!(a == b);
    // And a second secret is a second key space.
    let other = TipKeyer::new(b"another-secret", "tenant-a/").tip_keys(&chain, &tools);
    assert!(a.iter().zip(&other).all(|(x, y)| x != y));
}

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
fn claude_fixture_divergence_is_pinned() {
    let agreed = |a: &str, b: &str| fixture_chain(a).agreed_len(&fixture_chain(b));
    // Two sessions of one version, and two versions: item 0 is the
    // attribution block, whose fingerprint differs, so nothing is shared.
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-1.json",
            "claude-2.1.257-mcp-turn-1.json"
        ),
        0
    );
    assert_eq!(
        agreed("claude-2.1.251-turn-1.json", "claude-2.1.257-turn-1.json"),
        0
    );
    // One session, turn 1 to turn 2: the client rewrote system block 2 (its
    // model line) between the two, so the lineage keeps two links.
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-1.json",
            "claude-2.1.257-turn-2-continue.json"
        ),
        2
    );
    assert_eq!(
        agreed(
            "claude-2.1.251-turn-1.json",
            "claude-2.1.251-turn-2-continue.json"
        ),
        2
    );
    // Turn 2 to turn 3, and the tool loop, share everything the earlier
    // request had: the budget notice was dropped, not chained.
    let turn_two = fixture_chain("claude-2.1.257-turn-2-continue.json");
    assert_eq!(turn_two.len(), 8);
    assert_eq!(
        agreed(
            "claude-2.1.257-turn-2-continue.json",
            "claude-2.1.257-turn-3-continue.json"
        ),
        8
    );
    let tool_call = fixture_chain("claude-2.1.257-mcp-turn-1.json");
    assert_eq!(tool_call.len(), 6);
    assert_eq!(
        agreed(
            "claude-2.1.257-mcp-turn-1.json",
            "claude-2.1.257-mcp-turn-2-toolresult.json"
        ),
        6
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
    let parent_view = view(Surface::OpenAiResponses, &parent, Some("root"));
    let child_view = view(Surface::OpenAiResponses, &child, Some("root"));
    assert_eq!(client_session(&parent_view).as_deref(), Some("root"));
    assert_eq!(client_session(&parent_view), client_session(&child_view));

    let name = |view: &RequestView<'_>| match label(view) {
        Ok(Labeled::Named(label)) => label.name,
        other => panic!("{other:?}"),
    };
    let (parent_label, child_label) = (name(&parent_view), name(&child_view));
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
        hex(&t.0),
        "c0e1994d24ec7987b146a99f01d5dd7f65b3af70fe3ba2f9a4e1d57ceafdcad9"
    );

    let keyer = TipKeyer::new(SECRET, "tenant-a/");
    let tips: Vec<String> = keyer
        .tip_keys(&chain, &t)
        .iter()
        .map(|tip| hex(&tip.0))
        .collect();
    assert_eq!(
        tips,
        [
            "81500c8203770a4752953552e8027a93",
            "bcf063d7b38dc26d2158a9725eac6188"
        ]
    );
    assert_eq!(
        hex(&keyer.tip_key(&tools_digest(None), chain.link(0).unwrap()).0),
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
        hex(&digester.invalidation_id(&s2, None)),
        "3a8268bc54910302871d4747f1c0b690"
    );
    assert_eq!(
        hex(&digester.invalidation_id(&s2, Some(&s3))),
        "2eba8f2cc8917a72fe33f6be249ad0c1"
    );
}

#[test]
fn the_anchor_is_the_last_tip_of_the_first_prompt() {
    let keyer = TipKeyer::new(SECRET, "");
    let items = canonical(&body("claude-2.1.257-turn-1.json"));
    let keys = keyer.tip_keys(&Chain::over(&items), &tools_digest(None));
    let anchor = new_anchor(&keys).expect("a non-empty prompt has an anchor");
    assert_eq!(anchor.0, keys.last().unwrap().0);
    assert!(new_anchor(&[] as &[TipKey]).is_none());
}

#[test]
fn the_prefix_fingerprint_ends_at_the_first_user_item() {
    let items = [
        Item::system_text("be brief"),
        Item::user_text("hi"),
        Item::user_text("later"),
    ];
    let chain = Chain::over(&items);
    let fingerprint = prefix_fingerprint(&chain, &items);
    assert_eq!(
        fingerprint,
        "9d409093ad1f59ca3eeba8a2d517f652a1dbc27fffced49d70365d9a46b32460"
    );
    // Later items do not move it; an earlier edit does.
    assert_eq!(
        prefix_fingerprint(&Chain::over(&items[..2]), &items[..2]),
        fingerprint
    );
    let edited = [Item::system_text("be long"), Item::user_text("hi")];
    assert_ne!(
        prefix_fingerprint(&Chain::over(&edited), &edited),
        fingerprint
    );
    // A chain too short for the position is recomputed, never guessed.
    assert_eq!(prefix_fingerprint(&Chain::default(), &items), fingerprint);
    // No User item: through the last item. No items: the domain alone.
    let config = [Item::system_text("be brief")];
    assert_eq!(
        prefix_fingerprint(&Chain::over(&config), &config),
        prefix_fingerprint(&Chain::over(&items[..1]), &items[..1])
    );
    assert_eq!(
        prefix_fingerprint(&Chain::default(), &[]),
        "3953460c5cabd36f3509993492a6c72a831c14b2b166404a035240fd4f319924"
    );
}
