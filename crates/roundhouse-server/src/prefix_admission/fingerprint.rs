// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixed-size admission metadata without retained item payloads.
//!
//! Structural encoding preserves item matching without using rendered prompts.
//! Tool namespaces stay separate because an absent stored namespace is a wildcard.
//! JSON numbers preserve integer/float distinctions and normalize signed floating zero.
//! These digests are comparison metadata, not evidence of backend KV residency.
//! Debug output hides digests because they can confirm guesses about content.

use serde_json::{Number, Value};
use sha2::{Digest as _, Sha256};
use std::fmt;

use roundhouse_core::ids::ResponseId;
use roundhouse_core::item::{Item, ItemContent, Role};
use roundhouse_core::session::{TurnConfiguration, is_turn_configuration};

// Keep 128 bits of each SHA-256 digest to limit per-item metadata.
const WIDTH: usize = 16;

type Compact = [u8; WIDTH];

const ITEM_DOMAIN: &[u8] = b"rh-admit-item-v1\0";
const NAMESPACE_DOMAIN: &[u8] = b"rh-admit-namespace-v1\0";
const RESPONSE_DOMAIN: &[u8] = b"rh-admit-response-v1\0";

#[derive(Clone, Copy)]
pub(super) struct ItemFingerprint {
    content: Compact,
    namespace: NamespaceClaim,
    configuration: bool,
}

impl ItemFingerprint {
    pub(super) fn of(item: &Item) -> Self {
        let mut hash = Sha256::new();
        hash.update(ITEM_DOMAIN);
        hash.update([role_tag(item.role)]);
        content(&mut hash, &item.content);
        Self {
            content: compact(hash),
            namespace: NamespaceClaim::of(&item.content),
            configuration: is_turn_configuration(item),
        }
    }

    pub(super) fn matches(&self, claimed: &Self) -> bool {
        self.content == claimed.content && self.namespace.admits(claimed.namespace)
    }
}

impl TurnConfiguration for ItemFingerprint {
    fn is_turn_configuration(&self) -> bool {
        self.configuration
    }
}

impl fmt::Debug for ItemFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ItemFingerprint(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NamespaceClaim {
    Inapplicable,
    Absent,
    Present(Compact),
}

impl NamespaceClaim {
    fn of(content: &ItemContent) -> Self {
        match content {
            ItemContent::ToolCall { namespace, .. } => match namespace {
                None => Self::Absent,
                Some(namespace) => {
                    let mut hash = Sha256::new();
                    hash.update(NAMESPACE_DOMAIN);
                    field(&mut hash, namespace.as_bytes());
                    Self::Present(compact(hash))
                }
            },
            ItemContent::Text { .. }
            | ItemContent::ToolResult { .. }
            | ItemContent::Thinking { .. }
            | ItemContent::RedactedThinking { .. }
            | ItemContent::Opaque { .. } => Self::Inapplicable,
        }
    }

    fn admits(self, claimed: Self) -> bool {
        match self {
            Self::Absent => claimed != Self::Inapplicable,
            Self::Present(stored) => claimed == Self::Present(stored),
            Self::Inapplicable => claimed == Self::Inapplicable,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ResponseStamp(Compact);

impl ResponseStamp {
    pub(super) fn of(response_id: &ResponseId) -> Self {
        let mut hash = Sha256::new();
        hash.update(RESPONSE_DOMAIN);
        field(&mut hash, response_id.as_str().as_bytes());
        Self(compact(hash))
    }
}

impl fmt::Debug for ResponseStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResponseStamp(..)")
    }
}

fn compact(hash: Sha256) -> Compact {
    let full = hash.finalize();
    full[..WIDTH]
        .try_into()
        .expect("SHA-256 is wider than a fingerprint")
}

// Length prefixes separate adjacent variable-length fields.
fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn role_tag(role: Role) -> u8 {
    match role {
        Role::System => 1,
        Role::Developer => 2,
        Role::User => 3,
        Role::Assistant => 4,
        Role::Tool => 5,
    }
}

const TAG_TEXT: u8 = 1;
const TAG_TOOL_CALL: u8 = 2;
const TAG_TOOL_RESULT: u8 = 3;
const TAG_THINKING: u8 = 4;
const TAG_REDACTED_THINKING: u8 = 5;
const TAG_OPAQUE: u8 = 6;

fn content(hash: &mut Sha256, content: &ItemContent) {
    match content {
        ItemContent::Text { text } => {
            hash.update([TAG_TEXT]);
            field(hash, text.as_bytes());
        }
        ItemContent::ToolCall {
            call_id,
            name,
            arguments,
            namespace: _,
        } => {
            hash.update([TAG_TOOL_CALL]);
            field(hash, call_id.as_bytes());
            field(hash, name.as_bytes());
            field(hash, arguments.as_bytes());
        }
        ItemContent::ToolResult { call_id, output } => {
            hash.update([TAG_TOOL_RESULT]);
            field(hash, call_id.as_bytes());
            field(hash, output.as_bytes());
        }
        ItemContent::Thinking {
            thinking,
            signature,
        } => {
            hash.update([TAG_THINKING]);
            field(hash, thinking.as_bytes());
            field(hash, signature.as_bytes());
        }
        ItemContent::RedactedThinking { data } => {
            hash.update([TAG_REDACTED_THINKING]);
            field(hash, data.as_bytes());
        }
        ItemContent::Opaque { block_type, block } => {
            hash.update([TAG_OPAQUE]);
            field(hash, block_type.as_bytes());
            value(hash, block);
        }
    }
}

const TAG_NULL: u8 = 1;
const TAG_BOOL: u8 = 2;
const TAG_U64: u8 = 3;
const TAG_I64: u8 = 4;
const TAG_F64: u8 = 5;
const TAG_STRING: u8 = 6;
const TAG_ARRAY: u8 = 7;
const TAG_OBJECT: u8 = 8;
const TAG_NUMBER_LITERAL: u8 = 9;

fn value(hash: &mut Sha256, value: &Value) {
    match value {
        Value::Null => hash.update([TAG_NULL]),
        Value::Bool(flag) => {
            hash.update([TAG_BOOL]);
            hash.update([u8::from(*flag)]);
        }
        Value::Number(number) => self::number(hash, number),
        Value::String(text) => {
            hash.update([TAG_STRING]);
            field(hash, text.as_bytes());
        }
        Value::Array(elements) => {
            hash.update([TAG_ARRAY]);
            hash.update((elements.len() as u64).to_be_bytes());
            for element in elements {
                self::value(hash, element);
            }
        }
        Value::Object(members) => {
            hash.update([TAG_OBJECT]);
            hash.update((members.len() as u64).to_be_bytes());
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort_unstable();
            for key in keys {
                field(hash, key.as_bytes());
                self::value(hash, &members[key]);
            }
        }
    }
}

// Match serde_json numeric equality under the workspace feature configuration.
fn number(hash: &mut Sha256, number: &Number) {
    if let Some(unsigned) = number.as_u64() {
        hash.update([TAG_U64]);
        hash.update(unsigned.to_be_bytes());
    } else if let Some(signed) = number.as_i64() {
        hash.update([TAG_I64]);
        hash.update(signed.to_be_bytes());
    } else if let Some(float) = number.as_f64() {
        // `0.0 == -0.0`, so they must hash alike; see this module's note.
        let float = if float == 0.0 { 0.0 } else { float };
        hash.update([TAG_F64]);
        hash.update(float.to_be_bytes());
    } else {
        hash.update([TAG_NUMBER_LITERAL]);
        field(hash, number.to_string().as_bytes());
    }
}
