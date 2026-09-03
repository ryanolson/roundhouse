// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The recursion plumbing four `..._contract_suite!` macros used to carry
//! separately, once each.
//!
//! [`correlation_maps_contract_suite!`](crate::correlation_maps_contract_suite),
//! [`spend_ledger_contract_suite!`](crate::spend_ledger_contract_suite),
//! [`fair_use_ledger_contract_suite!`](crate::fair_use_ledger_contract_suite)
//! and [`store_contract_suite!`](crate::store_contract_suite) all expand one
//! test module's worth of names into one `#[tokio::test]` per name, gated by
//! one optional `#[ignore]` the whole suite shares. That expansion — "one
//! test per recursion step rather than one repetition over the names, because
//! the attribute group is captured at depth one and `macro_rules` cannot
//! re-expand it inside a second repetition" — is the same twenty-two lines in
//! all four, differing only in which module answers each generated test's
//! call and what the local binding is named. Before this rung each family
//! carried its own copy (M14.1 review, F6); now each carries only its own
//! name, its own module path, its own binding name and its own list of test
//! names, and delegates the rest here.
//!
//! `#[doc(hidden)]` because this is plumbing a family macro reaches for, not
//! a suite an implementation calls directly — the four public macros above
//! are the contract.

/// See the module doc: the recursion the four family `..._contract_suite!`
/// macros delegate to.
///
/// `$binding` becomes the local variable name each generated test binds
/// `$make` to (`maps`, `ledger`, `store`, one per family, kept distinct so a
/// family's contract functions and its suite macro read the same word);
/// `$module` is the fully-qualified path to that family's contract functions.
/// Both are supplied by the family macro's own `@list` arm, which is also
/// where its test-name list still lives — nothing about *which* tests a
/// family runs moves here, only how each one becomes a `#[tokio::test]`.
#[doc(hidden)]
#[macro_export]
macro_rules! __contract_suite {
    // `$module` is matched one segment at a time (`$($module:ident)::+`)
    // rather than with a single `:path` fragment: a captured `path` becomes
    // one opaque token in the expansion, and `$module::$name` after it is a
    // macro-expansion error ("macro expansion ignores `::` and any tokens
    // following") rather than the qualified call it looks like — path
    // fragments compose with what is written around them at *match* time,
    // not at the call site the expansion produces.
    ($binding:ident, $($module:ident)::+, ($(#[$attr:meta])*), $make:expr; $name:ident $(, $rest:ident)* $(,)?) => {
        #[tokio::test]
        $(#[$attr])*
        async fn $name() {
            let $binding = $make;
            $($module)::+::$name(&$binding).await;
        }
        $crate::__contract_suite!($binding, $($module)::+, ($(#[$attr])*), $make; $($rest),*);
    };
    ($binding:ident, $($module:ident)::+, ($(#[$attr:meta])*), $make:expr; ) => {};
}
