// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! M11.3 review finding F17, from the other side: the launch fixtures have one
//! home, and the suites that use them do not grow a second copy.
//!
//! F17 was reported as a five-file duplication and refuted in part: `TURN_KEY`
//! and `ROOT` were declared four times (launch, relay, plan, tui), not five —
//! `cli/tests.rs` drives a real admin router on an ephemeral loopback port, so
//! it has no fixed root and no turn key at all, and its overlap with the others
//! was only `scratch()`/`env_at()`. What survives is the maintainability
//! complaint, and `src/test_support.rs` is the answer to it.
//!
//! **A source-text check rather than a compile-time one**, because the thing
//! being prevented is a file declaring its *own* copy — which compiles
//! perfectly. It lives outside `src/` so that `include_str!` reads those
//! modules without also swallowing this file's own literals, which would make
//! every assertion below trivially true of itself.
//!
//! `tui/tests.rs` was the named remainder — it carried its own copy of the two
//! constants, its own `nemo-relay` double and its own dry-run report — and the
//! F17 follow-up moved all of it onto `test_support.rs`. It is asserted over
//! like the rest now, so there is no remainder left to name.
//!
//! **The relay double is a fixture too, and the larger one.** Two suites spawn
//! it, and a screen test whose double reported a different upstream from the
//! subcommand's would not fail — it would agree with itself while the two call
//! sites drifted, which is exactly what `relay::dry_run`'s parity test exists
//! to catch. So the report constants and the script that prints them are held
//! to the same one-home rule as the key and the root.

/// Where the fixtures live now.
const TEST_SUPPORT: &str = include_str!("../src/test_support.rs");

/// The suites migrated onto them.
const MIGRATED: &[(&str, &str)] = &[
    ("launch/tests.rs", include_str!("../src/launch/tests.rs")),
    ("relay/tests.rs", include_str!("../src/relay/tests.rs")),
    ("plan/tests.rs", include_str!("../src/plan/tests.rs")),
    ("cli/tests.rs", include_str!("../src/cli/tests.rs")),
    ("tui/tests.rs", include_str!("../src/tui/tests.rs")),
];

/// Every fixture the suites are held to sharing.
///
/// The relay double's own names among them: it is the fixture two suites
/// *spawn*, so a second copy is two shell scripts that must keep answering the
/// same way, and the way they stop is one of them being edited alone.
const FIXTURES: &[&str] = &[
    "const TURN_KEY",
    "const ROOT",
    "fn scratch(",
    "fn env(",
    "const AIMED_HERE",
    "const RE_AIMED",
    "fn dry_run_double(",
    "fn relay_double_on_path(",
];

/// The control the refutation turned on: the fixture literals really are in
/// `test_support.rs`, so the assertions below are discriminating rather than
/// vacuously true of a file that declares nothing anywhere.
#[test]
fn the_shared_module_is_where_the_fixtures_are_declared() {
    for fixture in FIXTURES {
        assert!(
            TEST_SUPPORT.contains(fixture),
            "test_support.rs no longer declares `{fixture}`, so the suites below are checked \
             against a home that has moved"
        );
    }
}

/// The property itself: a suite that used the shared fixture and then grew its
/// own copy is a fixture change that has to be made twice, which is the way the
/// two stop agreeing.
#[test]
fn no_migrated_suite_declares_its_own_copy_of_a_shared_fixture() {
    for (name, source) in MIGRATED {
        for fixture in FIXTURES {
            assert!(
                !source.contains(fixture),
                "{name} declares its own `{fixture}` again; the shared one is in \
                 src/test_support.rs"
            );
        }
    }
}
