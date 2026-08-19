// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turning a Responses SSE body into [`FrontierChunk`]s.
//!
//! Split from [`super`] because it answers a different question. That file is
//! about *authentication and the request*; this one is about a byte stream that
//! arrives in arbitrary pieces and has to become a sequence of durable deltas.
//!
//! **Dispatch is on the payload's own `type`, never on the `event:` line.** The
//! two are meant to agree and a proxy is free to drop the `event:` line
//! entirely — it is optional in the SSE grammar — while the JSON body is what
//! the provider actually documents. Reading the line would make roundhouse's
//! accounting depend on a field intermediaries treat as decoration.
//!
//! **A stream that ends without `response.completed` yields no `Done`, and that
//! is deliberate.** The engine already stands an estimate in for a provider
//! that reported nothing, and marks it estimated
//! (`Engine::estimated_usage` → [`Accounting`]). Synthesizing a zero-token
//! `Done` here would fold as *zero tokens for zero dollars*, which is
//! indistinguishable from a saving — the exact failure
//! [`crate::usage`] exists to prevent.
//!
//! [`Accounting`]: roundhouse_core::event::Accounting

use std::collections::VecDeque;

use serde_json::Value;

use crate::frontier::{FrontierChunk, FrontierError};

/// How much of a single SSE event this will buffer before giving up.
///
/// A bound rather than trust: the body is a remote party's, and an upstream (or
/// something pretending to be one) that never sends the blank line separating
/// events would otherwise grow this buffer until the process died. Generous
/// enough that no real frame comes close — a `response.completed` payload is a
/// few hundred bytes and the largest text delta a provider emits is far below
/// this.
const MAX_EVENT_BYTES: usize = 1 << 20;

/// Assembles SSE events out of arbitrary byte runs and decodes the ones that
/// carry output or accounting.
#[derive(Default)]
pub(super) struct SseDecoder {
    /// Bytes received and not yet consumed by a complete event.
    buffer: String,
    /// Decoded chunks waiting to be yielded, in arrival order. A queue because
    /// one `feed` may complete several events.
    pending: VecDeque<FrontierChunk>,
    /// Set by the frame that ends a response. Nothing after it is read: a
    /// provider that kept talking past `response.completed` has already told us
    /// what the turn cost.
    finished: bool,
}

impl SseDecoder {
    /// Take the next decoded chunk, if one is ready.
    pub(super) fn next_chunk(&mut self) -> Option<FrontierChunk> {
        self.pending.pop_front()
    }

    /// Whether the terminal frame has been seen.
    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    /// Add a run of bytes and decode whatever events it completed.
    ///
    /// Takes `&[u8]` and appends lossily rather than requiring valid UTF-8 per
    /// call: a chunk boundary lands mid-codepoint routinely, and an error there
    /// would fail turns at random on any prompt containing a non-ASCII
    /// character. Buffering the raw bytes and decoding at the event boundary is
    /// the alternative, and it costs a second copy of every delta for a
    /// difference nothing observes — the payload is JSON, whose parser rejects
    /// a truncated escape anyway.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        self.drain()
    }

    /// The body ended. Decode a final event that arrived without its blank
    /// line, which is what a well-behaved server sends before closing.
    pub(super) fn eof(&mut self) -> Result<(), FrontierError> {
        if self.finished {
            return Ok(());
        }
        let tail = std::mem::take(&mut self.buffer);
        self.decode_event(&tail)
    }

    /// Consume every complete event in the buffer.
    fn drain(&mut self) -> Result<(), FrontierError> {
        // `\n\n` is the separator; `\r\n\r\n` ends with it, so splitting on the
        // shorter form handles both and leaves a stray `\r` that `trim` takes.
        while let Some(end) = self.buffer.find("\n\n") {
            let event = self.buffer[..end].to_string();
            self.buffer.drain(..end + 2);
            self.decode_event(&event)?;
            if self.finished {
                self.buffer.clear();
                return Ok(());
            }
        }
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err(FrontierError::Upstream(format!(
                "the upstream sent {} bytes with no event boundary, past the \
                 {MAX_EVENT_BYTES}-byte limit; abandoning the stream rather \
                 than buffering it",
                self.buffer.len()
            )));
        }
        Ok(())
    }

    /// Decode one event block: every `data:` line joined, parsed as JSON,
    /// dispatched on its own `type`.
    fn decode_event(&mut self, event: &str) -> Result<(), FrontierError> {
        let mut data = String::new();
        for line in event.lines() {
            // A comment (`: keep-alive`) and every non-`data` field are skipped
            // rather than refused: the SSE grammar lets a server send both, and
            // a proxy that inserts heartbeats must not fail a turn.
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        let data = data.trim();
        // `[DONE]` is the Chat Completions sentinel. The Responses API does not
        // send it, but a gateway that normalizes between the two may, and
        // treating it as JSON would fail an otherwise complete turn.
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let payload: Value = serde_json::from_str(data).map_err(|source| {
            FrontierError::Upstream(format!("the upstream sent an unparseable event: {source}"))
        })?;
        self.dispatch(&payload)
    }

    fn dispatch(&mut self, payload: &Value) -> Result<(), FrontierError> {
        match payload.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = payload.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.pending
                        .push_back(FrontierChunk::OutputText(delta.to_string()));
                }
                Ok(())
            }
            Some("response.completed") => {
                self.finished = true;
                // A completion with no usage object is not an error: it is an
                // unaccounted call, which the engine marks as estimated. What
                // is *not* done here is inventing zeros — see the module doc.
                if let Some(usage) = payload.pointer("/response/usage") {
                    self.pending.push_back(usage_chunk(usage));
                }
                Ok(())
            }
            // The upstream's own terminal failures. Reported as an upstream
            // error rather than a short stream, because a turn that ended
            // because the provider refused it must not look to the engine like
            // a turn that simply produced little.
            Some("response.failed") => {
                self.finished = true;
                Err(FrontierError::Upstream(format!(
                    "the upstream failed the response: {}",
                    error_message(payload.pointer("/response/error"))
                )))
            }
            Some("error") => {
                self.finished = true;
                Err(FrontierError::Upstream(format!(
                    "the upstream sent an error frame: {}",
                    error_message(Some(payload))
                )))
            }
            // Everything else on the Responses wire — `response.created`,
            // item lifecycle, reasoning summaries — is a frame this client has
            // no use for. Skipped rather than refused: a provider adding a
            // frame type must not break a deployment that already works.
            _ => Ok(()),
        }
    }
}

/// The accounting frame, read out of a `usage` object.
///
/// Missing fields read as zero, which is right *here* and wrong in general: this
/// function only runs when the provider sent a `usage` object, so an absent
/// `reasoning_tokens` means a model that does not reason rather than an
/// unaccounted call. The unaccounted case never reaches this function at all.
fn usage_chunk(usage: &Value) -> FrontierChunk {
    let count = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    FrontierChunk::Done {
        input_tokens: count(usage.get("input_tokens")),
        cached_input_tokens: count(usage.pointer("/input_tokens_details/cached_tokens")),
        output_tokens: count(usage.get("output_tokens")),
        reasoning_tokens: count(usage.pointer("/output_tokens_details/reasoning_tokens")),
    }
}

/// What an upstream error object says, or a stand-in naming the absence.
///
/// The caller redacts this before it reaches anyone — see
/// [`ForwardedCredential::redact`](roundhouse_core::control::ForwardedCredential::redact)
/// — so it may quote the upstream freely.
fn error_message(error: Option<&Value>) -> String {
    error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "the upstream named no reason".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the decoder over `pieces` and collect what it yields.
    fn decode(pieces: &[&str]) -> Result<Vec<FrontierChunk>, FrontierError> {
        let mut decoder = SseDecoder::default();
        let mut chunks = Vec::new();
        for piece in pieces {
            decoder.feed(piece.as_bytes())?;
            while let Some(chunk) = decoder.next_chunk() {
                chunks.push(chunk);
            }
        }
        decoder.eof()?;
        while let Some(chunk) = decoder.next_chunk() {
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    const COMPLETED: &str = concat!(
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":120,"input_tokens_details":{"cached_tokens":100},"#,
        r#""output_tokens":30,"output_tokens_details":{"reasoning_tokens":12}}}}"#,
        "\n\n"
    );

    #[test]
    fn a_responses_stream_becomes_deltas_and_one_accounting_frame() {
        let chunks = decode(&[
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            COMPLETED,
        ])
        .unwrap();

        assert_eq!(
            chunks,
            vec![
                FrontierChunk::OutputText("Hel".into()),
                FrontierChunk::OutputText("lo".into()),
                FrontierChunk::Done {
                    input_tokens: 120,
                    cached_input_tokens: 100,
                    output_tokens: 30,
                    reasoning_tokens: 12,
                },
            ],
            "the cached count is the quantity the whole system exists to \
             maximize, and it only arrives if this reads the field"
        );
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        // The ordinary case on a real socket, and the one a naive
        // parse-per-read gets wrong: a chunk boundary anywhere, including
        // mid-JSON and mid-separator.
        let split = decode(&[
            "event: response.outp",
            "ut_text.delta\ndata: {\"type\":\"response.output_te",
            "xt.delta\",\"delta\":\"ok\"}\n",
            "\n",
            &COMPLETED[..40],
            &COMPLETED[40..],
        ])
        .unwrap();
        assert_eq!(split[0], FrontierChunk::OutputText("ok".into()));
        assert!(matches!(split[1], FrontierChunk::Done { .. }));

        // A multi-byte character straddling a read is text, not a failure.
        let text = "日本語";
        let frame =
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n");
        let bytes = frame.as_bytes();
        let mut decoder = SseDecoder::default();
        decoder.feed(&bytes[..40]).unwrap();
        decoder.feed(&bytes[40..]).unwrap();
        assert_eq!(
            decoder.next_chunk(),
            Some(FrontierChunk::OutputText(text.into()))
        );
    }

    #[test]
    fn a_stream_that_never_completes_yields_no_accounting_frame() {
        // The engine estimates for a provider that reported nothing and marks
        // the estimate. A synthesized zero-token `Done` here would fold as zero
        // tokens for zero dollars, which reads as a saving.
        let chunks = decode(&[
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half an answ\"}\n\n",
        ])
        .unwrap();
        assert_eq!(
            chunks,
            vec![FrontierChunk::OutputText("half an answ".into())]
        );

        // And a completion with no usage object is the same case, not a zeroed
        // one.
        let unaccounted =
            decode(&["data: {\"type\":\"response.completed\",\"response\":{}}\n\n"]).unwrap();
        assert!(unaccounted.is_empty());
    }

    #[test]
    fn an_upstream_failure_is_an_error_and_not_a_short_stream() {
        // PROBE: both terminal failure shapes the Responses wire uses.
        for frame in [
            r#"data: {"type":"response.failed","response":{"error":{"message":"quota exceeded"}}}"#,
            r#"data: {"type":"error","message":"quota exceeded"}"#,
        ] {
            let error = decode(&[&format!("{frame}\n\n")]).expect_err("must be an error");
            assert!(
                matches!(&error, FrontierError::Upstream(message) if message.contains("quota exceeded")),
                "{error}"
            );
        }

        // CONTROL: an unparseable body is also an error rather than a silently
        // empty turn -- and it names what happened, because "the upstream sent
        // something we could not read" and "the model said nothing" are
        // different operational problems.
        let garbage = decode(&["data: not json at all\n\n"]).expect_err("must be an error");
        assert!(garbage.to_string().contains("unparseable"), "{garbage}");

        // CONTROL: a keep-alive comment and an unknown frame type are skipped,
        // so the strictness above is about failures and not about novelty.
        assert!(
            decode(&[": keep-alive\n\ndata: {\"type\":\"response.in_progress\"}\n\n"])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_upstream_that_never_ends_an_event_is_abandoned_rather_than_buffered() {
        // The body is a remote party's. Without the bound this grows until the
        // process dies, which is a denial of service delivered by a provider.
        let mut decoder = SseDecoder::default();
        let filler = "x".repeat(64 * 1024);
        let error = loop {
            match decoder.feed(filler.as_bytes()) {
                Ok(()) => continue,
                Err(error) => break error,
            }
        };
        assert!(error.to_string().contains("no event boundary"), "{error}");

        // CONTROL: a payload comfortably under the bound is decoded, so the
        // limit is about an unterminated stream and not about size.
        let big = "y".repeat(4096);
        let chunks = decode(&[&format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{big}\"}}\n\n"
        )])
        .unwrap();
        assert_eq!(chunks, vec![FrontierChunk::OutputText(big)]);
    }
}
