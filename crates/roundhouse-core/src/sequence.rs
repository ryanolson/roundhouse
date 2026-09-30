// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The plain values of session and sequence identity, as records hold them.
//!
//! **Here and not in `roundhouse-sequence-id`, because core stores them.** A
//! session record carries an [`Anchor`], a turn's signals carry a
//! [`CompactionKind`], and a placement store is keyed by [`TipKey`]; every one
//! of those types is core's. That crate depends on core, so a value defined
//! there could never appear in a record defined here without core depending
//! back on the crate that derives it. What stays in the crate is everything
//! *keyed* — the deployment secret, the HMACs, the principal scoping — so this
//! module holds the bytes and nothing that can mint them.
//!
//! **No wire shape is chosen for the byte values yet.** [`Anchor`] and
//! [`SequenceDigest`] carry no `serde` derive on purpose: no record holds one
//! today, and the derive would serialize `[u8; 16]` as a JSON array of sixteen
//! integers — a format nobody chose, which the first record to store one would
//! then be stuck with. The milestone that adds that record picks the shape with
//! it. [`CompactionKind`] does derive, because its `snake_case` names are the
//! client's own words and the shape a turn's signals record in.
//!
//! ```compile_fail
//! fn needs_serialize<T: serde::Serialize>() {}
//! needs_serialize::<roundhouse_core::sequence::Anchor>();
//! ```
//!
//! ```compile_fail
//! fn needs_serialize<T: serde::Serialize>() {}
//! needs_serialize::<roundhouse_core::sequence::SequenceDigest>();
//! ```
//!
//! The control, so the two above fail for the missing derive and not for a
//! path that stopped resolving:
//!
//! ```
//! fn needs_serialize<T: serde::Serialize>() {}
//! needs_serialize::<roundhouse_core::sequence::CompactionKind>();
//! ```

use serde::{Deserialize, Serialize};

/// `L_i`: the stored key of one chain link, for one principal and one tool set.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TipKey(pub [u8; 16]);

/// A KV lineage family. A fork that resends a lineage's prompt shares it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Anchor(pub [u8; 16]);

/// `S`, sent as `x-dynamo-session-id`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SequenceDigest(pub [u8; 16]);

impl SequenceDigest {
    /// All 32 hex characters: the header value.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The first 8 hex characters: the most a log line may carry (§3.8).
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

/// How a compaction was started, in the client's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    Auto,
    Manual,
    Reactive,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one serialized shape this module chooses: the client's own words,
    /// because a turn's signals record them and a renamed variant must not
    /// change what a stored record reads back as.
    #[test]
    fn a_compaction_kind_serializes_as_the_clients_word() {
        for (kind, word) in [
            (CompactionKind::Auto, "\"auto\""),
            (CompactionKind::Manual, "\"manual\""),
            (CompactionKind::Reactive, "\"reactive\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), word);
            assert_eq!(serde_json::from_str::<CompactionKind>(word).unwrap(), kind);
        }
    }

    #[test]
    fn a_sequence_digest_prints_as_lowercase_hex() {
        let digest = SequenceDigest([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0, 0, 0, 0, 0, 0, 0, 0xff,
        ]);
        assert_eq!(digest.to_hex(), "0123456789abcdef00000000000000ff");
        assert_eq!(digest.short(), "01234567");
    }
}
