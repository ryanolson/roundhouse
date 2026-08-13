// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Real BPE [`Tokenizer`] backed by the HuggingFace `tokenizers` crate.
//!
//! [`ByteTokenizer`](roundhouse_core::context::ByteTokenizer) exists so the
//! token buffer can be tested without a vocabulary. This is what a real
//! deployment plugs in instead: block hashes are only meaningful to a worker
//! if the token ids match what that worker's own tokenizer would produce, and
//! that requires the worker's actual vocabulary and merge rules.

use std::path::Path;

use roundhouse_core::context::Tokenizer;

/// [`Tokenizer`] backed by a loaded `tokenizers::Tokenizer`.
///
/// `tokenizers::Tokenizer` is `Clone`, which is what lets this satisfy the
/// `Tokenizer + Clone` bound the engine needs to hand the same tokenizer to
/// every session it assembles context for.
#[derive(Clone)]
pub struct HfTokenizer {
    inner: tokenizers::Tokenizer,
}

impl HfTokenizer {
    /// Loads a tokenizer from a HuggingFace `tokenizer.json` file.
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let inner = tokenizers::Tokenizer::from_file(path)
            .map_err(|err| anyhow::anyhow!("failed to load tokenizer: {err}"))?;
        Ok(Self { inner })
    }
}

impl Tokenizer for HfTokenizer {
    /// Encodes `text` into raw token ids, with no special tokens injected.
    ///
    /// Roundhouse builds the prompt's token stream incrementally: each item is
    /// rendered to text and encoded on its own, and the resulting ids are
    /// appended to one running [`TokenBuffer`](roundhouse_core::context::TokenBuffer)
    /// whose block hashes are the routing currency. If this call injected a
    /// BOS/EOS pair per invocation, every appended item would splice special
    /// tokens into the middle of that stream, and the buffer's hashes would
    /// stop matching the hashes of the single sequence a worker actually sees.
    /// Special-token structure is a property of how an item is *rendered* into
    /// text (the chat template's job), not of how rendered text becomes ids —
    /// so this always calls the inner tokenizer with `add_special_tokens =
    /// false`.
    ///
    /// The trait is infallible on purpose: routing needs token ids to exist,
    /// unconditionally, for every item that gets pushed. A tokenizer that
    /// cannot encode the conversation cannot route it either, so an encode
    /// error panics here instead of surfacing as a `Result` some caller could
    /// swallow — dying loudly beats silently mis-hashing the routing key.
    fn encode(&self, text: &str) -> Vec<u32> {
        let encoding = self.inner.encode(text, false).unwrap_or_else(|err| {
            panic!("HfTokenizer: encode failed ({} bytes): {err}", text.len())
        });
        encoding.get_ids().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TinyLlama v1.1 tokenizer (Apache-2.0), copied from ai-dynamo/dynamo test
    // assets at rev ac7b751, sha256 bcd04f0e....
    fn fixture() -> HfTokenizer {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tinyllama-tokenizer.json");
        HfTokenizer::from_file(path).expect("fixture tokenizer must load")
    }

    #[test]
    fn loading_and_encoding_succeed_and_are_deterministic() {
        let tokenizer = fixture();
        let text = "The quick brown fox jumps over the lazy dog, twice.";

        let first = tokenizer.encode(text);
        let second = tokenizer.encode(text);

        assert!(!first.is_empty());
        assert_eq!(first, second, "encoding the same text twice must agree");
    }

    #[test]
    fn no_special_tokens_are_injected() {
        let tokenizer = fixture();

        // TinyLlama's BOS is id 1 (token "<s>"). If add_special_tokens were
        // left on, every encode() would start with it.
        const BOS: u32 = 1;
        let ids = tokenizer.encode("hello");
        assert_ne!(
            ids.first().copied(),
            Some(BOS),
            "encode() must not prepend BOS -- special-token structure belongs to item rendering"
        );

        // An empty item renders to no tokens at all, not a bare BOS/EOS pair.
        assert_eq!(tokenizer.encode(""), Vec::<u32>::new());
    }

    #[test]
    fn boundary_tokens_depend_on_what_they_are_encoded_alongside() {
        let tokenizer = fixture();

        // Real BPE merges across whatever characters are adjacent at encode
        // time, so splitting a string before encoding can change the pieces
        // chosen right at the split point. Concretely, for this fixture:
        //   encode("<|user|>hel") ++ encode("lo world")
        //     != encode("<|user|>hello world")
        // because "hel" alone and the "hel" prefix of "hello" get merged
        // differently once "lo" is available to merge with. This is exactly
        // why the incrementally-built [`TokenBuffer`] -- appending each item's
        // own encode() result -- is the canonical stream that local dispatch
        // routes on, rather than concatenating rendered text and re-tokenizing
        // it: the buffer's tokens are defined to be the piecewise encoding,
        // and only the piecewise encoding is what block hashes are computed
        // over turn by turn.
        let a = "<|user|>hel";
        let b = "lo world";

        let mut piecewise = tokenizer.encode(a);
        piecewise.extend(tokenizer.encode(b));

        let whole = tokenizer.encode(&format!("{a}{b}"));

        assert_ne!(
            piecewise, whole,
            "expected the split and whole encodings to diverge at the boundary"
        );
    }
}
