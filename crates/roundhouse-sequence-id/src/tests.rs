// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The crate's suite, one module per concern, over the server's own fixtures
//! read in place: no fixture moves, so no existing `include_str!` changes and
//! the captures the server's suites pin are the ones this crate reads.

mod detection;
mod fixtures;
mod keyed;
mod labels;
mod signals;
