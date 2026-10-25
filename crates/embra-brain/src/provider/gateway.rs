//! The provider gateway: WSDS Local AI Gateway's unified listener, in front
//! of every provider while the operator keeps it on (`/gateway`).
//!
//! The gateway forwards each provider's own wire format byte for byte, with
//! the key the client sends, and an explicit path prefix forces the backend:
//! `/anthropic/v1/messages` reaches Anthropic whatever the model is called,
//! and the prefix is stripped before the request goes on. Routing through it
//! is therefore a change of base URL and nothing else. [`base_for`] is the
//! one rule; every provider builds its URLs on the base it is given, and the
//! health probe takes the same base through
//! `grpc_service::provider_probe_target`.

use crate::config::ProviderGateway;
use crate::provider::ProviderKind;

/// The gateway's own id for a provider, which is its forced-route path
/// prefix. `ProviderKind::as_str` says `lm_studio`; the gateway's id has no
/// underscore.
pub(crate) fn prefix(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Ollama => "ollama",
        ProviderKind::LmStudio => "lmstudio",
    }
}

/// The base every request of `kind` is built on: `"{url}/{prefix}"` while a
/// gateway is stored and on, `direct` otherwise (the provider's own host, or
/// the preset's stored endpoint).
pub(crate) fn base_for(
    gateway: Option<&ProviderGateway>,
    kind: ProviderKind,
    direct: &str,
) -> String {
    match gateway {
        Some(g) if g.enabled && !g.url.trim().is_empty() => {
            format!("{}/{}", g.url.trim().trim_end_matches('/'), prefix(kind))
        }
        _ => direct.to_string(),
    }
}

/// `host[:port]` of a base URL, for the probe's endpoint label:
/// `api.anthropic.com` direct, `axiom-app.ops.wsds:4000` through the
/// gateway. A base that does not parse is returned as it is.
pub(crate) fn host_label(base: &str) -> String {
    match reqwest::Url::parse(base) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            (None, _) => base.to_string(),
        },
        Err(_) => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw(url: &str, enabled: bool) -> ProviderGateway {
        ProviderGateway {
            url: url.to_string(),
            enabled,
        }
    }

    #[test]
    fn each_provider_has_the_gateways_own_prefix() {
        assert_eq!(prefix(ProviderKind::Anthropic), "anthropic");
        assert_eq!(prefix(ProviderKind::Gemini), "gemini");
        assert_eq!(prefix(ProviderKind::Ollama), "ollama");
        assert_eq!(prefix(ProviderKind::LmStudio), "lmstudio");
    }

    #[test]
    fn off_or_unset_routes_direct_and_on_routes_through_the_prefix() {
        let direct = "https://api.anthropic.com";
        assert_eq!(base_for(None, ProviderKind::Anthropic, direct), direct);
        let off = gw("http://gw:4000", false);
        assert_eq!(base_for(Some(&off), ProviderKind::Anthropic, direct), direct);
        let on = gw("http://gw:4000/", true);
        assert_eq!(
            base_for(Some(&on), ProviderKind::Anthropic, direct),
            "http://gw:4000/anthropic"
        );
        assert_eq!(
            base_for(Some(&on), ProviderKind::Gemini, "https://generativelanguage.googleapis.com"),
            "http://gw:4000/gemini"
        );
        assert_eq!(
            base_for(Some(&on), ProviderKind::Ollama, "http://mac:11434"),
            "http://gw:4000/ollama"
        );
        assert_eq!(
            base_for(Some(&on), ProviderKind::LmStudio, "http://mac:1234"),
            "http://gw:4000/lmstudio"
        );
        let empty = gw("  ", true);
        assert_eq!(base_for(Some(&empty), ProviderKind::Gemini, direct), direct);
    }

    #[test]
    fn the_host_label_is_the_authority_of_the_base() {
        assert_eq!(host_label("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(
            host_label("http://axiom-app.ops.wsds:4000/anthropic"),
            "axiom-app.ops.wsds:4000"
        );
        assert_eq!(host_label("http://[fd00::5]:4000/gemini"), "[fd00::5]:4000");
        assert_eq!(host_label("not a url"), "not a url");
    }
}
