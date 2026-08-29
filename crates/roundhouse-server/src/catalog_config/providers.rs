// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Providers as data: where a catalog entry's traffic actually goes.
//!
//! Until M10.1 "which upstream" was one environment variable naming one
//! transport, and every catalog entry in the process went to it whatever its
//! `provider` said. That is exactly wrong for the thing this phase is for —
//! a session whose capable tier is a model on OpenRouter and whose fallback is
//! the same OpenAI Responses endpoint we already speak, both in one turn's
//! candidate list. Two origins, two keys, two connection pools, one process.
//!
//! So a provider becomes a row in the catalog file: a base URL, the paths its
//! four dialects live at, the variable its key is expected to arrive in, and
//! any static headers it wants. The registry in `main` builds one client per
//! definition; [`crate::catalog_config::CatalogConfig::validate`] is what makes
//! that registry total, by refusing at load a catalog entry naming a provider
//! nothing defines.
//!
//! **What is deliberately not here: the key itself.** [`ProviderAuth::env`]
//! names a variable and this module never reads it. A turn's credential
//! travels on the [`FrontierQuote`] and is resolved per turn from the three
//! tiers the control plane already has (deployment, project, member), because
//! a client is a connection pool and a connection pool is a bad place to keep
//! a secret — `openai_responses.rs` states that argument at the field it
//! applies to. Writing the variable down here is still worth doing: it is what
//! lets `main` tell an operator at boot that the provider they configured has
//! no key anywhere, instead of letting them find out one turn at a time.
//!
//! [`FrontierQuote`]: roundhouse_fleet::FrontierQuote

use std::collections::BTreeMap;

use serde::Deserialize;

use roundhouse_fleet::WireProtocol;
use roundhouse_fleet::anthropic_messages::StoredAuthStyle;

use super::CatalogError;

/// The one provider name a deployment gets without writing a definition.
///
/// It is what `ROUNDHOUSE_FRONTIER_UPSTREAM=openai_responses` plus
/// `ROUNDHOUSE_OPENAI_API_BASE` have always meant, and naming it here is what
/// keeps that wiring working unchanged: a catalog whose entries all say
/// `"provider": "openai"` needs no `"providers"` section at all, and every
/// deployment written before this milestone is such a catalog.
pub const BUILT_IN_OPENAI: &str = "openai";

/// One external provider, as a deployment writes it down.
///
/// `deny_unknown_fields` for the reason the control-plane shapes carry it: every
/// optional axis below *widens* when it is absent — a `route` lost to a typo is
/// a dialect this provider silently cannot serve, and `extra_header` for
/// `extra_headers` is a request that goes out unlabelled. None of those is
/// visible from any read surface afterwards.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The origin every route below is joined onto, e.g.
    /// `https://openrouter.ai/api/v1`. A trailing slash is trimmed when the
    /// client is built, so `{base}{route}` is one slash rather than two — some
    /// gateways route on the exact path.
    pub base_url: String,
    /// Where each dialect lives under [`Self::base_url`].
    ///
    /// All four optional, because a provider that speaks one dialect is
    /// ordinary and a definition that had to state paths it does not serve
    /// would be inviting an operator to invent them. What makes the absence
    /// safe is the cross-check: a catalog entry whose `wire_protocol` names a
    /// dialect this provider left blank is a boot refusal, not a dispatch-time
    /// surprise.
    #[serde(default)]
    pub routes: ProviderRoutes,
    /// Which environment variable this provider's key is expected to arrive
    /// in. Never read as a secret here — see the module doc.
    pub auth: ProviderAuth,
    /// Static headers every request to this provider carries.
    ///
    /// For the identification headers a gateway asks for — OpenRouter's
    /// `HTTP-Referer` and `X-OpenRouter-Title` are the shipped case. Not a
    /// place for credentials: these are written in a file that is not the
    /// credential file, and the transport applies them *under* the
    /// authorization header so a definition cannot overwrite the one thing
    /// that decides whose money a turn spends.
    ///
    /// `BTreeMap` rather than `HashMap` so the order a client sends them in is
    /// the order the file lists them alphabetically — a request that differs
    /// run to run is a request nobody can diff against a captured one.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

/// The path each dialect is served at, relative to
/// [`ProviderConfig::base_url`].
///
/// Four fields and not one, because a provider may serve some and not others —
/// OpenRouter serves all four at `/models`, `/chat/completions`, `/responses`
/// and `/messages`; a Dynamo frontend serves only `/chat/completions`; a
/// `switchyard-server` serves whichever its own config declares.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRoutes {
    /// The catalog-discovery route. Read by no runtime path — the import tool
    /// (M10.1's P6) is its consumer — and carried here so a deployment states
    /// its provider once rather than once per tool.
    pub models: Option<String>,
    pub chat_completions: Option<String>,
    pub responses: Option<String>,
    pub messages: Option<String>,
}

impl ProviderRoutes {
    /// The path a request in `dialect` goes to, if this provider serves it.
    ///
    /// **No catch-all arm, deliberately.** `WireProtocol` is the enum a fourth
    /// dialect gets added to, and `usage.rs` already relies on exhaustiveness
    /// to make that addition a compile error everywhere it matters. A `_ =>
    /// None` here would instead make the new dialect silently unroutable
    /// through every provider in every catalog.
    pub fn for_dialect(&self, dialect: WireProtocol) -> Option<&str> {
        match dialect {
            WireProtocol::OpenAiChatCompletions => self.chat_completions.as_deref(),
            WireProtocol::OpenAiResponses => self.responses.as_deref(),
            WireProtocol::AnthropicMessages => self.messages.as_deref(),
        }
    }

    /// How the file spells the route a `dialect` needs.
    ///
    /// Exists so a refusal can name the field an operator would go and add,
    /// rather than the dialect they already wrote. Same argument
    /// [`WireProtocol::wire_name`] makes for itself.
    pub fn field_for(dialect: WireProtocol) -> &'static str {
        match dialect {
            WireProtocol::OpenAiChatCompletions => "chat_completions",
            WireProtocol::OpenAiResponses => "responses",
            WireProtocol::AnthropicMessages => "messages",
        }
    }
}

/// Where this provider's key is expected to live, and how it is spelled.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuth {
    /// An environment variable name, e.g. `OPENROUTER_API_KEY`.
    pub env: String,
    /// Which header a stored key goes out in, for a provider serving the
    /// `anthropic_messages` dialect.
    ///
    /// **Read only on that dialect, and optional there.** The two OpenAI wires
    /// have one spelling each and have never had another, so there is nothing
    /// for a file to say about them; `anthropic_messages` is the dialect with
    /// two GA providers that disagree — `api.anthropic.com` authenticates on a
    /// bare `x-api-key` and rejects a bearer, OpenRouter's `/messages` route
    /// does the reverse. Absent, it is `x_api_key`: the first-party convention,
    /// so the definition that needs no line is the common one.
    ///
    /// Held as a `String` and parsed by [`Self::stored_auth_style`] rather than
    /// deserialized straight into the enum, because a typo here must produce a
    /// refusal naming *this field and this provider* — serde's own "unknown
    /// variant" error names neither, and the remedy is a one-word edit an
    /// operator has to be pointed at.
    #[serde(default)]
    pub style: Option<String>,
}

impl ProviderAuth {
    /// The spelling this definition names, or `None` for one nothing
    /// implements.
    ///
    /// `None` is the refusal case, not a fallback — see
    /// [`ProviderConfig::validate`]. A silent default on an unrecognised value
    /// would send a deployment's OpenRouter key out in a header that provider
    /// ignores, and the symptom, a 401 on every turn, names neither the file
    /// nor the field.
    pub fn stored_auth_style(&self) -> Option<StoredAuthStyle> {
        match self.style.as_deref() {
            None => Some(StoredAuthStyle::default()),
            Some(style) => StoredAuthStyle::from_wire_name(style),
        }
    }
}

impl ProviderConfig {
    /// Refuse a definition that cannot mean one thing.
    ///
    /// Everything here is a pure function of the file, which is what keeps it
    /// in this boundary rather than in the registry constructor: whether *this
    /// build* has a transport for a dialect is a fact about the binary, and it
    /// is checked where the binary is composed. Whether the file says something
    /// coherent is checked here, where an operator can be told which line.
    pub(super) fn validate(&self, path: &str, name: &str) -> Result<(), CatalogError> {
        // A base URL with no scheme reaches `reqwest` as a relative URL and
        // fails at the first dispatch of the first turn, per tenant, with a
        // message about a URL rather than about a config file.
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            return Err(CatalogError::ProviderBaseUrl {
                path: path.to_string(),
                provider: name.to_string(),
                base_url: self.base_url.clone(),
            });
        }
        for (field, route) in [
            ("models", &self.routes.models),
            ("chat_completions", &self.routes.chat_completions),
            ("responses", &self.routes.responses),
            ("messages", &self.routes.messages),
        ] {
            let Some(route) = route else { continue };
            // Joined as `{base}{route}`, so a route that forgot its leading
            // slash silently addresses a sibling of the base rather than a
            // child of it — `https://host/api/v1` + `responses` is
            // `https://host/api/v1responses`, which 404s in a way that reads
            // like an outage.
            if !route.starts_with('/') {
                return Err(CatalogError::ProviderRoutePath {
                    path: path.to_string(),
                    provider: name.to_string(),
                    field,
                    route: route.clone(),
                });
            }
        }
        // A variable name that could never be exported is a promise nothing
        // can keep, and the shell would have refused it too.
        let env = &self.auth.env;
        let plausible = !env.is_empty()
            && !env.starts_with(|c: char| c.is_ascii_digit())
            && env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !plausible {
            return Err(CatalogError::ProviderAuthEnv {
                path: path.to_string(),
                provider: name.to_string(),
                env: env.clone(),
            });
        }
        // A spelling no client implements. Refused here rather than defaulted,
        // because every wrong answer to this question is a 401 on every turn of
        // this provider's — and a 401 is the one upstream response an operator
        // reads as "my key is bad" rather than as "my file is wrong".
        if self.auth.stored_auth_style().is_none() {
            return Err(CatalogError::ProviderAuthStyle {
                path: path.to_string(),
                provider: name.to_string(),
                style: self.auth.style.clone().unwrap_or_default(),
                accepted: StoredAuthStyle::ALL
                    .iter()
                    .map(|style| format!("`{}`", style.wire_name()))
                    .collect::<Vec<_>>()
                    .join(" or "),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(json: &str) -> Result<ProviderConfig, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn a_definition_states_an_origin_its_routes_and_where_its_key_lives() {
        let openrouter = provider(
            r#"{
              "base_url": "https://openrouter.ai/api/v1",
              "routes": { "models": "/models", "responses": "/responses" },
              "auth": { "env": "OPENROUTER_API_KEY" },
              "extra_headers": { "X-OpenRouter-Title": "roundhouse" }
            }"#,
        )
        .unwrap();
        openrouter.validate("test", "openrouter").unwrap();

        assert_eq!(
            openrouter.routes.for_dialect(WireProtocol::OpenAiResponses),
            Some("/responses")
        );
        // The dialects this definition did not claim answer `None` rather than
        // a default: a provider that happens to serve `/chat/completions` at
        // the conventional path is still a provider nobody wrote down, and
        // guessing would send a turn somewhere no operator chose.
        assert_eq!(
            openrouter
                .routes
                .for_dialect(WireProtocol::OpenAiChatCompletions),
            None
        );
        assert_eq!(
            openrouter
                .routes
                .for_dialect(WireProtocol::AnthropicMessages),
            None
        );
    }

    #[test]
    fn a_misspelled_field_is_a_refusal_and_not_a_dialect_silently_dropped() {
        // PROBE: `response` for `responses`. Without `deny_unknown_fields` this
        // parses as a provider that serves nothing, and the catalog entry that
        // names it fails at its first dispatch rather than at load.
        assert!(
            provider(
                r#"{ "base_url": "https://x.test", "routes": { "response": "/responses" },
                     "auth": { "env": "K" } }"#
            )
            .is_err()
        );
        // CONTROL: the same document spelled right, so the refusal above is
        // about the typo and not about the shape.
        assert!(
            provider(
                r#"{ "base_url": "https://x.test", "routes": { "responses": "/responses" },
                     "auth": { "env": "K" } }"#
            )
            .is_ok()
        );
    }

    #[test]
    fn the_three_shapes_that_would_fail_one_turn_at_a_time_fail_at_load_instead() {
        let base = |json: &str| provider(json).unwrap().validate("test", "p").unwrap_err();

        // A scheme-less origin: `reqwest` refuses it per request, forever.
        assert!(matches!(
            base(r#"{ "base_url": "openrouter.ai/api/v1", "auth": { "env": "K" } }"#),
            CatalogError::ProviderBaseUrl { .. }
        ));
        // A route joined without its slash addresses a sibling of the base.
        assert!(matches!(
            base(
                r#"{ "base_url": "https://x.test", "routes": { "responses": "responses" },
                     "auth": { "env": "K" } }"#
            ),
            CatalogError::ProviderRoutePath {
                field: "responses",
                ..
            }
        ));
        // A variable name no shell could export.
        assert!(matches!(
            base(r#"{ "base_url": "https://x.test", "auth": { "env": "not a var" } }"#),
            CatalogError::ProviderAuthEnv { .. }
        ));

        // CONTROL: the minimum that is actually serviceable, so the three above
        // are about the values and not about the fields being required.
        provider(r#"{ "base_url": "https://x.test", "auth": { "env": "K2" } }"#)
            .unwrap()
            .validate("test", "p")
            .unwrap();
    }

    /// **Which header a stored key rides in is a fact about the provider, and
    /// the file is where it is stated.**
    ///
    /// Both GA providers on the `anthropic_messages` dialect are here, because
    /// the pair is the whole reason the field exists: `api.anthropic.com`
    /// authenticates a bare `x-api-key` and answers a bearer with a 401,
    /// OpenRouter's `/messages` route answers an `x-api-key` with "Missing
    /// Authentication header". A client that hardcoded either spelling makes
    /// the other provider unreachable, which is F4.
    #[test]
    fn a_definition_names_the_header_its_stored_key_rides_in() {
        let openrouter = provider(
            r#"{
              "base_url": "https://openrouter.ai/api/v1",
              "routes": { "messages": "/messages" },
              "auth": { "env": "OPENROUTER_API_KEY", "style": "bearer" }
            }"#,
        )
        .unwrap();
        openrouter.validate("test", "openrouter").unwrap();
        assert_eq!(
            openrouter.auth.stored_auth_style(),
            Some(StoredAuthStyle::Bearer)
        );

        // The first-party convention is what a definition gets by saying
        // nothing, so the common case needs no line and the uncommon one is
        // written down where an operator can see it.
        let anthropic = provider(
            r#"{
              "base_url": "https://api.anthropic.com/v1",
              "routes": { "messages": "/messages" },
              "auth": { "env": "ANTHROPIC_API_KEY" }
            }"#,
        )
        .unwrap();
        anthropic.validate("test", "anthropic").unwrap();
        assert_eq!(
            anthropic.auth.stored_auth_style(),
            Some(StoredAuthStyle::XApiKey)
        );
    }

    #[test]
    fn an_auth_style_nothing_implements_is_a_refusal_naming_the_field() {
        // PROBE: the spelling an operator reaches for first, and the one a
        // reader of the HTTP header would write. Neither is what the file
        // spells, and defaulting either to `x_api_key` would send an OpenRouter
        // key out in a header that provider ignores -- a 401 on every turn,
        // which reads as a bad key rather than as a wrong file.
        for wrong in ["Bearer", "x-api-key", ""] {
            let definition = provider(&format!(
                r#"{{ "base_url": "https://x.test", "auth": {{ "env": "K", "style": "{wrong}" }} }}"#
            ))
            .unwrap();
            let error = definition
                .validate("test", "openrouter")
                .expect_err(&format!("`{wrong}` must be refused"));
            assert!(
                matches!(&error, CatalogError::ProviderAuthStyle { provider, style, .. }
                    if provider == "openrouter" && style == wrong),
                "{error}"
            );
            // The refusal has to carry the remedy: the field, the value that
            // was written, and what may be written instead.
            let message = error.to_string();
            assert!(message.contains("auth.style"), "{message}");
            assert!(message.contains("`x_api_key`"), "{message}");
            assert!(message.contains("`bearer`"), "{message}");
        }

        // CONTROL: both accepted spellings validate, so the refusals above are
        // about the value and not about the field being unusable.
        for accepted in StoredAuthStyle::ALL {
            provider(&format!(
                r#"{{ "base_url": "https://x.test", "auth": {{ "env": "K", "style": "{}" }} }}"#,
                accepted.wire_name()
            ))
            .unwrap()
            .validate("test", "p")
            .unwrap();
        }
    }
}
