// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process identity is part of the cross-process lease boundary.

use roundhouse_server::EngineConfig;

#[test]
fn default_engines_mint_distinct_node_identities() {
    let first = EngineConfig::default();
    let second = EngineConfig::default();

    assert_ne!(
        first.node_id, second.node_id,
        "two default server processes must not present themselves as the same lease holder"
    );
}
