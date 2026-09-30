// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The unkeyed prefix chain over canonical items.
//!
//! One link per item: `c_i` names items `0..=i` and nothing else, so two
//! histories agree on their first `n` links exactly when they agree on their
//! first `n` renders. Everything keyed — the tip keys a store holds, the
//! sequence digest a deployment sees — is built *on* these links in
//! `roundhouse-sequence-id`; this module is only the part the context assembler
//! can extend from a render it already computed, which is why it lives in core
//! rather than in the crate that depends on core.
//!
//! ```text
//! r_i = items[i].render()
//! d_i = SHA-256("rh-item-v1\0" || u64be(len(r_i)) || r_i)
//! c_0 = SHA-256("rh-chain-v1\0" || d_0)
//! c_i = SHA-256(c_{i-1} || d_i)
//! ```
//!
//! **The input is [`Item::render`], and that choice carries two rulings with
//! it.** The render leaves out a tool call's `namespace` and an item's
//! `response_id`, so the chain does too — which is what makes it agree with
//! prefix admission's `same_item`, where a stored `None` namespace agrees with
//! any claim and the stamp is never compared. A chain that hashed the serde
//! form instead would break on the second turn of every Codex session that
//! made an MCP call before M17, and on every assistant item this deployment
//! stamped: two histories admission calls one conversation would look like
//! two lineages, and a continuation would be placed as new.
//!
//! **Configuration is in the chain.** A rewritten configuration run loses the
//! KV beyond it on every target, so the chain breaks there too. Tools are not:
//! they enter only the keyed tip, where a changed declaration makes a new tip
//! the way it makes a new provider cache prefix.
//!
//! # Why a [`ChainValue`] cannot be printed or stored
//!
//! A link is an unkeyed digest of conversation content. Anyone holding one can
//! confirm a guess at the prefix it names — a system prompt, a pasted file —
//! by hashing the guess, and two principals with one prompt share every link.
//! So a link lives in process memory only: the store holds HMAC outputs keyed
//! per principal, and the only thing that reads a link's bytes is the keying.
//! That is enforced by the type rather than by review — no `Serialize`, no
//! `Display`, no hex, and a `Debug` that redacts — so a link cannot reach a
//! log line or a stored record by an accident of `derive`:
//!
//! ```compile_fail
//! fn needs_serialize<T: serde::Serialize>() {}
//! needs_serialize::<roundhouse_core::item::chain::ChainValue>();
//! ```
//!
//! ```compile_fail
//! fn needs_display<T: std::fmt::Display>() {}
//! needs_display::<roundhouse_core::item::chain::ChainValue>();
//! ```
//!
//! The per-item digest a link is built from is not exported at all, so it
//! cannot leak by a derive either:
//!
//! ```compile_fail
//! let _ = roundhouse_core::item::chain::item_digest("a secret prompt");
//! ```
//!
//! The control, so the three above fail for the reason they name and not for a
//! path that stopped resolving: the same shape with `Debug` compiles, and what
//! it prints carries no byte of the link.
//!
//! ```
//! use roundhouse_core::item::{Item, chain::{Chain, ChainValue}};
//! fn needs_debug<T: std::fmt::Debug>() {}
//! needs_debug::<ChainValue>();
//! let chain = Chain::over(&[Item::user_text("a secret prompt")]);
//! assert_eq!(format!("{:?}", chain.link(0).unwrap()), "ChainValue(..)");
//! ```

use std::fmt;

use sha2::{Digest, Sha256};

use super::Item;

/// Domain prefix of an item digest. Each keyed and unkeyed value built from
/// the chain has its own, so no two of them can be equal by construction.
const ITEM_DOMAIN: &[u8] = b"rh-item-v1\0";

/// Domain prefix of the first link, which has no predecessor to extend.
const CHAIN_DOMAIN: &[u8] = b"rh-chain-v1\0";

/// The digest of one item's render, `d_i`.
///
/// Length-prefixed so the formula stays unambiguous if a later version ever
/// hashes a second variable field beside the render. Redacted in `Debug` for
/// the same reason a link is: it confirms a guess at one item's content.
///
/// Private to core, field and all: it is as sensitive as a link, and nothing
/// outside needs one — a caller that holds a render extends a [`Chain`] with
/// [`Chain::push_rendered`] and gets the link, which is what everything keyed
/// is built on. Exported, it would be an unredacted second way to the bytes
/// the link's type exists to fence.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ItemDigest([u8; 32]);

impl fmt::Debug for ItemDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ItemDigest(..)")
    }
}

/// `d_i` of a render already computed — the one the context assembler holds.
pub(crate) fn item_digest(render: &str) -> ItemDigest {
    let mut hash = Sha256::new();
    hash.update(ITEM_DOMAIN);
    hash.update((render.len() as u64).to_be_bytes());
    hash.update(render.as_bytes());
    ItemDigest(hash.finalize().into())
}

/// One link, `c_i`: the unkeyed digest of items `0..=i`.
///
/// No `Serialize`, no `Display`, no hex, and a redacting `Debug`: an unkeyed
/// chain value must not reach a store, a log, or a wire by accident. The module
/// doc says why and pins it with `compile_fail` doctests.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainValue([u8; 32]);

impl ChainValue {
    /// The link's bytes, for the keyed derivations and nothing else.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ChainValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChainValue(..)")
    }
}

/// The links of one item list, in order.
#[derive(Default, Clone)]
pub struct Chain {
    links: Vec<ChainValue>,
}

impl fmt::Debug for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chain")
            .field("len", &self.links.len())
            .finish_non_exhaustive()
    }
}

impl Chain {
    /// The chain of a whole item list.
    pub fn over(items: &[Item]) -> Self {
        let mut chain = Self {
            links: Vec::with_capacity(items.len()),
        };
        for item in items {
            chain.push(item);
        }
        chain
    }

    /// Extend by one item, rendering it.
    pub fn push(&mut self, item: &Item) -> &ChainValue {
        self.push_rendered(&item.render())
    }

    /// Extend by one item whose render the caller already holds.
    ///
    /// The assembler renders every item to tokenize it; rendering again here
    /// would double the one cost the chain adds per turn. It takes the render
    /// and not the item so that the two can never disagree about what the
    /// render was — which is what
    /// `the_chain_over_renders_equals_the_chain_over_items` pins.
    pub fn push_rendered(&mut self, render: &str) -> &ChainValue {
        let digest = item_digest(render);
        let mut hash = Sha256::new();
        match self.links.last() {
            Some(previous) => hash.update(previous.0),
            None => hash.update(CHAIN_DOMAIN),
        }
        hash.update(digest.0);
        self.links.push(ChainValue(hash.finalize().into()));
        self.links.last().expect("a link was just pushed")
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn link(&self, index: usize) -> Option<&ChainValue> {
        self.links.get(index)
    }

    pub fn links(&self) -> &[ChainValue] {
        &self.links
    }

    /// Leading links equal in both chains.
    ///
    /// Each link already commits to every earlier item, so the first unequal
    /// link ends the agreement and nothing after it can restore it.
    pub fn agreed_len(&self, other: &Chain) -> usize {
        self.links
            .iter()
            .zip(&other.links)
            .take_while(|(a, b)| a == b)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::ids::ResponseId;
    use crate::item::{ItemContent, Role};

    fn history() -> Vec<Item> {
        vec![
            Item::system_text("be brief"),
            Item::user_text("list the files"),
            Item::namespaced_tool_call("c1", "ls", Some("mcp__fs".into()), r#"{"path":"."}"#),
            Item {
                role: Role::Tool,
                content: ItemContent::ToolResult {
                    call_id: "c1".into(),
                    output: "a.rs\nb.rs".into(),
                },
                response_id: None,
            },
            Item::assistant_text("two files", ResponseId::new("resp_1")),
            Item::user_text("thanks"),
        ]
    }

    fn unstamped(mut item: Item) -> Item {
        item.response_id = None;
        item
    }

    #[test]
    fn a_response_stamp_does_not_move_the_chain() {
        // The client resends assistant history with no id attached; only the
        // copy this deployment stored carries one. Admission already ignores
        // the stamp, so a chain that did not would call one conversation two
        // lineages on every turn after the first.
        let stored = history();
        let resent: Vec<Item> = stored.iter().cloned().map(unstamped).collect();
        assert!(
            stored[4].response_id.is_some(),
            "the fixture must stamp one"
        );
        assert_eq!(Chain::over(&stored).agreed_len(&Chain::over(&resent)), 6);
        assert_eq!(
            Chain::over(&stored).links(),
            Chain::over(&resent).links(),
            "the stamp moved a link"
        );
    }

    #[test]
    fn a_tool_call_namespace_does_not_move_the_chain() {
        // A record written before M17 has `None`; the same call resent by a
        // current Codex has `Some`. Admission agrees on the two, so the chain
        // must too, or every such session would be placed as new.
        let with = history();
        let mut without = history();
        without[2] = Item::tool_call("c1", "ls", r#"{"path":"."}"#);
        let mut other = history();
        other[2] = Item::namespaced_tool_call("c1", "ls", Some("mcp__x".into()), r#"{"path":"."}"#);
        for variant in [&without, &other] {
            assert_eq!(
                Chain::over(&with).agreed_len(&Chain::over(variant)),
                with.len(),
                "a namespace moved the chain"
            );
        }
        // Control: the same position with a changed *name* does move it, so
        // the agreement above is about the namespace and not a blind spot.
        let mut renamed = history();
        renamed[2] =
            Item::namespaced_tool_call("c1", "cat", Some("mcp__fs".into()), r#"{"path":"."}"#);
        assert_eq!(Chain::over(&with).agreed_len(&Chain::over(&renamed)), 2);
    }

    #[test]
    fn an_opaque_block_digests_the_same_in_any_key_order() {
        // A chained Relay re-serializes bodies through an alphabetizing map, so
        // turn two can arrive with every key reordered. The render digests the
        // parsed value, and the chain inherits that insensitivity.
        let opaque = |json: &str| Item {
            role: Role::User,
            content: ItemContent::Opaque {
                block_type: "image".into(),
                block: serde_json::from_str::<Value>(json).unwrap(),
            },
            response_id: None,
        };
        let a = opaque(
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}"#,
        );
        let b = opaque(
            r#"{"source":{"data":"AAAA","media_type":"image/png","type":"base64"},"type":"image"}"#,
        );
        assert_eq!(item_digest(&a.render()), item_digest(&b.render()));
        assert_eq!(
            Chain::over(std::slice::from_ref(&a)).links(),
            Chain::over(&[b]).links()
        );
        // Control: a changed payload changes the link.
        let c = opaque(
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAB"}}"#,
        );
        assert_ne!(Chain::over(&[a]).links(), Chain::over(&[c]).links());
    }

    #[test]
    fn a_chain_link_depends_on_every_earlier_item() {
        let base = history();
        let chain = Chain::over(&base);
        assert_eq!(chain.len(), base.len());
        for changed in 0..base.len() {
            let mut edited = base.clone();
            edited[changed] = Item::user_text(format!("edited {changed}"));
            let other = Chain::over(&edited);
            assert_eq!(
                chain.agreed_len(&other),
                changed,
                "an edit at {changed} must leave exactly the links before it"
            );
            // Every later link moves, not only the first unequal one: a link
            // that stopped committing to its predecessor would agree again
            // after the edit, and `agreed_len` alone would never see it.
            for later in changed..base.len() {
                assert_ne!(
                    chain.link(later),
                    other.link(later),
                    "link {later} after an edit at {changed}"
                );
            }
        }
        // Order is content too: the same items swapped are a different chain.
        let mut swapped = base.clone();
        swapped.swap(1, 5);
        assert_eq!(chain.agreed_len(&Chain::over(&swapped)), 1);
    }

    #[test]
    fn the_chain_over_renders_equals_the_chain_over_items() {
        let items = history();
        let mut rendered = Chain::default();
        for item in &items {
            rendered.push_rendered(&item.render());
        }
        let mut pushed = Chain::default();
        for item in &items {
            pushed.push(item);
        }
        assert_eq!(rendered.links(), Chain::over(&items).links());
        assert_eq!(pushed.links(), Chain::over(&items).links());
        assert_eq!(Chain::over(&[]).len(), 0);
        assert!(Chain::over(&[]).is_empty());
    }

    /// Known-answer vectors for the chain formula, computed outside this crate
    /// (Python `hashlib`) from the formula in the module doc.
    ///
    /// Tips are stored in a shared store and compared across nodes and
    /// releases, so a change to the formula is not a refactor: it orphans every
    /// stored tip at once, and every continuation is placed as new with no test
    /// of behaviour noticing, because both sides of every comparison moved
    /// together. Only a pinned vector sees that.
    #[test]
    fn the_chain_formula_is_pinned() {
        let chain = Chain::over(&[Item::system_text("be brief"), Item::user_text("hi")]);
        assert_eq!(
            hex::encode(item_digest("<|system|>be brief").0),
            "8f11135bbb8b0a0b73997c7054bee25516680933c8a4588a7f005ba5f6d09ebf"
        );
        assert_eq!(
            hex::encode(chain.link(0).unwrap().as_bytes()),
            "2438d9577d6a45e5c67ea4e811d26d12a0d84e686d15e32187cc374c6905e31a"
        );
        assert_eq!(
            hex::encode(chain.link(1).unwrap().as_bytes()),
            "b71853b003562986e8547e85af601d56321dcf8af14091e8a4fde471589d4b73"
        );
    }

    #[test]
    fn debug_output_carries_no_byte_of_a_link() {
        let chain = Chain::over(&history());
        let printed = format!(
            "{chain:?} {:?} {:?}",
            chain.link(0).unwrap(),
            item_digest("x")
        );
        assert_eq!(
            printed,
            "Chain { len: 6, .. } ChainValue(..) ItemDigest(..)"
        );
    }
}
