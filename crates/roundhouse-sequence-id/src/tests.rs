// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The crate's suite, one module per concern, over the server's own fixtures
//! read in place: no fixture moves, so no existing `include_str!` changes and
//! the captures the server's suites pin are the ones this crate reads.
//!
//! Nothing here canonicalizes a capture: the crate cannot call the server's
//! `canonicalize`, and a copy of it pinned the copy rather than the product.
//! The tests that need a fixture as items are in
//! `roundhouse-server/tests/sequence_identity_fixtures.rs`.

mod detection;
mod fixtures;
mod keyed;
mod labels;
mod signals;
