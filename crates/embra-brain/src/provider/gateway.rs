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
//!
//! The routing is specific to this gateway, so a URL is stored only once
//! the listener behind it has identified itself ([`identify`]): every
//! response of a WSDS Local AI Gateway proxy listener carries
//! [`GATEWAY_HEADER`] with the gateway's version, and `/local-ai-gateway
//! <url>` and `on` read it from `GET {url}/v1/models` before a change is
//! saved.
//!
//! Higgsfield's image API is no provider of [`ProviderKind`], and it goes
//! through the gateway too, under [`HIGGSFIELD_PREFIX`]
//! ([`higgsfield_base`]). That route came with the gateway's 0.2.0
//! ([`HIGGSFIELD_SINCE`]): an older gateway knows no such prefix and would
//! hand the request, with its key, to its default backend. So a call of
//! Higgsfield's asks the gateway who it is first, every time, and sends
//! nothing to one that does not say 0.2.0 or later
//! (`provider::image::higgsfield`).

use crate::config::{ProviderGateway, SystemConfig};
use crate::provider::ProviderKind;

/// Every response of a WSDS Local AI Gateway proxy listener carries this
/// header with the gateway's version (its `listener::GATEWAY_HEADER`).
pub(crate) const GATEWAY_HEADER: &str = "x-local-ai-gateway";

/// The whole of the identification request: connect, TLS and the answer.
const IDENTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The gateway's forced-route prefix of Higgsfield's API. Its model ids
/// begin with a vendor's name (`xai/…`), so the gateway routes nothing to
/// it by model, and the prefix is the only way in.
pub(crate) const HIGGSFIELD_PREFIX: &str = "higgsfield";

/// The first version of the gateway that has that route: major, minor,
/// patch.
pub(crate) const HIGGSFIELD_SINCE: (u64, u64, u64) = (0, 2, 0);

/// What a refused certificate means here, and what fixes it. First the
/// `/ca` command, on the running machine: the brain trusts an imported
/// certificate at once, and the command is answered before a session, so
/// the wizard's question can be answered again right after it. Then the two
/// routes from outside the OS: the STATE drop-in on a raw image, and the
/// build's own folder.
const CA_HINT: &str = " The listener's certificate is not trusted. /ca import <url | uploads/<file>> reads \
     the gateway's CA certificate and imports it by its SHA-256 (on the gateway's host, local-ai-gateway \
     tls ca-cert prints the certificate and tls show its SHA-256); the brain trusts it at once. From outside \
     the OS the file goes into /embra/state/ca-certificates/ with seed-state.sh --ca-dir, or into \
     Imported_CAs/ before the image is built, and is trusted from the next boot.";

/// What an answer that is not HTTP to a plain `http://` request means: a
/// TLS listener answers a plaintext request with an alert, which hyper
/// reports as an invalid HTTP version.
const TLS_HINT: &str = " The listener did not speak plain HTTP: a gateway listener with tls on is \
     reached as https://host:port.";

/// The unified listener's default port, given to a bare host and to an
/// `http://` URL without one, as the presets' `normalize_endpoint` gives
/// theirs.
pub(crate) const UNIFIED_PORT: &str = "4000";

/// The gateway's own id for a provider, which is its forced-route path
/// prefix. `ProviderKind::as_str` says `lm_studio`; the gateway's id has no
/// underscore. `None` for a native model: it runs on this machine, and
/// nothing of it is routed anywhere.
pub(crate) fn prefix(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Anthropic => Some("anthropic"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::Ollama => Some("ollama"),
        ProviderKind::LmStudio => Some("lmstudio"),
        ProviderKind::Native => None,
    }
}

/// The gateway while one is stored and on.
pub(crate) fn in_force(gateway: Option<&ProviderGateway>) -> Option<&ProviderGateway> {
    gateway.filter(|g| g.enabled && !g.url.trim().is_empty())
}

/// The base every request of `kind` is built on: `"{url}/{prefix}"` while a
/// gateway is stored and on, `direct` otherwise (the provider's own host, or
/// the preset's stored endpoint).
pub(crate) fn base_for(
    gateway: Option<&ProviderGateway>,
    kind: ProviderKind,
    direct: &str,
) -> String {
    match (in_force(gateway), prefix(kind)) {
        (Some(g), Some(prefix)) => under(g, prefix),
        _ => direct.to_string(),
    }
}

/// `"{url}/{prefix}"` of a gateway.
fn under(gateway: &ProviderGateway, prefix: &str) -> String {
    format!("{}/{}", gateway.url.trim().trim_end_matches('/'), prefix)
}

/// The base of Higgsfield's API through a gateway in force:
/// `"{url}/higgsfield"`.
pub(crate) fn higgsfield_base(gateway: &ProviderGateway) -> String {
    under(gateway, HIGGSFIELD_PREFIX)
}

/// Whether a gateway that says `version` has the route of Higgsfield's API:
/// [`HIGGSFIELD_SINCE`] or later. A version this cannot read (no
/// `major.minor` of digits at its start, a `v` before them allowed) is
/// none that has it.
pub(crate) fn routes_higgsfield(version: &str) -> bool {
    let mut parts = version.trim().trim_start_matches(['v', 'V']).split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    });
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor, parts.next().flatten().unwrap_or(0)) >= HIGGSFIELD_SINCE,
        _ => false,
    }
}

/// What a call of Higgsfield's is told when the gateway in force is from
/// before the route.
pub(crate) fn no_higgsfield_route(url: &str, version: &str) -> String {
    let (major, minor, patch) = HIGGSFIELD_SINCE;
    format!(
        "the provider gateway at {url} says it is WSDS Local AI Gateway {version}, and the route of Higgsfield's API \
         came with {major}.{minor}.{patch}: an older gateway would hand the request and its key to another backend, \
         so nothing was sent. Update the gateway, or turn it off for this machine with /local-ai-gateway off"
    )
}

/// `host[:port]` of a base URL, for the probe's endpoint label:
/// `api.anthropic.com` direct, `gateway.example.com:4000` through the
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
/// OS and never inside it. No network call is made here; [`identify`] is
/// the call that follows, and the health probe after the change.
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

/// The URL `/local-ai-gateway` asks to identify itself before the action is
/// applied: the new URL of `Set`, the stored one of `On`. None for `Show`,
/// `Off` and `Reset`, which reach no listener, and none for `On` without a
/// stored gateway, which [`apply`] refuses.
pub(crate) fn url_to_identify(cfg: &SystemConfig, action: &GatewayAction) -> Option<String> {
    match action {
        GatewayAction::Set(url) => Some(url.clone()),
        GatewayAction::On => cfg.provider_gateway.as_ref().map(|g| g.url.clone()),
        GatewayAction::Show | GatewayAction::Off | GatewayAction::Reset => None,
    }
}

/// What follows the cause of a request that got no answer. A certificate
/// refused on its dates gets the clock's hint (`net::certificate_date_hint`,
/// passed in): the date check comes before the issuer check, so that cause
/// says nothing about the CA and the drop-in would fix nothing. Any other
/// certificate refusal names the drop-in directory; a TLS listener given as
/// `http://` names the scheme.
fn identify_hint(url: &str, cause: &str, date_hint: Option<String>) -> String {
    let lower = cause.to_ascii_lowercase();
    if let Some(hint) = date_hint {
        hint
    } else if lower.contains("certificate") {
        CA_HINT.to_string()
    } else if url.starts_with("http://") && lower.contains("invalid http version") {
        TLS_HINT.to_string()
    } else {
        String::new()
    }
}

/// Asks the listener at `url` to identify itself: `GET {url}/v1/models`
/// with no provider header (the gateway's own merged model list, which it
/// does not record), and the answer's [`GATEWAY_HEADER`], which every
/// response of a WSDS Local AI Gateway proxy listener carries, whatever
/// the status. The version in it is returned. A listener that does not
/// answer, or answers without the header (another gateway, a plain model
/// server, a gateway from before the header), is refused with the bare
/// reason — a sentence the command wraps with [`refusal`] and the wizard
/// shows as it is; a certificate the brain does not trust names the
/// drop-in directory, and one refused on its dates names the clock. The
/// caller saves nothing on an `Err`.
pub(crate) async fn identify(url: &str) -> Result<String, String> {
    identify_within(url, IDENTIFY_TIMEOUT).await
}

/// [`identify`] within `timeout`: a call that has a deadline of its own
/// asks inside it.
pub(crate) async fn identify_within(url: &str, timeout: std::time::Duration) -> Result<String, String> {
    let target = format!("{}/v1/models", url.trim_end_matches('/'));
    let client = crate::net::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client.get(&target).send().await.map_err(|e| {
        let cause = crate::net::error_chain(&e);
        let hint = identify_hint(url, &cause, crate::net::certificate_date_hint(&cause));
        format!("nothing answered at {target}: {cause}.{hint}")
    })?;
    let version = resp
        .headers()
        .get(GATEWAY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    version.ok_or_else(|| {
        format!(
            "{url} answered (HTTP {}) without the {GATEWAY_HEADER} header: it is not WSDS Local AI \
             Gateway's unified listener, or a build from before the header.",
            resp.status().as_u16()
        )
    })
}

/// `/local-ai-gateway`'s answer to a refused identification: the bare
/// reason of [`identify`] as the command's own error.
pub(crate) fn refusal(reason: &str) -> String {
    format!("/local-ai-gateway: {reason} The configuration is unchanged.")
}

/// What an identified listener is, for the wizard's Info: the version, the
/// URL, the prefixes, the plain-HTTP note.
pub(crate) fn identified_line(url: &str, version: &str) -> String {
    format!(
        "WSDS Local AI Gateway {version} at {url}. Every provider request goes through it: \
         {PREFIX_LIST}.{}",
        plain_http_note(url)
    )
}

/// The presets set up through the gateway with no direct endpoint, named
/// in the answers that turn the gateway off or forget it, and in the status
/// line: each answers only while the gateway is on. `way_back` is the
/// command that brings the gateway back, when there is one.
fn stranded_note(cfg: &SystemConfig, way_back: &str) -> String {
    cfg.openai_compat
        .gateway_only_presets()
        .into_iter()
        .map(|preset| {
            format!(
                " {} has no direct endpoint: it answers only while the gateway is on; \
                 /provider --setup {} stores one{way_back}.",
                preset.label(),
                preset.as_kind().as_str()
            )
        })
        .collect()
}

const WAY_BACK_ON: &str = ", or /local-ai-gateway on brings the gateway back";
const WAY_BACK_URL: &str = ", or /local-ai-gateway <url> stores a gateway again";

const PREFIX_LIST: &str = "Anthropic at /anthropic, Gemini at /gemini, Ollama at /ollama, LM Studio at /lmstudio, \
     and Higgsfield's image API at /higgsfield (a gateway from 0.2.0 on)";

/// What `Set` and `On` add when the listener that identified itself is from
/// before Higgsfield's route and Higgsfield is the image provider: its
/// calls are refused while this gateway is on.
fn higgsfield_note(cfg: &SystemConfig, version: Option<&str>) -> String {
    let selected = cfg.image_provider.as_deref() == Some(crate::provider::image::ImageProviderKind::Higgsfield.as_str());
    match version {
        Some(version) if selected && !routes_higgsfield(version) => format!(
            " This gateway is {version}, from before Higgsfield's route: image_generate, whose provider here is \
             Higgsfield, is refused while it is on."
        ),
        _ => String::new(),
    }
}

fn plain_http_note(url: &str) -> &'static str {
    if url.starts_with("http://") {
        " Plain HTTP: the provider key travels in clear to this host."
    } else {
        ""
    }
}

/// What the listener said it was, for the answer to `Set` and `On`.
fn identified(version: Option<&str>) -> String {
    version
        .map(|v| format!(" (WSDS Local AI Gateway {v})"))
        .unwrap_or_default()
}

/// Apply `/local-ai-gateway`'s action to the configuration and say what it did.
/// `version` is what [`identify`] learned for `Set` and `On`, named in the
/// answer. `Show` changes nothing and answers with [`status_line`]. An `Err`
/// names what could not be done and leaves the configuration as it was.
pub(crate) fn apply(
    cfg: &mut SystemConfig,
    action: &GatewayAction,
    version: Option<&str>,
) -> Result<String, String> {
    match action {
        GatewayAction::Show => Ok(status_line(cfg)),
        GatewayAction::Set(url) => {
            cfg.provider_gateway = Some(ProviderGateway {
                url: url.clone(),
                enabled: true,
            });
            Ok(format!(
                "Provider gateway set and on: {url}{}. Every provider request goes through it \
                 from your next message: {PREFIX_LIST}. The health probe runs through it now; \
                 /status shows the result.{}{}",
                identified(version),
                plain_http_note(url),
                higgsfield_note(cfg, version)
            ))
        }
        GatewayAction::On => match cfg.provider_gateway.as_mut() {
            Some(g) => {
                g.enabled = true;
                let (url, note) = (g.url.clone(), higgsfield_note(cfg, version));
                Ok(format!(
                    "Provider gateway on: {url}{}. Every provider request goes through it from your \
                     next message; the health probe runs through it now.{}{note}",
                    identified(version),
                    plain_http_note(&url)
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
                let url = g.url.clone();
                Ok(format!(
                    "Provider gateway off; {url} is kept. Requests go to each provider's own host \
                     from your next message (Ollama and LM Studio to their stored endpoint). \
                     /local-ai-gateway on routes them through it again.{}",
                    stranded_note(cfg, WAY_BACK_ON)
                ))
            }
            None => Err("No provider gateway is configured.".to_string()),
        },
        GatewayAction::Reset => match cfg.provider_gateway.take() {
            Some(_) => Ok(format!(
                "Provider gateway forgotten. Requests go to each provider's own host from your \
                 next message.{}",
                stranded_note(cfg, WAY_BACK_URL)
            )),
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
             used again when it is off. /local-ai-gateway off keeps the URL; /local-ai-gateway reset forgets it.{}{}",
            g.url,
            plain_http_note(&g.url),
            stranded_note(cfg, "")
        ),
        Some(g) => format!(
            "Provider gateway: off — {} kept. Requests go to each provider's own host (Ollama and \
             LM Studio to their stored endpoint). /local-ai-gateway on routes them through it again.{}",
            g.url,
            stranded_note(cfg, WAY_BACK_ON)
        ),
        None => "Provider gateway: not configured. /local-ai-gateway <url> stores WSDS Local AI Gateway's \
                 unified listener (http://host:4000) and turns it on; every provider request then \
                 goes through it."
            .to_string(),
    }
}

/// The system-status rows the web console's `local-ai-gateway` pill is
/// rendered from (`GetSystemStatus.services`, proxied by apid's `/status`):
/// `local-ai-gateway` is `on` or `off` and `.detail` says what that means,
/// with the URL; nothing while no gateway is stored.
pub(crate) fn status_rows(cfg: &SystemConfig) -> Vec<(String, String)> {
    let Some(g) = cfg.provider_gateway.as_ref() else {
        return Vec::new();
    };
    let (state, detail) = if g.enabled {
        ("on", format!("every provider request goes through {}", g.url))
    } else {
        ("off", format!("off; {} kept", g.url))
    };
    vec![
        ("local-ai-gateway".to_string(), state.to_string()),
        ("local-ai-gateway.detail".to_string(), detail),
    ]
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg() -> SystemConfig {
        crate::config::tests_support::minimal_cfg()
    }

    /// Higgsfield's image API has a route of its own on the gateway, which
    /// came with the gateway's 0.2.0. Its base is the gateway's URL and the
    /// prefix; a version from 0.2.0 on has the route, and one this cannot
    /// read has not. `Set` and `On` name the route among the others, and
    /// say so when the gateway that answered is from before it and
    /// Higgsfield is the image provider of this machine.
    #[test]
    fn higgsfields_route_is_a_prefix_of_its_own_and_came_with_a_later_gateway() {
        assert_eq!(higgsfield_base(&gw("https://gw.example:4000/", true)), "https://gw.example:4000/higgsfield");
        assert_eq!(higgsfield_base(&gw(" http://10.0.0.5:4000 ", true)), "http://10.0.0.5:4000/higgsfield");
        for (version, routes) in [
            ("0.2.0", true),
            ("0.2", true),
            ("0.2.7", true),
            ("0.10.0", true),
            ("1.0.0", true),
            ("v0.2.0", true),
            (" 0.2.0-dev ", true),
            ("0.2.0+build.5", true),
            ("0.1.0", false),
            ("0.1.99", false),
            ("0.1", false),
            ("0", false),
            ("", false),
            ("dev", false),
            ("two", false),
            (".2.0", false),
        ] {
            assert_eq!(routes_higgsfield(version), routes, "{version:?}");
        }
        assert!(in_force(Some(&gw("http://gw:4000", true))).is_some());
        assert!(in_force(Some(&gw("http://gw:4000", false))).is_none() && in_force(Some(&gw(" ", true))).is_none() && in_force(None).is_none());

        let mut c = cfg();
        let msg = apply(&mut c, &parse_action("gw.example").unwrap(), Some("0.2.0")).unwrap();
        assert!(msg.contains("LM Studio at /lmstudio, and Higgsfield's image API at /higgsfield (a gateway from 0.2.0 on)."), "{msg}");
        assert!(!msg.contains("from before Higgsfield's route"), "{msg}");
        // An older gateway, and Gemini the image provider (the default): nothing to say.
        let msg = apply(&mut c, &GatewayAction::On, Some("0.1.0")).unwrap();
        assert!(!msg.contains("from before Higgsfield's route"), "{msg}");
        // An older gateway, and Higgsfield the image provider.
        c.image_provider = Some("higgsfield".into());
        let note = " This gateway is 0.1.0, from before Higgsfield's route: image_generate, whose provider here is Higgsfield, is refused while it is on.";
        assert!(apply(&mut c, &GatewayAction::On, Some("0.1.0")).unwrap().ends_with(note));
        assert!(apply(&mut c, &parse_action("gw.example").unwrap(), Some("0.1.0")).unwrap().ends_with(note));
        assert!(!apply(&mut c, &GatewayAction::On, Some("0.2.0")).unwrap().contains("from before"));
        assert!(!apply(&mut c, &GatewayAction::On, None).unwrap().contains("from before"), "a version that was not asked is no verdict");
        assert_eq!(
            no_higgsfield_route("http://gw:4000", "0.1.0"),
            "the provider gateway at http://gw:4000 says it is WSDS Local AI Gateway 0.1.0, and the route of Higgsfield's API came with 0.2.0: an older gateway would hand the request and its key to another backend, so nothing was sent. Update the gateway, or turn it off for this machine with /local-ai-gateway off"
        );
        assert!(status_line(&c).contains("Higgsfield's image API at /higgsfield (a gateway from 0.2.0 on)"));
    }

    #[test]
    fn a_url_is_normalized_saved_and_turned_on() {
        let mut c = cfg();
        let msg = apply(&mut c, &parse_action(" gateway.example.com ").unwrap(), Some("0.1.0")).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://gateway.example.com:4000", true)));
        assert!(msg.contains("/anthropic") && msg.contains("/lmstudio"), "{msg}");
        assert!(msg.contains("http://gateway.example.com:4000 (WSDS Local AI Gateway 0.1.0)"), "{msg}");
        assert!(msg.contains("Plain HTTP"), "{msg}");
        let msg = apply(&mut c, &parse_action("https://gw.example").unwrap(), None).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("https://gw.example", true)));
        assert!(!msg.contains("Plain HTTP") && !msg.contains("WSDS Local AI Gateway"), "{msg}");
    }

    #[test]
    fn off_keeps_the_url_and_on_brings_it_back() {
        let mut c = cfg();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into()), None).unwrap();
        let msg = apply(&mut c, &parse_action("OFF").unwrap(), None).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://gw:4000", false)));
        assert!(msg.contains("kept"), "{msg}");
        assert!(status_line(&c).starts_with("Provider gateway: off"));
        assert_eq!(provider_line_suffix(&c), " Gateway: off (http://gw:4000 kept).");
        let msg = apply(&mut c, &parse_action("on").unwrap(), Some("0.2.0")).unwrap();
        assert_eq!(c.provider_gateway, Some(gw("http://gw:4000", true)));
        assert!(msg.contains("http://gw:4000 (WSDS Local AI Gateway 0.2.0)"), "{msg}");
        assert!(status_line(&c).starts_with("Provider gateway: on"));
        assert_eq!(provider_line_suffix(&c), " Gateway: on (http://gw:4000).");
    }

    #[test]
    fn set_and_on_name_the_url_to_identify_and_the_rest_none() {
        let mut c = cfg();
        assert_eq!(
            url_to_identify(&c, &GatewayAction::Set("http://gw:4000".into())),
            Some("http://gw:4000".to_string())
        );
        assert_eq!(url_to_identify(&c, &GatewayAction::On), None, "nothing stored: apply refuses");
        c.provider_gateway = Some(gw("http://gw:4000", false));
        assert_eq!(url_to_identify(&c, &GatewayAction::On), Some("http://gw:4000".to_string()));
        for action in [GatewayAction::Show, GatewayAction::Off, GatewayAction::Reset] {
            assert_eq!(url_to_identify(&c, &action), None, "{action:?}");
        }
    }

    /// The gateway's answer: the header on any status, read from the merged
    /// model list, asked with no provider header so the gateway generates
    /// the list itself. A base with a path composes the same way.
    #[tokio::test]
    async fn a_listener_with_the_header_gives_its_version_whatever_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(GATEWAY_HEADER, " 0.1.0 ")
                    .set_body_json(serde_json::json!({"object": "list", "data": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ai/v1/models"))
            .respond_with(ResponseTemplate::new(502).insert_header(GATEWAY_HEADER, "0.2.0"))
            .mount(&server)
            .await;

        assert_eq!(identify(&server.uri()).await.unwrap(), "0.1.0");
        assert_eq!(identify(&format!("{}/ai/", server.uri())).await.unwrap(), "0.2.0");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for req in &requests {
            for header in ["anthropic-version", "x-goog-api-key", "openai-organization", "authorization", "x-api-key"] {
                assert!(!req.headers.contains_key(header), "{header} was sent");
            }
        }
    }

    /// What is refused, with the reason: an answer without the header (the
    /// status named), an empty header, and no answer at all. Nothing is
    /// saved on either; the handler returns before `apply`.
    #[tokio::test]
    async fn a_listener_without_the_header_or_without_an_answer_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"models\":[]}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/empty/v1/models"))
            .respond_with(ResponseTemplate::new(200).insert_header(GATEWAY_HEADER, ""))
            .mount(&server)
            .await;

        let err = identify(&server.uri()).await.unwrap_err();
        assert!(err.starts_with(&server.uri()), "the bare reason, no command prefix: {err}");
        assert!(err.contains("(HTTP 200) without the x-local-ai-gateway header"), "{err}");
        assert!(!err.contains("/local-ai-gateway") && !err.contains("unchanged"), "{err}");
        let wrapped = refusal(&err);
        assert!(wrapped.starts_with("/local-ai-gateway: ") && wrapped.contains(&err), "{wrapped}");
        assert!(wrapped.ends_with("The configuration is unchanged."), "{wrapped}");
        let err = identify(&format!("{}/empty", server.uri())).await.unwrap_err();
        assert!(err.contains("without the x-local-ai-gateway header"), "{err}");

        // A port nothing listens on: bound, read, released.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let err = identify(&format!("http://{closed}")).await.unwrap_err();
        assert!(err.starts_with(&format!("nothing answered at http://{closed}/v1/models: ")), "{err}");
        assert!(err.ends_with('.'), "{err}");
        assert!(!err.contains("ca-certificates") && !err.contains("/ca import") && !err.contains("https://"), "no hint on a refused connection: {err}");
    }

    /// A preset set up through the gateway with no direct endpoint is named
    /// by the answers that turn the gateway off or forget it, and by the
    /// status line, with the way back; one with both fields stored is not.
    #[test]
    fn off_reset_and_the_status_line_name_a_preset_without_a_direct_endpoint() {
        let mut c = cfg();
        c.openai_compat.ollama_model = "qwen3.8:27b".into();
        c.openai_compat.lm_studio_endpoint = "http://mac:1234".into();
        c.openai_compat.lm_studio_model = "m".into();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into()), Some("0.1.0")).unwrap();
        let on = status_line(&c);
        assert!(on.contains("Ollama has no direct endpoint: it answers only while the gateway is on; /provider --setup ollama stores one."), "{on}");
        assert!(!on.contains("LM Studio has no direct endpoint"), "{on}");
        let off = apply(&mut c, &GatewayAction::Off, None).unwrap();
        for text in [&off, &status_line(&c)] {
            assert!(text.contains("Ollama has no direct endpoint"), "{text}");
            assert!(text.contains("/provider --setup ollama stores one, or /local-ai-gateway on brings the gateway back."), "{text}");
        }
        let reset = apply(&mut c, &GatewayAction::Reset, None).unwrap();
        assert!(reset.contains("Ollama has no direct endpoint"), "{reset}");
        assert!(reset.contains(", or /local-ai-gateway <url> stores a gateway again."), "{reset}");
        c.openai_compat.ollama_endpoint = "http://mac:11434".into();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into()), None).unwrap();
        assert!(!apply(&mut c, &GatewayAction::Off, None).unwrap().contains("no direct endpoint"));
    }

    /// A certificate refused on its dates gets the clock's hint and nothing
    /// about the CA; any other certificate refusal names the CA directory.
    #[test]
    fn a_date_refusal_takes_the_clock_hint_before_the_ca_directory() {
        let dated = "client error (Connect): invalid peer certificate: certificate not valid yet: verification time 1514765221 (UNIX), but certificate is not valid before 1793750400 (278985179 seconds in future)";
        assert_eq!(identify_hint("https://gw:4000", dated, Some(" The clock.".into())), " The clock.");
        assert_eq!(identify_hint("https://gw:4000", "client error (Connect): invalid peer certificate: UnknownIssuer", None), CA_HINT);
        assert_eq!(identify_hint("http://gw:4000", "client error (SendRequest): invalid HTTP version parsed", None), TLS_HINT);
        assert_eq!(identify_hint("https://gw:4000", "client error (SendRequest): invalid HTTP version parsed", None), "", "the scheme hint is for http:// alone");
        assert_eq!(identify_hint("http://gw:4000", "client error (Connect): tcp connect error: Connection refused (os error 111)", None), "");
        // The hint the brain makes of the bare-metal line names no CA directory.
        let hint = crate::net::certificate_date_hint(dated).expect("a date refusal");
        assert!(hint.contains("2018-01-01 00:07:01 UTC") && hint.contains("2026-11-04 00:00:00 UTC"), "{hint}");
        let for_dates = identify_hint("https://gw:4000", dated, Some(hint));
        assert!(!for_dates.contains("ca-certificates") && !for_dates.contains("/ca import"), "{for_dates}");
        // The certificate hint names the command of the running machine
        // first, with where the owner shows the fingerprint, and then the
        // two routes from outside the OS.
        let at = |needle: &str| CA_HINT.find(needle).unwrap_or_else(|| panic!("the hint lacks {needle}: {CA_HINT}"));
        assert!(CA_HINT.starts_with(" The listener's certificate is not trusted."));
        assert!(at("/ca import <url | uploads/<file>>") < at("/embra/state/ca-certificates/"));
        assert!(at("local-ai-gateway tls ca-cert") < at("tls show its SHA-256"));
        assert!(at("the brain trusts it at once") < at("seed-state.sh --ca-dir"));
        assert!(at("Imported_CAs/") < at("is trusted from the next boot"));
    }

    /// A TLS listener given as `http://` answers the plaintext request with
    /// an alert record; the refusal says to use `https://`. (Live on
    /// 2026-10-25: the gateway's `tls = "on"` listener on 4000 reads
    /// `client error (SendRequest): invalid HTTP version parsed`.)
    #[tokio::test]
    async fn a_tls_listener_given_as_http_is_refused_with_the_https_hint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            // TLS alert, fatal, protocol_version: what rustls sends to a
            // ClientHello it cannot read.
            tokio::io::AsyncWriteExt::write_all(&mut tcp, &[0x15, 0x03, 0x01, 0x00, 0x02, 0x02, 0x46]).await.ok();
        });
        let err = identify(&format!("http://{addr}")).await.unwrap_err();
        assert!(err.contains("nothing answered at"), "{err}");
        assert!(err.contains("reached as https://host:port"), "{err}");
    }

    #[test]
    fn the_status_rows_say_on_or_off_with_the_url_and_nothing_when_unset() {
        let mut c = cfg();
        assert!(status_rows(&c).is_empty());
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into()), None).unwrap();
        assert_eq!(
            status_rows(&c),
            vec![
                ("local-ai-gateway".to_string(), "on".to_string()),
                (
                    "local-ai-gateway.detail".to_string(),
                    "every provider request goes through http://gw:4000".to_string()
                ),
            ]
        );
        apply(&mut c, &GatewayAction::Off, None).unwrap();
        assert_eq!(
            status_rows(&c),
            vec![
                ("local-ai-gateway".to_string(), "off".to_string()),
                ("local-ai-gateway.detail".to_string(), "off; http://gw:4000 kept".to_string()),
            ]
        );
    }

    #[test]
    fn reset_forgets_the_gateway() {
        let mut c = cfg();
        apply(&mut c, &GatewayAction::Set("http://gw:4000".into()), None).unwrap();
        apply(&mut c, &parse_action("reset").unwrap(), None).unwrap();
        assert_eq!(c.provider_gateway, None);
        assert!(status_line(&c).starts_with("Provider gateway: not configured"));
        assert_eq!(provider_line_suffix(&c), "");
        assert!(apply(&mut c, &GatewayAction::Reset, None).is_err());
    }

    #[test]
    fn on_without_a_url_is_refused_and_a_bad_url_saves_nothing() {
        let mut c = cfg();
        assert!(apply(&mut c, &GatewayAction::On, None).unwrap_err().contains("/local-ai-gateway <url>"));
        assert!(apply(&mut c, &GatewayAction::Off, None).is_err());
        let err = parse_action("localhost").unwrap_err();
        assert!(err.starts_with("/local-ai-gateway: ") && err.contains("beside the OS"), "{err}");
        assert_eq!(c.provider_gateway, None);
        assert_eq!(parse_action("").unwrap(), GatewayAction::Show);
        assert_eq!(apply(&mut c, &GatewayAction::Show, None).unwrap(), status_line(&c));
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
            normalize_url("gateway.example.com").unwrap(),
            "http://gateway.example.com:4000"
        );
        assert_eq!(normalize_url("http://10.0.2.2").unwrap(), "http://10.0.2.2:4000");
        assert_eq!(normalize_url(" [fd00::5] ").unwrap(), "http://[fd00::5]:4000");
    }

    #[test]
    fn a_scheme_a_port_and_a_path_are_kept_and_a_trailing_slash_is_dropped() {
        assert_eq!(
            normalize_url("http://gateway.example.com:4000/").unwrap(),
            "http://gateway.example.com:4000"
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
        assert_eq!(prefix(ProviderKind::Anthropic), Some("anthropic"));
        assert_eq!(prefix(ProviderKind::Gemini), Some("gemini"));
        assert_eq!(prefix(ProviderKind::Ollama), Some("ollama"));
        assert_eq!(prefix(ProviderKind::LmStudio), Some("lmstudio"));
        assert_eq!(prefix(ProviderKind::Native), None);
        // A native model keeps its loopback base whatever the gateway is.
        let on = ProviderGateway { url: "http://gw:4000".into(), enabled: true };
        let base = embra_boot::native_model::BASE_URL;
        assert_eq!(base_for(Some(&on), ProviderKind::Native, base), base);
        assert_eq!(base_for(None, ProviderKind::Native, base), base);
        assert_eq!(base_for(Some(&on), ProviderKind::LmStudio, "http://studio:1234"), "http://gw:4000/lmstudio");
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
            host_label("http://gateway.example.com:4000/anthropic"),
            "gateway.example.com:4000"
        );
        assert_eq!(host_label("http://[fd00::5]:4000/gemini"), "[fd00::5]:4000");
        assert_eq!(host_label("not a url"), "not a url");
    }
}
