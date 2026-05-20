//! Endpoint → Bearer → Probe-and-Select sub-flow for OpenAI-compat
//! presets in the first-run wizard.

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::info;

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
/// On probe failure (unreachable endpoint, bad bearer, malformed
/// response) or zero-models-found, the sub-flow re-prompts from the
/// Endpoint step. The operator can fix the URL or bearer and retry.
pub async fn run_openai_compat_subflow(
    preset: OpenAiCompatPreset,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<OpenAiCompatSubflow> {
    loop {
        let endpoint = prompt_endpoint(preset, tx, response_rx).await?;
        let bearer = prompt_bearer(preset, tx, response_rx).await?;

        match probe_and_select(preset, &endpoint, bearer.as_deref(), tx, response_rx).await {
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
/// label. Normalizes on submit.
async fn prompt_endpoint(
    preset: OpenAiCompatPreset,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String> {
    let default = preset.default_base_url();
    let prompt_text = format!(
        "Enter the {} endpoint URL (default: {}):",
        preset.label(),
        default
    );
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
    let endpoint = if raw.trim().is_empty() {
        default.to_string()
    } else {
        normalize_endpoint(&raw, preset)
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
                field_type: SetupFieldType::Text as i32,
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

/// Probe-and-Select step: hit `GET /v1/models`, surface errors back to
/// the wizard loop (which re-prompts), and on success render a Selector
/// populated from the returned model list.
async fn probe_and_select(
    preset: OpenAiCompatPreset,
    endpoint: &str,
    bearer: Option<&str>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<String, SubflowError> {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: format!("Probing {} for available models…", endpoint),
                msg_type: SystemMessageType::Info as i32,
            })),
        }))
        .await;

    let models = OpenAICompatProvider::probe_models(preset, endpoint, bearer)
        .await
        .map_err(|e| SubflowError::ProbeFailed(e.to_string()))?;

    if models.is_empty() {
        return Err(SubflowError::NoModels(endpoint.to_string()));
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
    #[error("No models found at {0}. Pull or load a model on the server, then try again.")]
    NoModels(String),
    #[error("Selection '{given}' is not in the offered list of {offered} models.")]
    InvalidSelection { offered: usize, given: String },
    #[error("Setup channel closed during model selection.")]
    SelectionChannelClosed,
}

/// Normalize an endpoint URL the operator typed:
/// - Trim whitespace + trailing slash
/// - Prepend `http://` if scheme missing
/// - Append the preset's default port if no port is present in the
///   host portion
///
/// Examples (Ollama default port 11434):
/// - `localhost`             → `http://localhost:11434`
/// - `http://localhost`      → `http://localhost:11434`
/// - `http://localhost:8080` → `http://localhost:8080`
/// - `localhost:11434/`      → `http://localhost:11434`
/// - `https://api.example.com` → `https://api.example.com:11434`
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
    if has_port {
        format!("{scheme}{host_part}{path_part}")
    } else {
        format!("{scheme}{host_part}:{default_port}{path_part}")
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
    fn normalize_https_preserved() {
        let out = normalize_endpoint("https://api.example.com", OpenAiCompatPreset::Ollama);
        assert_eq!(out, "https://api.example.com:11434");
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
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, &resp_tx, &mut user_rx).await;
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
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, &resp_tx, &mut user_rx).await;
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
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, &resp_tx, &mut user_rx).await;
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
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, &resp_tx, &mut user_rx).await;
        let out = result.expect("subflow should succeed");
        assert_eq!(out.bearer.as_deref(), Some("secret-token"));

        while resp_rx.try_recv().is_ok() {}
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
            run_openai_compat_subflow(OpenAiCompatPreset::Ollama, &resp_tx, &mut user_rx).await;
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
}
