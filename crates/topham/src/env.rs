// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The operator's environment, as a value — and the two directories resolved
//! out of it.
//!
//! **Read once, into a map, and never written.** Every refusal this crate owes
//! is about what is set when the launch happens ([`must_be_unset`], the turn
//! key, an ambient `CODEX_HOME`), so the alternative to a captured map is a
//! scatter of `std::env::var` calls that a test can only exercise by mutating
//! the test process. That is the discipline `claude_e2e.rs` has to observe with
//! `unsafe` and `--test-threads=1`, and it is worth avoiding here: with the
//! environment as a parameter, the refusal tests are pure functions over a
//! `BTreeMap` and run in parallel with everything else.
//!
//! [`must_be_unset`]: roundhouse_server::ClaudeLaunch::must_be_unset
//!
//! # XDG, in two lines and no crate
//!
//! `XDG_CONFIG_HOME` if it names something, else `$HOME/.config` — and the same
//! shape for the data directory. This is the rule NeMo Relay follows, and
//! agreeing with it matters more than any refinement a directories crate would
//! add: an operator running both tools should not have to learn that their two
//! launchers disagree about where a profile lives.
//!
//! A value that is not an **absolute path** — empty or merely relative — is
//! treated as unset, which is the XDG specification's own rule and applies to
//! `HOME` here as well. The failure it avoids is specific: a login shell that
//! exports `XDG_CONFIG_HOME=` or `XDG_CONFIG_HOME=config` would otherwise put a
//! deployment's profiles, and a codex profile's generated `auth.json` with
//! them, in whatever directory the operator happened to be standing in when
//! they ran `topham` — a different directory on the next run, so the profile
//! that vanished is indistinguishable from one that was never saved.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The environment a launch is resolved against.
///
/// `BTreeMap` rather than `HashMap` so that anything rendered from it — the
/// plan's variable list, a refusal naming several suppressors — comes out in
/// one order on every machine. A launcher whose dry-run output reordered
/// between runs would be a launcher nobody could diff.
pub type EnvMap = BTreeMap<String, String>;

/// This process's environment, captured.
///
/// Non-UTF-8 names and values are dropped rather than lossily converted. Both
/// are vanishingly rare on the platforms this runs on, and the alternative is
/// worse than dropping: a `to_string_lossy` on a variable *name* would build a
/// map keyed by a name no shell can set, so a refusal could fire against a
/// variable that does not exist and a real one could be missed.
pub fn system() -> EnvMap {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// Neither the XDG variable nor `HOME` names a directory, so there is nowhere
/// to look.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "cannot resolve the {what} directory: neither `{xdg}` nor `HOME` is set. Every profile this \
     launcher reads lives under one of those, so there is no default to fall back to -- set \
     `{xdg}` to the directory the profiles are in"
)]
pub struct NoHome {
    /// `config` or `data`, so the message names which of the two failed.
    pub what: &'static str,
    /// The XDG variable that would have answered it.
    pub xdg: &'static str,
}

/// Where profiles live: `XDG_CONFIG_HOME`, else `$HOME/.config`.
pub fn config_home(env: &EnvMap) -> Result<PathBuf, NoHome> {
    resolve(env, "XDG_CONFIG_HOME", ".config", "config")
}

/// Where per-profile scratch lives — the generated `CODEX_HOME` above all:
/// `XDG_DATA_HOME`, else `$HOME/.local/share`.
///
/// Separate from [`config_home`] because the two have different lifetimes. A
/// profile is written by hand and belongs in a backup; a generated
/// `config.toml` is derived from it and is rewritten on every launch, so
/// keeping the second out of the operator's configuration directory is what
/// stops a dotfile repository from accumulating files nobody edits.
pub fn data_home(env: &EnvMap) -> Result<PathBuf, NoHome> {
    resolve(env, "XDG_DATA_HOME", ".local/share", "data")
}

fn resolve(
    env: &EnvMap,
    xdg: &'static str,
    fallback: &str,
    what: &'static str,
) -> Result<PathBuf, NoHome> {
    let absolute = |name: &str| {
        env.get(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(value) = absolute(xdg) {
        return Ok(value);
    }
    match absolute("HOME") {
        Some(home) => Ok(home.join(fallback)),
        None => Err(NoHome { what, xdg }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn the_xdg_variable_wins_over_home() {
        let env = env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/op")]);
        assert_eq!(config_home(&env).unwrap(), PathBuf::from("/xdg"));
    }

    #[test]
    fn home_is_the_fallback_for_both_directories() {
        let env = env(&[("HOME", "/home/op")]);
        assert_eq!(
            config_home(&env).unwrap(),
            PathBuf::from("/home/op/.config")
        );
        assert_eq!(
            data_home(&env).unwrap(),
            PathBuf::from("/home/op/.local/share")
        );
    }

    /// An exported-but-empty `XDG_CONFIG_HOME` is unset, not the current
    /// directory — see the module doc. Without this the fallback below would
    /// never run and profiles would land wherever `topham` was invoked from.
    #[test]
    fn an_empty_xdg_variable_is_unset() {
        let env = env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/op")]);
        assert_eq!(
            config_home(&env).unwrap(),
            PathBuf::from("/home/op/.config")
        );
    }

    #[test]
    fn nothing_to_resolve_is_an_error_that_names_the_variable() {
        let error = config_home(&EnvMap::new()).unwrap_err();
        assert_eq!(error.xdg, "XDG_CONFIG_HOME");
        assert!(error.to_string().contains("XDG_CONFIG_HOME"), "{error}");
        assert_eq!(data_home(&EnvMap::new()).unwrap_err().xdg, "XDG_DATA_HOME");
    }

    /// F7: the XDG spec (cited in this module's own doc comment) says a
    /// relative value for an XDG variable must be treated as if it were
    /// unset. `resolve` returned a non-empty value verbatim regardless of
    /// whether it was absolute, so a relative `XDG_CONFIG_HOME` scattered
    /// profiles -- and, through `codex_home`, a generated `auth.json` --
    /// under whatever directory `topham` happened to be invoked from.
    ///
    /// One case per variable, because each is read by a different resolver
    /// and a fix applied to one of them would leave the others silent.
    #[test]
    fn a_relative_xdg_variable_falls_back_to_home() {
        let config = env(&[("XDG_CONFIG_HOME", "relative/config"), ("HOME", "/home/op")]);
        assert_eq!(
            config_home(&config).unwrap(),
            PathBuf::from("/home/op/.config")
        );

        let data = env(&[("XDG_DATA_HOME", "relative/data"), ("HOME", "/home/op")]);
        assert_eq!(
            data_home(&data).unwrap(),
            PathBuf::from("/home/op/.local/share")
        );
    }

    /// F7's other half: `HOME` is the fallback, so a relative one is a
    /// fallback to nowhere. Joining it would put the same files under the
    /// invocation directory by the longer route, so it is the error that
    /// names the variable an operator can set instead.
    #[test]
    fn a_relative_home_is_no_home_at_all() {
        let env = env(&[("HOME", "op")]);
        assert_eq!(
            config_home(&env).unwrap_err(),
            NoHome {
                what: "config",
                xdg: "XDG_CONFIG_HOME"
            }
        );
        assert_eq!(
            data_home(&env).unwrap_err(),
            NoHome {
                what: "data",
                xdg: "XDG_DATA_HOME"
            }
        );
    }
}
