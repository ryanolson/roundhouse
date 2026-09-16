// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Getting a turn key, over the admin API that already mints them.
//!
//! `topham mint --profile <p> --project <P> --user <U>` posts to
//! `/v1/admin/projects/{P}/members/{U}/keys` with the admin key from
//! [`ADMIN_KEY_ENV`], and prints the export line for the variable the profile
//! names. **It writes nothing.** The minted secret is returned once and never
//! again — that is a property of the admin plane, which stores a hash — so the
//! one thing a launcher must not do is put it somewhere the operator will not
//! notice it later.
//!
//! # Why a subcommand rather than a new route
//!
//! The route exists. What was deferred by name in the README was *who calls
//! it*, and every alternative to a subcommand ends up worse: an admin read that
//! also mints would be a GET with a side effect, and a launcher that minted
//! locally would need the directory's private key material. So this is a client
//! of a route, and the only thing it adds is knowing which variable the key is
//! for — which is exactly what the profile is.
//!
//! # Why the project and the member are arguments
//!
//! A profile names a deployment and an agent (see [`crate::profile`]); a
//! membership is a fact about the control plane's directory. Storing one in a
//! profile would be a second copy of a tenancy edge, and the copy would be
//! wrong the first time somebody moved a member between projects — silently, in
//! a file whose whole job is to be believed.

use std::io::Write;

use roundhouse_server::API_PREFIX;
use serde::Deserialize;

use crate::env::EnvMap;
use crate::profile::Profile;

/// Where the admin key is read from.
///
/// A different variable from the profile's `key-env`, and never the same value:
/// an `rh_admin_…` administers the control plane and is refused on every
/// turn-serving surface, so a launch that presented one would start, connect,
/// and fail on its first turn and every turn after.
pub const ADMIN_KEY_ENV: &str = "ROUNDHOUSE_ADMIN_KEY";

/// Why a mint did not produce a key.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    #[error(
        "no admin key: `{ADMIN_KEY_ENV}` is unset. Minting writes to the control plane, which \
         only an `rh_admin_...` key may do -- a turn key is refused with `wrong_key_kind` rather \
         than narrowed"
    )]
    AdminKeyMissing,
    #[error(
        "`{what}` is `{value}`, which is not one path segment. It goes into the mint URL as one, \
         so a value carrying a separator would address a route nobody serves and read as a 404 \
         about a member who exists"
    )]
    UnusableSegment { what: &'static str, value: String },
    #[error("could not reach {url}")]
    Unreachable {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error(
        "the deployment refused to mint ({status}): {body}. A `401` is a secret this deployment \
         does not know; a `403` is the right secret of the wrong *kind* -- a turn key administers \
         nothing -- or a deployment running with no control plane at all; a `404` is the project \
         or the member not existing; a `409` is a membership the control-plane *file* owns, under \
         which the API declines to mint"
    )]
    Refused { status: u16, body: String },
    #[error(
        "the deployment answered {status} with a body this launcher could not read as a minted \
         key. The secret is returned exactly once, so if that response did carry one it is \
         already gone -- check `GET /v1/admin/keys` for a key that now exists and revoke it \
         before retrying"
    )]
    Unreadable {
        status: u16,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write the minted key's export line")]
    Output(#[from] std::io::Error),
}

/// The half of the admin response this launcher reads.
///
/// Not the whole `MintedKeyDto`: an operator minting a key needs the secret and
/// enough to identify it afterwards, and every other field is a fact about the
/// membership they already named. Extra fields are ignored rather than refused
/// (no `deny_unknown_fields`) because this is a *client* of somebody else's
/// response — the strictness a request body deserves would make a launcher
/// break on the day the admin plane adds a column.
#[derive(Debug, Clone, Deserialize)]
pub struct MintedKey {
    /// The one field in this deployment that is a secret in a response body.
    pub secret: String,
    /// The key's id, which is what a revocation names.
    pub id: String,
    /// The last characters of the secret, as every *other* admin read shows it.
    pub display_tail: String,
}

/// The mint route for one membership on one deployment.
///
/// Built from [`API_PREFIX`] rather than a second `/v1` literal, for the reason
/// `claude_launch::messages_url` derives its own: two literals agree today and
/// part company silently.
pub fn mint_url(deployment_root: &str, project: &str, user: &str) -> Result<String, MintError> {
    check_segment("project", project)?;
    check_segment("user", user)?;
    let root = deployment_root.trim_end_matches('/');
    Ok(format!(
        "{root}{API_PREFIX}/admin/projects/{project}/members/{user}/keys"
    ))
}

/// Refused rather than percent-encoded.
///
/// Encoding would be friendlier and would be a guess: the admin routes match a
/// single path segment, and whether a given deployment's project ids may carry
/// a `/` at all is a question about that directory rather than about this URL.
/// A refusal that names the value is the answer that cannot be wrong.
fn check_segment(what: &'static str, value: &str) -> Result<(), MintError> {
    let usable = !value.is_empty()
        && !value.contains('/')
        && !value.contains('?')
        && !value.contains('#')
        && !value.contains(char::is_whitespace);
    match usable {
        true => Ok(()),
        false => Err(MintError::UnusableSegment {
            what,
            value: value.to_string(),
        }),
    }
}

/// What `topham mint` prints, and the only line in this crate that carries a
/// secret.
///
/// `export` rather than a bare assignment because the variable has to reach a
/// *child* process — the agent — and a shell variable that was never exported
/// is invisible to it. The failure that avoids is the quietest one in the
/// system: an unexported key means no credential, which roundhouse admits and
/// degrades to local-only.
pub fn export_line(key_env: &str, secret: &str) -> String {
    format!("export {key_env}={secret}")
}

/// One POST, with the admin key on `Authorization`.
///
/// A trait so a test can drive [`mint`]'s parsing and refusal handling without
/// a socket — and, more usefully, so the *real* transport is the thing a test
/// can point at a real admin router. See this crate's mint suite: it serves
/// `roundhouse_server::admin_api::admin_router` on a loopback port and mints
/// through [`HttpTransport`], so the route path, the header spelling, the
/// status and the body's field names are all read from the deployment rather
/// than restated here.
pub trait AdminTransport {
    fn post(&self, url: &str, admin_key: &str) -> Result<(u16, String), MintError>;
}

/// The real one: `reqwest`, on a runtime built for this single request.
pub struct HttpTransport;

impl AdminTransport for HttpTransport {
    fn post(&self, url: &str, admin_key: &str) -> Result<(u16, String), MintError> {
        let unreachable = |source: reqwest::Error| MintError::Unreachable {
            url: url.to_string(),
            source: Box::new(source),
        };
        // Current-thread, and built here rather than around `fn main`: mint is
        // the only asynchronous thing this binary does, and every other
        // subcommand ends in an `exec` that would abandon a runtime's worker
        // threads mid-flight.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| MintError::Unreachable {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        runtime.block_on(async {
            let response = reqwest::Client::new()
                .post(url)
                .header("authorization", format!("Bearer {admin_key}"))
                .send()
                .await
                .map_err(unreachable)?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(unreachable)?;
            Ok((status, body))
        })
    }
}

/// Mint a turn key for `project`/`user` on `deployment_root`.
///
/// The response is parsed rather than echoed. `201` with a body this launcher
/// cannot read is its own error and not a refusal — see
/// [`MintError::Unreadable`], which is the only failure here that can leave a
/// key existing that nobody holds.
pub fn mint(
    deployment_root: &str,
    project: &str,
    user: &str,
    admin_key: &str,
    transport: &dyn AdminTransport,
) -> Result<MintedKey, MintError> {
    let url = mint_url(deployment_root, project, user)?;
    let (status, body) = transport.post(&url, admin_key)?;
    if status != 201 {
        return Err(MintError::Refused { status, body });
    }
    serde_json::from_str(&body).map_err(|source| MintError::Unreadable { status, source })
}

/// The whole subcommand: read the admin key, mint, print the two lines.
///
/// **Here rather than in the dispatch** (F19). `cli::run`'s rule is that every
/// arm is one call into another module, so that the screen and the subcommand
/// cannot be two implementations of the same action — and the `mint` arm was
/// the one that broke it, holding the `ADMIN_KEY_ENV` read, the emptiness check
/// and the print order in the dispatch where the only way to drive them was to
/// assemble a whole `Cli`. That contract belongs to the module that knows what
/// a minted key is.
///
/// The transport is a parameter for the reason [`AdminTransport`] exists at
/// all; both real callers pass [`HttpTransport`].
pub fn run(
    env: &EnvMap,
    profile: &Profile,
    project: &str,
    user: &str,
    transport: &dyn AdminTransport,
    out: &mut dyn Write,
) -> Result<(), MintError> {
    // Empty is missing, not "an admin key that happens to be blank": an
    // exported-but-unset variable is what a shell leaves behind when the
    // command that was supposed to fill it failed, and sending it would ask the
    // deployment to answer 401 about a secret nobody has.
    let admin_key = env
        .get(ADMIN_KEY_ENV)
        .filter(|value| !value.trim().is_empty())
        .ok_or(MintError::AdminKeyMissing)?;
    let minted = mint(
        &profile.deployment_root,
        project,
        user,
        admin_key,
        transport,
    )?;
    // The id and tail first, on their own line: the export line is what gets
    // copied, and a comment on the same line would be copied with it into a
    // shell that would then treat `#` as part of the value in a `.env` file.
    writeln!(out, "# minted {} (…{})", minted.id, minted.display_tail)?;
    writeln!(out, "{}", export_line(&profile.key_env, &minted.secret))?;
    Ok(())
}

#[cfg(test)]
mod tests;
