//! The provider gateway: WSDS Local AI Gateway's unified listener, in front
//! of every provider while the operator keeps it on (`/local-ai-gateway`).
//!
//! The gateway forwards each provider's own wire format byte for byte, with
//! the key the client sends, and an explicit path prefix forces the backend:
//! `/anthropic/v1/messages` reaches Anthropic whatever the model is called,
//! and the prefix is stripped before the request goes on. Routing through it
//! is therefore a change of base URL and nothing else. [`base_for`] is the
//! one rule; every provider builds its URLs on the base it is given, and the
//! health probe takes the same base through
//! `grpc_service::provider_probe_target`.

use crate::config::{ProviderGateway, SystemConfig};
use crate::provider::ProviderKind;

/// The unified listener's default port, given to a bare host and to an
/// `http://` URL without one, as the presets' `normalize_endpoint` gives
/// theirs.
pub(crate) const UNIFIED_PORT: &str = "4000";

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

/// The operator's input as the URL that is stored. The scheme defaults to
/// `http://`; a bare host or an `http://` URL without a port gets
/// [`UNIFIED_PORT`] (never `https://`, whose port is implicit); a trailing
/// `/` is dropped; a path is kept, so a gateway behind another proxy
/// composes as `"{url}/{prefix}"` too. Refused, with the reason: a scheme
/// other than http or https, a query or a fragment, credentials in the URL,
/// and a loopback or unspecified host, because the gateway runs beside the
/// OS and never inside it. No network call is made here; the health probe
/// that follows a change reports whether the gateway answers.
pub(crate) fn normalize_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("no URL given".to_string());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    let (scheme, rest) = with_scheme
        .split_once("://")
        .expect("a scheme separator was ensured above");
    if scheme != "http" && scheme != "https" {
        return Err(format!("the scheme must be http or https, not {scheme}"));
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() {
        return Err("no host in the URL".to_string());
    }
    // An IPv6 literal is bracketed; only a ':' after the bracket is a port.
    let has_port = if authority.starts_with('[') {
        authority
            .rfind(']')
            .is_some_and(|i| authority[i..].contains(':'))
    } else {
        authority.contains(':')
    };
    let authority = if has_port || scheme == "https" {
        authority.to_string()
    } else {
        format!("{authority}:{UNIFIED_PORT}")
    };
    let candidate = format!("{scheme}://{authority}{tail}");
    let url = reqwest::Url::parse(&candidate).map_err(|e| format!("not a URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "credentials in the URL are refused: the gateway's listener takes none".to_string(),
        );
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(
            "a query or a fragment is refused: the gateway is reached at a base URL".to_string(),
        );
    }
    let host = url.host_str().ok_or_else(|| "no host in the URL".to_string())?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let reaches_nothing = bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    if reaches_nothing {
        return Err(format!(
            "{host} reaches nothing: the gateway runs beside the OS, never inside it \
             (from QEMU the host machine is 10.0.2.2)"
        ));
    }
    Ok(format!("{scheme}://{authority}{}", tail.trim_end_matches('/')))
}

/// What `/local-ai-gateway` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Show,
    /// A normalized URL to store and turn on.
    Set(String),
    On,
    Off,
    Reset,
}

/// `/local-ai-gateway`'s argument: nothing shows; `on`, `off` and `reset` (in any
/// case) are themselves; anything else is a URL, normalized or refused with
/// the reason.
pub(crate) fn parse_action(args: &str) -> Result<GatewayAction, String> {
    let arg = args.trim();
    if arg.is_empty() {
        return Ok(GatewayAction::Show);
    }
    match arg.to_ascii_lowercase().as_str() {
        "on" => Ok(GatewayAction::On),
        "off" => Ok(GatewayAction::Off),
        "reset" | "clear" => Ok(GatewayAction::Reset),
        _ => normalize_url(arg).map(GatewayAction::Set).map_err(|reason| {
            format!(
                "/local-ai-gateway: {reason}. Give the gateway's unified listener as http://host:{UNIFIED_PORT} \
                 (a bare host gets http:// and :{UNIFIED_PORT}), or on, off, reset."
            )
        }),
    }
}

const PREFIX_LIST: &str =
    "Anthropic at /anthropic, Gemini at /gemini, Ollama at /ollama, LM Studio at /lmstudio";

fn plain_http_note(url: &str) -> &'static str {
    if url.starts_with("http://") {
        " Plain HTTP: the provider key travels in clear to this host."
    } else {
        ""
    }
}

/// Apply `/local-ai-gateway`'s action to the configuration and say what it did.
/// `Show` changes nothing and answers with [`status_line`]. An `Err` names
/// what could not be done and leaves the configuration as it was.
pub(crate) fn apply(cfg: &mut SystemConfig, action: &GatewayAction) -> Result<String, String> {
    match action {
        GatewayAction::Show => Ok(status_line(cfg)),
        GatewayAction::Set(url) => {
            cfg.provider_gateway = Some(ProviderGateway {
                url: url.clone(),
                enabled: true,
            });
            Ok(format!(
                "Provider gateway set and on: {url}. Every provider request goes through it from \
                 your next message: {PREFIX_LIST}. The health probe runs through it now; /status \
                 shows the result.{}",
                plain_http_note(url)
            ))
        }
        GatewayAction::On => match cfg.provider_gateway.as_mut() {
            Some(g) => {
                g.enabled = true;
                Ok(format!(
                    "Provider gateway on: {}. Every provider request goes through it from your \
                     next message; the health probe runs through it now.{}",
                    g.url,
                    plain_http_note(&g.url)
                ))
            }
            None => Err(
                "No provider gateway is stored. /local-ai-gateway <url> stores one and turns it on."
                    .to_string(),
            ),
        },
        GatewayAction::Off => match cfg.provider_gateway.as_mut() {
            Some(g) => {
                g.enabled = false;
                Ok(format!(
                    "Provider gateway off; {} is kept. Requests go to each provider's own host \
                     from your next message (Ollama and LM Studio to their stored endpoint). \
                     /local-ai-gateway on routes them through it again.",
                    g.url
                ))
            }
            None => Err("No provider gateway is configured.".to_string()),
        },
        GatewayAction::Reset => match cfg.provider_gateway.take() {
            Some(_) => Ok(
                "Provider gateway forgotten. Requests go to each provider's own host from your \
                 next message."
                    .to_string(),
            ),
            None => Err("No provider gateway is configured.".to_string()),
        },
    }
}

/// The answer to a bare `/local-ai-gateway`.
pub(crate) fn status_line(cfg: &SystemConfig) -> String {
    match cfg.provider_gateway.as_ref() {
        Some(g) if g.enabled => format!(
            "Provider gateway: on — {}. Every provider request goes through it: {PREFIX_LIST}. \
             For Ollama and LM Studio the gateway's own upstream answers; the stored endpoint is \
             used again when it is off. /local-ai-gateway off keeps the URL; /local-ai-gateway reset forgets it.{}",
            g.url,
            plain_http_note(&g.url)
        ),
        Some(g) => format!(
            "Provider gateway: off — {} kept. Requests go to each provider's own host (Ollama and \
             LM Studio to their stored endpoint). /local-ai-gateway on routes them through it again.",
            g.url
        ),
        None => "Provider gateway: not configured. /local-ai-gateway <url> stores WSDS Local AI Gateway's \
                 unified listener (http://host:4000) and turns it on; every provider request then \
                 goes through it."
            .to_string(),
    }
}

/// What `/provider` adds to its status line: the gateway, when one is
/// stored.
pub(crate) fn provider_line_suffix(cfg: &SystemConfig) -> String {
    match cfg.provider_gateway.as_ref() {
        Some(g) if g.enabled => format!(" Gateway: on ({}).", g.url),
        Some(g) => format!(" Gateway: off ({} kept).", g.url),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SystemConfig {
        crate::config::tests_support::minimal_cfg()
    }

    #[test]
    fn a_url_is_normalized_saved_and_turned_on() {
        let mut c = cfg();
        let msg = apply(&mut c, &parse_action(" axiom-app.ops.wsds ").unwrap()).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://axiom-app.ops.wsds:4000", true)));
        assert!(msg.contains("/anthropic") && msg.contains("/lmstudio"), "{msg}");
        assert!(msg.contains("Plain HTTP"), "{msg}");
        let msg = apply(&mut c, &parse_action("https://gw.example").unwrap()).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("https://gw.example", true)));
        assert!(!msg.contains("Plain HTTP"), "{msg}");
    }

    #[test]
    fn off_keeps_the_url_and_on_brings_it_back() {
        let mut c = cfg();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into())).unwrap();
        let msg = apply(&mut c, &parse_action("OFF").unwrap()).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://gw:4000", false)));
        assert!(msg.contains("kept"), "{msg}");
        assert!(status_line(&c).starts_with("Provider gateway: off"));
        assert_eq!(provider_line_suffix(&c), " Gateway: off (http://gw:4000 kept).");
        apply(&mut c, &parse_action("on").unwrap()).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://gw:4000", true)));
        assert!(status_line(&c).starts_with("Provider gateway: on"));
        assert_eq!(provider_line_suffix(&c), " Gateway: on (http://gw:4000).");
    }

    #[test]
    fn reset_forgets_the_gateway() {
        let mut c = cfg();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into())).unwrap();
        apply(&mut c, &parse_action("reset").unwrap()).unwrap();
        assert_eq!(c.provider_gateway, None);
        assert!(status_line(&c).starts_with("Provider gateway: not configured"));
        assert_eq!(provider_line_suffix(&c), "");
        assert!(apply(&mut c, &GatewayAction::Reset).is_err());
    }

    #[test]
    fn on_without_a_url_is_refused_and_a_bad_url_saves_nothing() {
        let mut c = cfg();
        assert!(apply(&mut c, &GatewayAction::On).unwrap_err().contains("/local-ai-gateway <url>"));
        assert!(apply(&mut c, &GatewayAction::Off).is_err());
        let err = parse_action("localhost").unwrap_err();
        assert!(err.starts_with("/local-ai-gateway: ") && err.contains("beside the OS"), "{err}");
        assert_eq!(c.provider_gateway, None);
        assert_eq!(parse_action("").unwrap(), GatewayAction::Show);
        assert_eq!(apply(&mut c, &GatewayAction::Show).unwrap(), status_line(&c));
    }

    fn gw(url: &str, enabled: bool) -> ProviderGateway {
        ProviderGateway {
            url: url.to_string(),
            enabled,
        }
    }

    #[test]
    fn a_bare_host_gets_http_and_the_unified_port() {
        assert_eq!(
            normalize_url("axiom-app.ops.wsds").unwrap(),
            "http://axiom-app.ops.wsds:4000"
        );
        assert_eq!(normalize_url("http://10.0.2.2").unwrap(), "http://10.0.2.2:4000");
        assert_eq!(normalize_url(" [fd00::5] ").unwrap(), "http://[fd00::5]:4000");
    }

    #[test]
    fn a_scheme_a_port_and_a_path_are_kept_and_a_trailing_slash_is_dropped() {
        assert_eq!(
            normalize_url("http://axiom-app.ops.wsds:4000/").unwrap(),
            "http://axiom-app.ops.wsds:4000"
        );
        assert_eq!(
            normalize_url("http://gw.example:8080/ai/").unwrap(),
            "http://gw.example:8080/ai"
        );
        assert_eq!(
            normalize_url("http://[fd00::5]:4001").unwrap(),
            "http://[fd00::5]:4001"
        );
    }

    #[test]
    fn an_https_url_gets_no_port() {
        assert_eq!(normalize_url("https://gw.example").unwrap(), "https://gw.example");
        assert_eq!(
            normalize_url("https://gw.example:8443/x").unwrap(),
            "https://gw.example:8443/x"
        );
    }

    #[test]
    fn a_loopback_host_a_query_or_userinfo_is_refused_with_the_reason() {
        for input in [
            "localhost:4000",
            "http://127.0.0.1:4000",
            "[::1]:4000",
            "0.0.0.0:4000",
            "LOCALHOST",
        ] {
            let err = normalize_url(input).unwrap_err();
            assert!(err.contains("beside the OS"), "{input}: {err}");
        }
        assert!(normalize_url("http://gw:4000/?x=1").unwrap_err().contains("query"));
        assert!(normalize_url("http://gw:4000/#f").unwrap_err().contains("fragment"));
        assert!(normalize_url("http://u:p@gw:4000").unwrap_err().contains("credentials"));
        assert!(normalize_url("ftp://gw:4000").unwrap_err().contains("http or https"));
        assert!(normalize_url("   ").unwrap_err().contains("no URL"));
        assert!(normalize_url("http://").unwrap_err().contains("no host"));
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
