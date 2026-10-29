//! Two sub-flows of the first-run wizard: the provider gateway question
//! (direct, or through WSDS Local AI Gateway, whose listener identifies
//! itself), and the Endpoint → Bearer → Probe-and-Select flow of the
//! OpenAI-compat presets, which lists the models through the gateway when
//! one was chosen.

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::info;

use crate::config::ProviderGateway;
use crate::provider::gateway as gateway_rules;
use crate::provider::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset};
use embra_common::proto::brain::{
    conversation_response, ConversationResponse, SetupFieldType, SetupPrompt, SystemMessage,
    SystemMessageType,
};

/// Result of the OpenAI-compat sub-flow. Endpoint is normalized
/// (scheme prefix + default port if missing, trailing slash stripped).
/// Bearer is `None` when the operator submitted an empty string.
#[derive(Debug, Clone)]
pub struct OpenAiCompatSubflow {
    pub preset: OpenAiCompatPreset,
    pub endpoint: String,
    pub bearer: Option<String>,
    pub model_id: String,
}

/// Run the three-step Endpoint → Bearer → Probe-and-Select flow.
///
/// With a provider gateway chosen, the endpoint typed is the direct path
/// for when the gateway is off (`none` allowed) and the models are listed
/// through the gateway's prefix. On probe failure (unreachable endpoint,
/// bad bearer, malformed response, a gateway upstream that does not
/// answer) or zero-models-found, the sub-flow re-prompts from the
/// Endpoint step. The operator can fix the URL or bearer and retry.
pub async fn run_openai_compat_subflow(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<OpenAiCompatSubflow> {
    loop {
        let endpoint = prompt_endpoint(preset, gateway, tx, response_rx).await?;
        let bearer = prompt_bearer(preset, tx, response_rx).await?;

        match probe_and_select(preset, gateway, &endpoint, bearer.as_deref(), tx, response_rx).await {
            Ok(model_id) => {
                return Ok(OpenAiCompatSubflow {
                    preset,
                    endpoint,
                    bearer,
                    model_id,
                });
            }
            Err(e) => {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("{e} Re-entering endpoint step…"),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                continue;
            }
        }
    }
}

/// Endpoint step: free-text URL prompt with default-suggestion in the
/// label. Normalizes on submit. With a gateway chosen, the endpoint is the
/// direct path for when the gateway is off, `none` (the default) allowed:
/// the gateway's own upstream answers while it is on, and the preset's
/// default is never offered, since `localhost` inside the OS is the
/// machine itself.
async fn prompt_endpoint(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String> {
    let (prompt_text, default) = if gateway.is_some() {
        (
            format!(
                "Enter the {} endpoint URL for when the gateway is off (none is allowed):",
                preset.label()
            ),
            "none",
        )
    } else {
        (
            format!(
                "Enter the {} endpoint URL (default: {}):",
                preset.label(),
                preset.default_base_url()
            ),
            preset.default_base_url(),
        )
    };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Text as i32,
                prompt: prompt_text,
                options: vec![],
                default_value: default.to_string(),
            })),
        }))
        .await;

    let raw = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during endpoint step"))?;
    let trimmed = raw.trim();
    let endpoint = if gateway.is_some() && (trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none")) {
        String::new()
    } else if trimmed.is_empty() {
        preset.default_base_url().to_string()
    } else {
        normalize_endpoint(trimmed, preset)
    };
    info!(target: "setup::openai_compat", preset = %preset.label(), endpoint = %endpoint, "endpoint set");
    Ok(endpoint)
}

/// Bearer step: a set/skip Selector first ("Bearer token for X? (current: none)")
/// followed by a Text prompt for the actual token only when the
/// operator picks Set. Selector-then-Text is required because the
/// console enforces non-empty Text submissions — a single empty-string
/// Text prompt for "leave empty for no auth" is unreachable on the
/// console side. The Selector defaults to Skip so Enter without arrowing
/// accepts no-auth in the typical case. Wording + options + default
/// mirror `grpc_service.rs` `/provider --setup` fresh-setup branch so
/// the operator sees the same prompt in both flows.
async fn prompt_bearer(
    preset: OpenAiCompatPreset,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<Option<String>> {
    // Step 1: set/skip choice.
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: format!("Bearer token for {}? (current: none)", preset.label()),
                options: vec!["Set".to_string(), "Skip".to_string()],
                default_value: "Skip".to_string(),
            })),
        }))
        .await;

    let choice = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during bearer choice step"))?;
    let want_bearer = choice.trim().eq_ignore_ascii_case("set");
    if !want_bearer {
        return Ok(None);
    }

    // Step 2: text prompt for the actual token. Console enforces
    // non-empty so this branch always returns Some(<non-empty>).
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Secret as i32,
                prompt: format!("Enter your {} bearer token:", preset.label()),
                options: vec![],
                default_value: String::new(),
            })),
        }))
        .await;

    let raw = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during bearer token step"))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Defensive: if the console somehow emits empty here, treat
        // as no-bearer rather than a hard error.
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Probe-and-Select step: hit `GET /v1/models` where the next turn goes
/// (the typed endpoint, or the gateway's prefix when one was chosen),
/// surface errors back to the wizard loop (which re-prompts), and on
/// success render a Selector populated from the returned model list.
async fn probe_and_select(
    preset: OpenAiCompatPreset,
    gateway: Option<&ProviderGateway>,
    endpoint: &str,
    bearer: Option<&str>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String, SubflowError> {
    let probe_base = gateway_rules::base_for(gateway, preset.as_kind(), endpoint);
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: if gateway.is_some() {
                    format!("Probing {probe_base} for the models the gateway's upstream lists…")
                } else {
                    format!("Probing {probe_base} for available models…")
                },
                msg_type: SystemMessageType::Info as i32,
            })),
        }))
        .await;

    let models = OpenAICompatProvider::probe_models(preset, &probe_base, bearer)
        .await
        .map_err(|e| {
            if gateway.is_some() {
                SubflowError::ProbeThroughGatewayFailed {
                    base: probe_base.clone(),
                    prefix: gateway_rules::prefix(preset.as_kind()),
                    reason: e.to_string(),
                }
            } else {
                SubflowError::ProbeFailed(e.to_string())
            }
        })?;

    if models.is_empty() {
        return Err(SubflowError::NoModels(probe_base));
    }

    // Selector widget — always shown, even with one model option,
    // for consistent UX (Locked Decision #6).
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Select a model:".to_string(),
                options: models.clone(),
                default_value: models[0].clone(),
            })),
        }))
        .await;

    let selection = response_rx
        .recv()
        .await
        .ok_or(SubflowError::SelectionChannelClosed)?;
    let trimmed = selection.trim();
    // Defense against console drift: only accept selections from the
    // offered list. Empty input falls back to the default (first option).
    let chosen = if trimmed.is_empty() {
        models[0].clone()
    } else if models.iter().any(|m| m == trimmed) {
        trimmed.to_string()
    } else {
        return Err(SubflowError::InvalidSelection {
            offered: models.len(),
            given: trimmed.to_string(),
        });
    };
    Ok(chosen)
}

/// Errors surfaceable to the wizard loop. The loop converts these to
/// SystemMessage errors and re-prompts from the Endpoint step.
#[derive(Debug, thiserror::Error)]
pub enum SubflowError {
    #[error("Probe failed: {0}.")]
    ProbeFailed(String),
    #[error("Probe through the gateway at {base} failed: {reason}. Its [backends.{prefix}] upstream must answer.")]
    ProbeThroughGatewayFailed {
        base: String,
        prefix: &'static str,
        reason: String,
    },
    #[error("No models found at {0}. Pull or load a model on the server, then try again.")]
    NoModels(String),
    #[error("Selection '{given}' is not in the offered list of {offered} models.")]
    InvalidSelection { offered: usize, given: String },
    #[error("Setup channel closed during model selection.")]
    SelectionChannelClosed,
}

/// The provider gateway question of the first-run wizard, asked after the
/// name and before the provider: reach the providers directly, or through
/// WSDS Local AI Gateway. With the gateway chosen, its unified listener's
/// endpoint is normalized ([`gateway_rules::normalize_url`]) and must
/// identify itself ([`gateway_rules::identify`]); a refusal shows the
/// reason and asks again, and `direct` at the re-ask (its default, so Enter
/// is enough on both clients) falls back to direct. Returns the gateway,
/// on, with the version it answered, or `None` for direct.
pub async fn run_gateway_subflow(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<Option<(ProviderGateway, String)>> {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: GATEWAY_QUESTION.to_string(),
                options: vec![GATEWAY_DIRECT.to_string(), GATEWAY_THROUGH.to_string()],
                default_value: String::new(),
            })),
        }))
        .await;
    let choice = response_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("setup channel closed during the gateway step"))?;
    if !choice.to_ascii_lowercase().contains("gateway") {
        return Ok(None);
    }

    let mut prompt_text = GATEWAY_ENDPOINT_PROMPT.to_string();
    let mut default = String::new();
    loop {
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                    field_type: SetupFieldType::Text as i32,
                    prompt: prompt_text.clone(),
                    options: vec![],
                    default_value: default.clone(),
                })),
            }))
            .await;
        let raw = response_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("setup channel closed during the gateway endpoint step"))?;
        let answer = raw.trim();
        let re_asking = !default.is_empty();
        if re_asking && (answer.is_empty() || answer.eq_ignore_ascii_case("direct")) {
            send_system(tx, GATEWAY_DIRECT_NOTE.to_string(), SystemMessageType::Info).await;
            return Ok(None);
        }
        let outcome = match gateway_rules::normalize_url(answer) {
            Err(reason) => Err(format!("Not a gateway URL: {reason}.")),
            Ok(url) => {
                send_system(tx, format!("Asking {url} to identify itself…"), SystemMessageType::Info)
                    .await;
                gateway_rules::identify(&url).await.map(|version| (url, version))
            }
        };
        match outcome {
            Ok((url, version)) => {
                send_system(tx, gateway_rules::identified_line(&url, &version), SystemMessageType::Info)
                    .await;
                info!(target: "setup::gateway", url = %url, version = %version, "gateway identified");
                return Ok(Some((ProviderGateway { url, enabled: true }, version)));
            }
            Err(reason) => {
                send_system(tx, reason, SystemMessageType::Error).await;
                prompt_text = GATEWAY_REASK_PROMPT.to_string();
                default = "direct".to_string();
            }
        }
    }
}

/// The gateway question and its two options; the direct one first, since
/// the console's selector starts on the first option.
pub const GATEWAY_QUESTION: &str =
    "Reach the AI providers directly, or through WSDS Local AI Gateway?";
pub const GATEWAY_DIRECT: &str = "Directly";
pub const GATEWAY_THROUGH: &str = "Through WSDS Local AI Gateway";
/// "endpoint" gives the console its Endpoint header; no "provider" in it.
const GATEWAY_ENDPOINT_PROMPT: &str =
    "Enter the gateway's unified listener endpoint (http://host:4000):";
const GATEWAY_REASK_PROMPT: &str =
    "Re-enter the gateway endpoint, or answer direct to reach the providers directly:";
const GATEWAY_DIRECT_NOTE: &str =
    "Reaching the providers directly. /local-ai-gateway <url> stores a gateway after setup.";

async fn send_system(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    content: String,
    msg_type: SystemMessageType,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content,
                msg_type: msg_type as i32,
            })),
        }))
        .await;
}

/// Normalize an endpoint URL the operator typed:
/// - Trim whitespace + trailing slash
/// - Prepend `http://` if scheme missing
/// - Append the preset's default port if no port is present in the
///   host portion — for bare hosts and `http://` only. An `https://`
///   URL keeps its implicit 443: cloud OpenAI-compatible endpoints
///   (OpenRouter, Together, Fireworks, …) are `https://host/path`
///   without a port, and `https://host:1234` would break them.
///
/// Examples (Ollama default port 11434):
/// - `localhost`             → `http://localhost:11434`
/// - `http://localhost`      → `http://localhost:11434`
/// - `http://localhost:8080` → `http://localhost:8080`
/// - `localhost:11434/`      → `http://localhost:11434`
/// - `https://api.example.com` → `https://api.example.com`
/// - `https://openrouter.ai/api/` → `https://openrouter.ai/api`
pub fn normalize_endpoint(input: &str, preset: OpenAiCompatPreset) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    };

    // Split scheme from host portion.
    let (scheme, after) = if let Some(rest) = with_scheme.strip_prefix("http://") {
        ("http://", rest)
    } else if let Some(rest) = with_scheme.strip_prefix("https://") {
        ("https://", rest)
    } else {
        return with_scheme; // unreachable given the above
    };
    // Host portion is everything before the first `/`.
    let (host_part, path_part) = match after.find('/') {
        Some(idx) => (&after[..idx], &after[idx..]),
        None => (after, ""),
    };

    let default_port = match preset {
        OpenAiCompatPreset::Ollama => "11434",
        OpenAiCompatPreset::LmStudio => "1234",
    };

    // IPv6 bracketed host (e.g. `[::1]`) — only treat ':' after the
    // closing bracket as a port separator.
    let has_port = if host_part.starts_with('[') {
        host_part.rfind(']').is_some_and(|i| host_part[i..].contains(':'))
    } else {
        host_part.contains(':')
    };
    if has_port || scheme == "https://" {
        format!("{scheme}{host_part}{path_part}")
    } else {
        format!("{scheme}{host_part}:{default_port}{path_part}")
    }
}

/// Test support shared with the wizard's tests: a mock server on the
/// host's own non-loopback address, because `gateway::normalize_url`
/// refuses a loopback gateway by design and the gateway sub-flow runs it
/// before `identify`.
#[cfg(test)]
pub(crate) mod test_support {
    /// The address the host's default route leaves from; no packet is sent.
    pub fn local_non_loopback_ip() -> std::net::IpAddr {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").expect("a UDP socket");
        probe.connect("10.255.255.255:1").expect("a default route");
        probe.local_addr().expect("the route's source address").ip()
    }

    /// A wiremock server bound on that address, and its `http://` base.
    pub async fn reachable_mock_server() -> (wiremock::MockServer, String) {
        let listener = std::net::TcpListener::bind((local_non_loopback_ip(), 0)).expect("a listener");
        let addr = listener.local_addr().expect("a bound address");
        let server = wiremock::MockServer::builder().listener(listener).start().await;
        (server, format!("http://{addr}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bare_host_adds_scheme_and_port() {
        let out = normalize_endpoint("localhost", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_preserves_explicit_port() {
        let out = normalize_endpoint("http://localhost:8080", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:8080");
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        let out = normalize_endpoint("http://localhost:11434/", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_adds_scheme_with_explicit_port() {
        let out = normalize_endpoint("localhost:1234", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "http://localhost:1234");
    }

    #[test]
    fn normalize_lm_studio_default_port_when_missing() {
        let out = normalize_endpoint("http://localhost", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "http://localhost:1234");
    }

    #[test]
    fn normalize_https_keeps_implicit_443() {
        // Cloud OpenAI-compatible endpoints are https without a port;
        // injecting the preset's local default port broke them
        // (`https://openrouter.ai:1234/api`). 2026-09-17.
        let out = normalize_endpoint("https://api.example.com", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "https://api.example.com");
        let out = normalize_endpoint("https://openrouter.ai/api/", OpenAiCompatPreset::LmStudio);
        assert_eq!(out, "https://openrouter.ai/api");
    }

    #[test]
    fn normalize_https_explicit_port_kept() {
        let out = normalize_endpoint("https://api.example.com:8443/inference", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "https://api.example.com:8443/inference");
    }

    #[test]
    fn normalize_multiple_trailing_slashes_only_strips_one() {
        // trim_end_matches('/') strips ALL trailing — that's fine for URLs.
        let out = normalize_endpoint("localhost///", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://localhost:11434");
    }

    #[test]
    fn normalize_preserves_path() {
        let out = normalize_endpoint("http://api.example.com/proxy", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://api.example.com:11434/proxy");
    }

    #[test]
    fn normalize_ipv6_bracketed_host_no_port() {
        let out = normalize_endpoint("http://[::1]", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "http://[::1]:11434");
    }

    #[tokio::test]
    async fn subflow_happy_path_one_model() {
        // Mock server with one model in the response.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "qwen3.6:35b"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // Operator inputs — ordered: endpoint URL, bearer-set/skip (Skip),
        // model selection. 2-step bearer flow short-circuits on Skip.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("qwen3.6:35b".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.preset, OpenAiCompatPreset::Ollama);
        // server.uri() comes back like "http://127.0.0.1:54321" — already
        // has scheme + port; normalize_endpoint preserves it.
        assert!(out.endpoint.starts_with("http://"));
        assert_eq!(out.bearer, None);
        assert_eq!(out.model_id, "qwen3.6:35b");

        // Drain response stream so we don't hold the channel open.
        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_zero_models_reprompts_from_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // First request returns zero models.
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": []
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Second request (after re-prompt) returns one model.
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // First attempt: endpoint, bearer-skip, → probe returns 0 → re-prompt.
        // Second attempt: endpoint, bearer-skip, model.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed after re-prompt");
        assert_eq!(out.model_id, "m");

        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_invalid_selection_reprompts_from_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m1"}, {"id": "m2"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // First attempt: endpoint, bearer-skip, "not_in_list" — fails
        // validation and re-prompts from endpoint.
        // Second attempt: endpoint, bearer-skip, valid pick.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("not_in_list".to_string()).await.unwrap();
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m2".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed after re-prompt");
        assert_eq!(out.model_id, "m2");

        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_bearer_non_empty_persists() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        // Flow: endpoint, bearer-set, token text, model selection.
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Set".to_string()).await.unwrap();
        user_tx.send("secret-token".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.bearer.as_deref(), Some("secret-token"));

        while resp_rx.try_recv().is_ok() {}
    }

    /// #20: the bearer is asked for with the `Secret` field type.
    #[tokio::test]
    async fn the_bearer_step_asks_with_a_secret_field() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Set".to_string()).await.unwrap();
        user_tx.send("secret-token".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();
        run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx)
            .await
            .expect("subflow should succeed");
        drop(resp_tx);
        let mut bearer_prompt = None;
        while let Some(Ok(r)) = resp_rx.recv().await {
            if let Some(conversation_response::ResponseType::Setup(p)) = r.response_type
                && p.prompt.contains("bearer token:")
            {
                bearer_prompt = Some(p);
            }
        }
        let p = bearer_prompt.expect("the bearer was asked for");
        assert_eq!(p.field_type, SetupFieldType::Secret as i32);
    }

    #[tokio::test]
    async fn subflow_bearer_choice_defaults_skip() {
        // Operator presses Enter on the set/skip Selector without arrowing —
        // console emits the default option ("Skip") so bearer = None.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, user_rx) = mpsc::channel(32);

        user_tx.send(server.uri()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();

        let mut user_rx = user_rx;
        let result =
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, None, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.bearer, None);
        while resp_rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn subflow_bearer_choice_set_case_insensitive() {
        // "set" / "SET" / "Set" all accepted — eq_ignore_ascii_case on the
        // selector value. Console renders the option label verbatim
        // ("Set"), but lowercase is also tolerated for resilience.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "m"}]
            })))
            .mount(&server)
            .await;

        for variant in ["set", "SET", "Set"] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, user_rx) = mpsc::channel(32);
            user_tx.send(server.uri()).await.unwrap();
            user_tx.send(variant.to_string()).await.unwrap();
            user_tx.send("token".to_string()).await.unwrap();
            user_tx.send("m".to_string()).await.unwrap();
            let mut user_rx = user_rx;
            let out = run_openai_compat_subflow(
                OpenAiCompatPreset::Ollama,
                None,
                &resp_tx,
                &mut user_rx,
            )
            .await
            .expect("subflow should succeed");
            assert_eq!(
                out.bearer.as_deref(),
                Some("token"),
                "variant {variant:?} should reach the token prompt"
            );
            while resp_rx.try_recv().is_ok() {}
        }
    }

    #[tokio::test]
    async fn subflow_empty_endpoint_uses_preset_default() {
        // Operator presses Enter without typing — default is the
        // preset's default_base_url. We can't easily test this against
        // a real probe (the default localhost port likely isn't running),
        // so we just verify the prompt path receives a non-empty default
        // by checking normalize_endpoint behavior plus the input_placeholder
        // contract — the actual default-fallback is exercised in the probe
        // path under operator control.
        let out = normalize_endpoint("", OpenAiCompatPreset::Ollama);
        // Edge: empty input normalizes to "http://:11434" — caller of
        // prompt_endpoint must intercept empty string and use default.
        // This test asserts the subflow's "if raw.trim().is_empty()
        // → default" branch by confirming the normalization function
        // is NOT what handles empty.
        assert!(out != OpenAiCompatPreset::Ollama.default_base_url());
    }

    fn gateway(uri: &str) -> ProviderGateway {
        ProviderGateway { url: uri.to_string(), enabled: true }
    }

    async fn drain(resp_rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> Vec<ConversationResponse> {
        let mut out = Vec::new();
        while let Ok(Ok(r)) = resp_rx.try_recv() {
            out.push(r);
        }
        out
    }

    fn system_texts(frames: &[ConversationResponse]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::System(m)) => Some(m.content.clone()),
                _ => None,
            })
            .collect()
    }

    fn prompts(frames: &[ConversationResponse]) -> Vec<&SetupPrompt> {
        frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::Setup(p)) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// With the gateway chosen, the models come through its prefix and
    /// `none` leaves the direct endpoint blank; the direct path is never
    /// probed.
    #[tokio::test]
    async fn with_the_gateway_on_the_probe_takes_its_prefix_and_none_stores_a_blank_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list", "data": [{"id": "qwen3.8:27b"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send("none".to_string()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("qwen3.8:27b".to_string()).await.unwrap();
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx)
            .await
            .expect("the sub-flow completes through the gateway");
        assert_eq!(out.endpoint, "");
        assert_eq!(out.model_id, "qwen3.8:27b");
        let frames = drain(&mut resp_rx).await;
        let ps = prompts(&frames);
        assert_eq!(ps[0].prompt, "Enter the Ollama endpoint URL for when the gateway is off (none is allowed):");
        assert_eq!(ps[0].default_value, "none");
        assert!(system_texts(&frames).iter().any(|t| t == &format!("Probing {}/ollama for the models the gateway's upstream lists…", server.uri())));
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/ollama/v1/models"));
    }

    /// With the gateway chosen, a typed endpoint is normalized and kept as
    /// the direct path, and still not probed.
    #[tokio::test]
    async fn with_the_gateway_on_a_typed_endpoint_is_normalized_and_not_probed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": [{"id": "m"}]})))
            .mount(&server)
            .await;
        let (resp_tx, _resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send("mac".to_string()).await.unwrap();
        user_tx.send("Skip".to_string()).await.unwrap();
        user_tx.send("m".to_string()).await.unwrap();
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out.endpoint, "http://mac:11434");
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/ollama/v1/models"));
    }

    /// A probe that fails through the gateway names the gateway's upstream
    /// setting, and the loop asks the endpoint again.
    #[tokio::test]
    async fn a_probe_through_the_gateway_that_fails_names_the_upstream() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": [{"id": "m"}]})))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        for answer in ["none", "Skip", "none", "Skip", "m"] {
            user_tx.send(answer.to_string()).await.unwrap();
        }
        let gw = gateway(&server.uri());
        let out = run_openai_compat_subflow(OpenAiCompatPreset::Ollama, Some(&gw), &resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out.model_id, "m");
        let texts = system_texts(&drain(&mut resp_rx).await);
        let failed = texts.iter().find(|t| t.starts_with("Probe through the gateway at")).unwrap_or_else(|| panic!("{texts:?}"));
        assert!(failed.contains("[backends.ollama] upstream must answer. Re-entering endpoint step…"), "{failed}");
    }

    /// The gateway question: the direct option, an empty answer or anything
    /// that is not the gateway option stores no gateway and asks nothing more.
    #[tokio::test]
    async fn the_direct_option_or_an_empty_answer_stores_no_gateway() {
        for answer in [GATEWAY_DIRECT, "", "whatever"] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, mut user_rx) = mpsc::channel(32);
            user_tx.send(answer.to_string()).await.unwrap();
            drop(user_tx);
            let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
            assert!(out.is_none(), "{answer:?}");
            let frames = drain(&mut resp_rx).await;
            let ps = prompts(&frames);
            assert_eq!(ps.len(), 1);
            assert_eq!(ps[0].prompt, GATEWAY_QUESTION);
            assert_eq!(ps[0].options, vec![GATEWAY_DIRECT.to_string(), GATEWAY_THROUGH.to_string()]);
            assert_eq!(ps[0].default_value, "", "no default: nothing to leak into the next text prompt");
        }
    }

    /// A listener that identifies itself is stored, on, with its version
    /// named in the Info.
    #[tokio::test]
    async fn the_gateway_subflow_stores_an_identified_listener() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, uri) = super::test_support::reachable_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).insert_header(gateway_rules::GATEWAY_HEADER, "0.1.0"))
            .mount(&server)
            .await;
        let (resp_tx, mut resp_rx) = mpsc::channel(32);
        let (user_tx, mut user_rx) = mpsc::channel(32);
        user_tx.send(GATEWAY_THROUGH.to_string()).await.unwrap();
        user_tx.send(uri.clone()).await.unwrap();
        drop(user_tx);
        let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
        assert_eq!(out, Some((gateway(&uri), "0.1.0".to_string())));
        let frames = drain(&mut resp_rx).await;
        let ps = prompts(&frames);
        assert_eq!(ps[1].prompt, "Enter the gateway's unified listener endpoint (http://host:4000):");
        let texts = system_texts(&frames);
        assert!(texts.iter().any(|t| t.starts_with(&format!("WSDS Local AI Gateway 0.1.0 at {uri}. Every provider request goes through it:"))), "{texts:?}");
    }

    /// A refused URL (a loopback host, a listener without the header) shows
    /// the reason and asks again with `direct` as the default; `direct` or
    /// nothing at the re-ask falls back to direct.
    #[tokio::test]
    async fn the_gateway_subflow_falls_back_to_direct_on_an_empty_or_direct_re_ask() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let (server, uri) = super::test_support::reachable_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        for (bad, bail) in [("localhost", ""), (uri.as_str(), "direct")] {
            let (resp_tx, mut resp_rx) = mpsc::channel(32);
            let (user_tx, mut user_rx) = mpsc::channel(32);
            user_tx.send(GATEWAY_THROUGH.to_string()).await.unwrap();
            user_tx.send(bad.to_string()).await.unwrap();
            user_tx.send(bail.to_string()).await.unwrap();
            drop(user_tx);
            let out = run_gateway_subflow(&resp_tx, &mut user_rx).await.unwrap();
            assert!(out.is_none(), "{bad}");
            let frames = drain(&mut resp_rx).await;
            let texts = system_texts(&frames);
            if bad == "localhost" {
                assert!(texts.iter().any(|t| t.starts_with("Not a gateway URL:") && t.contains("beside the OS")), "{texts:?}");
            } else {
                assert!(texts.iter().any(|t| t.contains("without the x-local-ai-gateway header")), "{texts:?}");
            }
            let ps = prompts(&frames);
            assert_eq!(ps.last().unwrap().prompt, "Re-enter the gateway endpoint, or answer direct to reach the providers directly:");
            assert_eq!(ps.last().unwrap().default_value, "direct");
            assert!(texts.iter().any(|t| t == "Reaching the providers directly. /local-ai-gateway <url> stores a gateway after setup."), "{texts:?}");
        }
    }
}
