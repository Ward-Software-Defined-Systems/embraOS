//! `native_ask`: one question to the native model (experimental), from any
//! session.
//!
//! The call goes the way a native turn's request goes
//! (`OpenAICompatProvider`'s native request path): the same body, the same
//! wait for embrad and for the model's file, the same sentences when no
//! server will come. So what a session on another provider sees through
//! this tool is what a session on the native model would get, which is the
//! tool's second purpose: the native model and its request path are tested
//! from a session that does not depend on them.
//!
//! A session that the native model itself leads calls it too (William,
//! 2026-11-22, to experiment with it): the model then asks itself, in a
//! context of its own. The model server has one slot, so the call takes it
//! between two steps of the turn. The server keeps the turn's prompt in its
//! cache meanwhile and takes it back at the next step: embrad sizes that
//! cache for a prompt that fills the window (`embrad/src/native_model.rs`).
//!
//! The model asked sees what the call sends it and nothing else: no
//! session, no memory, no soul. Tools can be offered to it, by name or the
//! whole list as a turn sends it; the calls it asks for are reported and
//! none is run.

use std::time::Duration;

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use super::registry::{self, DispatchContext, ToolDescriptor};
use crate::provider::ir::{ApiMessage, Block, EarlyStopReason, TurnOutcome};
use crate::provider::openai_compat::native::{EFFORT_OFF, thinks};
use crate::provider::openai_compat::wire::{ProgressSink, ReadProgress};
use crate::provider::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset, span_words};
use crate::provider::{LlmProvider, LlmRequestOptions, ProviderKind, StreamEvent, SystemPromptBundle};

/// How long a call may take: the dispatcher's limit for one tool call, less
/// the time to say what ran out.
const ASK_LIMIT: Duration = registry::MAX_TOOL_DURATION.saturating_sub(Duration::from_secs(30));

/// The word of `offer_tools` that offers the whole tool list.
const ALL_TOOLS: &str = "all";

/// What an answer says of its text, by who leads the session: the leading
/// model did not write it, or wrote it without anything of the session.
const NOTE_ANOTHER: &str = "the native model's own text (experimental); another model wrote it";
const NOTE_ITSELF: &str = "the native model's own text (experimental); the same model wrote it in a context of its own, with nothing of this session";

/// What it says while no model is kept loaded.
const NOT_LOADED: &str = "no native model is loaded (experimental): the operator loads one with /native use <model>, and /native shows the models.";

// One question to the native model. A plain comment: a doc comment on an
// args struct is the schema's description, which the model is sent.
#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "native_ask",
    description = "Ask the native model (experimental) one question and get its answer. The native model runs on this machine's own processor (Qwen3.5 4B or 9B, loaded by the operator with /native use); nothing leaves the machine. It is a small model, and it sees only what this call sends it (no session, no memory, no identity): what comes back is its text, to be judged like any other source. Use it when the operator asks for the native model's answer, to compare or test it, or for work that should stay on the machine. `offer_tools` offers it tools from this tool list; the calls it asks for are reported and never run. The result is JSON: `text`, `reasoning` when it thought first, `tool_calls` (name and arguments) when it asked for any, `ended` (end_turn, tool_use, max_tokens or why the answer was cut) and `timings` as its server reports them (prompt tokens taken from its cache, read and written, and how fast). A call takes seconds to minutes: the first one after a model was loaded reads its whole prompt, later ones reuse what the server has. When the native model itself leads the session, the call asks the same model in a context of its own, between two steps of the turn. An error says so when no model is loaded."
)]
pub struct NativeAskArgs {
    /// The question or instruction for the native model.
    pub prompt: String,
    /// A system text for this call. Without one the model is sent none.
    #[serde(default)]
    pub system: Option<String>,
    /// Whether the model thinks before it answers. Without it, as the operator set it for the model with /effort (on unless switched off).
    #[serde(default)]
    pub thinking: Option<bool>,
    /// The most tokens the answer may take, its reasoning included. Without it the model ends by itself.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Tools to offer the native model: names from this tool list, or the one word `all` for the whole list as a turn sends it. The calls it asks for are reported and never run.
    #[serde(default)]
    pub offer_tools: Option<Vec<String>>,
}

impl NativeAskArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let cfg = ctx.config;
        // The model asked is the one that leads the session: said in the
        // answer, and nothing else differs.
        let itself = ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native);
        let Some(model) = cfg.openai_compat.chosen_native_model() else {
            return Err(DispatchError::Handler(NOT_LOADED.to_string()));
        };
        let offered = offered_tools(self.offer_tools.as_deref()).map_err(DispatchError::Handler)?;
        // The operator's own setting for the model, unless the call says.
        let thinking = self
            .thinking
            .unwrap_or_else(|| thinks(cfg.openai_compat.native_effort_by_model.get(model.id).map(String::as_str)));
        let provider = OpenAICompatProvider::new(
            OpenAiCompatPreset::Native,
            embra_boot::native_model::BASE_URL.to_string(),
            None,
            model.id.to_string(),
        )
        .with_model_wait(cfg.openai_compat.model_wait());
        let asked = Asked { prompt: &self.prompt, system: self.system.as_deref(), thinking, max_tokens: self.max_tokens, offered: &offered, itself };
        let answer = ask(provider, &asked, ASK_LIMIT).await.map_err(DispatchError::Handler)?;
        Ok(serde_json::to_string_pretty(&answer).unwrap_or_default())
    }
}

/// The tools a call offers the model: none, the ones named, or the whole
/// list as a turn builds it. A name that is no tool of the list refuses the
/// call, with the names.
fn offered_tools(names: Option<&[String]>) -> Result<Vec<&'static ToolDescriptor>, String> {
    let names: Vec<&str> = names.unwrap_or_default().iter().map(|name| name.trim()).filter(|name| !name.is_empty()).collect();
    if names.contains(&ALL_TOOLS) {
        if names.len() > 1 {
            return Err(format!("offer_tools takes `{ALL_TOOLS}` alone, or names from the tool list."));
        }
        return Ok(registry::all_descriptors().collect());
    }
    let unknown: Vec<&str> = names.iter().copied().filter(|name| !registry::REGISTRY.contains_key(name)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "not tools of the tool list: {}. offer_tools takes names from the tool list, or `{ALL_TOOLS}` alone.",
            unknown.join(", ")
        ));
    }
    let mut offered: Vec<&'static ToolDescriptor> = names.iter().filter_map(|name| registry::REGISTRY.get(name).copied()).collect();
    offered.sort_by_key(|tool| tool.name);
    offered.dedup_by_key(|tool| tool.name);
    Ok(offered)
}

/// One call, as it is asked.
struct Asked<'a> {
    prompt: &'a str,
    system: Option<&'a str>,
    thinking: bool,
    max_tokens: Option<u32>,
    offered: &'a [&'static ToolDescriptor],
    /// Whether the native model leads the session the call comes from.
    itself: bool,
}

/// How an answer ended, in the words the tool's description names.
fn ended_words(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::EndTurn => "end_turn",
        TurnOutcome::ToolUse => "tool_use",
        TurnOutcome::MaxTokens => "max_tokens",
        TurnOutcome::Pause => "pause",
        TurnOutcome::EarlyStop(EarlyStopReason::StreamEnded) => "cut: the server's answer ended before it said why",
        TurnOutcome::EarlyStop(EarlyStopReason::ContextWindow) => "cut: the context window was full",
        TurnOutcome::EarlyStop(_) => "cut",
    }
}

/// What a call says when its limit ended it: what ran out and, when the
/// model server said how far it had read, how far that was. What the server
/// read stays in its cache, so the sentence says that the same call again
/// goes on from there: on a slow processor a long prompt is read over two
/// calls, and nothing else tells the caller so.
fn out_of_time(limit: Duration, read: Option<ReadProgress>) -> String {
    let head = format!(
        "the native model did not finish within {}, and the call was ended: one tool call runs {} at most.",
        span_words(limit),
        span_words(registry::MAX_TOOL_DURATION)
    );
    let came_to = match read {
        Some(read) if !read.whole() => {
            let cached = if read.cached > 0 { format!(" ({} of them from its cache)", read.cached) } else { String::new() };
            format!(
                " It had read {} of the request's {} tokens{cached}. What it read is kept: the same call again goes on from there.",
                read.read, read.total
            )
        }
        Some(read) => format!(" It had read the whole request ({} tokens) and was writing its answer.", read.total),
        None => " The first call after a model was loaded reads its whole prompt, and a later one reuses what the server has.".to_string(),
    };
    format!("{head}{came_to} A shorter prompt, fewer offered tools, thinking off or max_tokens make a call shorter.")
}

/// Ask the model through `provider` and gather its answer. A call that
/// outlasts `limit` is ended: dropping it drops the request, and the server
/// stops its work for it.
async fn ask(provider: OpenAICompatProvider, asked: &Asked<'_>, limit: Duration) -> Result<JsonValue, String> {
    ask_into(provider, asked, limit, ProgressSink::default()).await
}

/// [`ask`], with the place the call's reading progress is kept: outside the
/// call, since the limit drops the call and its stream with it.
async fn ask_into(provider: OpenAICompatProvider, asked: &Asked<'_>, limit: Duration, progress: ProgressSink) -> Result<JsonValue, String> {
    let provider = provider
        .with_reasoning_effort((!asked.thinking).then(|| EFFORT_OFF.to_string()))
        .with_max_tokens(asked.max_tokens)
        .with_read_progress(progress.clone());
    let model = provider.display_name().to_string();
    let manifest = provider.build_tool_manifest(asked.offered);
    let system = SystemPromptBundle { text: asked.system.unwrap_or_default().to_string(), session_name: String::new() };
    let messages = [ApiMessage::user_text(asked.prompt)];
    let call = async {
        let (mut stream, timings) = provider
            .open_turn(&messages, &system, &manifest, LlmRequestOptions { include_reasoning: true })
            .await
            .map_err(|e| e.to_string())?;
        let (mut reasoning, mut turn, mut failed) = (String::new(), None, None);
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::ReasoningDelta(text) => reasoning.push_str(&text),
                StreamEvent::Complete(complete) => turn = Some(complete),
                StreamEvent::Error(why) => failed = Some(why),
                StreamEvent::TextDelta(_) | StreamEvent::BlockComplete => {}
            }
        }
        let turn = match (turn, failed) {
            (Some(turn), _) => turn,
            (None, Some(why)) => return Err(why),
            (None, None) => return Err("the model server's answer ended without a result".to_string()),
        };
        let text: Vec<&str> = turn.content.iter().filter_map(|block| if let Block::Text(text) = block { Some(text.as_str()) } else { None }).collect();
        let calls: Vec<JsonValue> = turn
            .content
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall { name, args, .. } => Some(json!({"name": name, "arguments": args})),
                _ => None,
            })
            .collect();
        let mut answer = serde_json::Map::new();
        answer.insert("model".into(), json!(provider.display_name()));
        answer.insert("thinking".into(), json!(asked.thinking));
        answer.insert("text".into(), json!(text.join("\n")));
        if !reasoning.is_empty() {
            answer.insert("reasoning".into(), json!(reasoning));
        }
        if !asked.offered.is_empty() {
            answer.insert("tools_offered".into(), json!(asked.offered.len()));
        }
        if !calls.is_empty() {
            answer.insert("tool_calls".into(), JsonValue::Array(calls));
            answer.insert("tool_calls_note".into(), json!("asked for by the native model; none was run"));
        }
        answer.insert("ended".into(), json!(ended_words(turn.outcome)));
        // The stream has ended, so the server's timings of the call are in.
        if let Ok(Some(timings)) = timings.await {
            let mut figures = serde_json::Map::new();
            let mut put = |key: &str, value: Option<JsonValue>| {
                if let Some(value) = value {
                    figures.insert(key.to_string(), value);
                }
            };
            put("cached_tokens", timings.cached_tokens.map(JsonValue::from));
            put("prompt_tokens", timings.prompt_tokens.map(JsonValue::from));
            put("prompt_ms", timings.prompt_ms.map(JsonValue::from));
            put("prompt_tokens_per_second", timings.prompt_tokens_per_second.map(JsonValue::from));
            put("output_tokens", timings.output_tokens.map(JsonValue::from));
            put("output_ms", timings.output_ms.map(JsonValue::from));
            put("output_tokens_per_second", timings.output_tokens_per_second.map(JsonValue::from));
            answer.insert("timings".into(), JsonValue::Object(figures));
        }
        answer.insert("note".into(), json!(if asked.itself { NOTE_ITSELF } else { NOTE_ANOTHER }));
        Ok(JsonValue::Object(answer))
    };
    match tokio::time::timeout(limit, call).await {
        Ok(answer) => answer,
        Err(_) => {
            let read = progress.lock().ok().and_then(|latest| *latest);
            // A call that ends here has no timings of the server's: this is
            // its one line, beside the lines of the calls that completed.
            tracing::info!(
                target: "openai_compat::timings",
                provider = "native",
                model = %model,
                limit_seconds = limit.as_secs(),
                prompt_tokens = read.map(|r| r.total),
                cached_tokens = read.map(|r| r.cached),
                tokens_behind = read.map(|r| r.read),
                read_ms = read.map(|r| r.ms),
                "a native_ask call was ended at its limit"
            );
            Err(out_of_time(limit, read))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_model::test_support::machine;
    use crate::tools::registry::dispatch;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sse(frames: &[&str]) -> ResponseTemplate {
        let mut body: String = frames.iter().map(|frame| format!("data: {frame}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        ResponseTemplate::new(200).set_body_string(body).insert_header("content-type", "text/event-stream")
    }

    /// A stand-in for the model server that answers every request with
    /// `answer`.
    async fn model_server(answer: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(answer).mount(&server).await;
        server
    }

    /// A provider on the stand-in, with the two records of a machine of the
    /// test's own (none: no embrad there, so the request goes).
    fn provider(server: &MockServer, tag: &str) -> OpenAICompatProvider {
        let (_root, env) = machine(tag);
        OpenAICompatProvider::new(OpenAiCompatPreset::Native, server.uri(), None, "qwen3.5-4b".to_string()).with_native_env(env)
    }

    fn asked<'a>(prompt: &'a str, offered: &'a [&'static ToolDescriptor]) -> Asked<'a> {
        Asked { prompt, system: None, thinking: true, max_tokens: None, offered, itself: false }
    }

    async fn bodies(server: &MockServer) -> Vec<JsonValue> {
        server.received_requests().await.unwrap().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// The three forms a call takes: no tools, tools by name, the whole
    /// list. Each is the request a native turn sends, with what the call
    /// set: the system text or none, thinking, the token limit.
    #[tokio::test]
    async fn a_call_offers_no_tools_the_named_ones_or_the_whole_list() {
        let server = model_server(sse(&[r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#])).await;

        // No tools, no system text, thinking as asked.
        let answer = ask(provider(&server, "ask-plain"), &asked("Say hi.", &[]), DAY).await.unwrap();
        assert_eq!((answer["text"].as_str(), answer["ended"].as_str(), answer["model"].as_str()), (Some("hi"), Some("end_turn"), Some("qwen3.5-4b")));
        assert!(answer.get("tools_offered").is_none() && answer.get("tool_calls").is_none() && answer.get("reasoning").is_none(), "{answer}");

        // Two tools by name, a system text, thinking off, a token limit.
        let two = offered_tools(Some(&["system_status".to_string(), "calculate".to_string(), "calculate".to_string()])).unwrap();
        let direct = Asked { prompt: "What is 2+2?", system: Some("Answer briefly."), thinking: false, max_tokens: Some(64), offered: &two, itself: false };
        let answer = ask(provider(&server, "ask-named"), &direct, DAY).await.unwrap();
        assert_eq!((answer["tools_offered"].as_u64(), answer["thinking"].as_bool()), (Some(2), Some(false)));

        // The whole list, as a turn builds it.
        let all = offered_tools(Some(&["all".to_string()])).unwrap();
        assert_eq!(all.len(), registry::tool_count());
        ask(provider(&server, "ask-all"), &asked("Which tools do you have?", &all), DAY).await.unwrap();

        let sent = bodies(&server).await;
        assert_eq!(sent.len(), 3);
        // The plain call: the question alone, no system message, no tools.
        assert_eq!(sent[0]["messages"], json!([{"role": "user", "content": "Say hi."}]));
        assert!(sent[0].get("tools").is_none() && sent[0].get("tool_choice").is_none() && sent[0].get("max_tokens").is_none(), "{}", sent[0]);
        assert_eq!(sent[0]["chat_template_kwargs"], json!({"enable_thinking": true}));
        assert_eq!(sent[0]["temperature"].as_f64(), Some(1.0), "the model card's sampling, as a turn's");
        // The named call.
        assert_eq!(sent[1]["messages"], json!([{"role": "system", "content": "Answer briefly."}, {"role": "user", "content": "What is 2+2?"}]));
        let names = |body: &JsonValue| -> Vec<String> {
            body["tools"].as_array().unwrap().iter().map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(names(&sent[1]), ["calculate", "system_status"], "each once");
        assert_eq!((sent[1]["tool_choice"].as_str(), sent[1]["max_tokens"].as_u64()), (Some("auto"), Some(64)));
        assert_eq!(sent[1]["chat_template_kwargs"], json!({"enable_thinking": false}));
        assert_eq!(sent[1]["temperature"].as_f64(), Some(0.7));
        // The whole list: every tool of the registry, this one among them.
        let whole = names(&sent[2]);
        assert_eq!(whole.len(), registry::tool_count());
        assert!(whole.iter().any(|name| name == "native_ask") && whole.iter().any(|name| name == "file_read"));
        // And it is the list a native turn sends, byte for byte.
        let turn = provider(&server, "ask-turn").build_tool_manifest(&registry::all_descriptors().collect::<Vec<_>>());
        assert_eq!(sent[2]["tools"], turn.wire_json);
    }

    /// The model's answer as the call returns it: the text, the reasoning
    /// apart, the calls it asked for with the word that none was run, how it
    /// ended, and the server's timings.
    #[tokio::test]
    async fn the_answer_carries_the_text_the_reasoning_the_calls_and_the_timings() {
        let server = model_server(sse(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"The user wants the time. "}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"I have a tool for it."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Let me look."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"time","arguments":"{\"zone\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"UTC\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"timings":{"cache_n":23398,"prompt_n":4,"prompt_ms":480.5,"prompt_per_second":8.3,"predicted_n":61,"predicted_ms":13555.0,"predicted_per_second":4.5}}"#,
        ]))
        .await;
        let time = offered_tools(Some(&["time".to_string()])).unwrap();
        let answer = ask(provider(&server, "ask-answer"), &asked("What time is it?", &time), DAY).await.unwrap();
        assert_eq!(
            answer,
            json!({
                "model": "qwen3.5-4b",
                "thinking": true,
                "text": "Let me look.",
                "reasoning": "The user wants the time. I have a tool for it.",
                "tools_offered": 1,
                "tool_calls": [{"name": "time", "arguments": {"zone": "UTC"}}],
                "tool_calls_note": "asked for by the native model; none was run",
                "ended": "tool_use",
                "timings": {
                    "cached_tokens": 23398, "prompt_tokens": 4, "prompt_ms": 480.5, "prompt_tokens_per_second": 8.3,
                    "output_tokens": 61, "output_ms": 13555.0, "output_tokens_per_second": 4.5
                },
                "note": NOTE_ANOTHER
            })
        );
        // One request went out, and nothing else: the call it asked for was not run.
        assert_eq!(bodies(&server).await.len(), 1);
        // From a session the native model itself leads the request is the
        // same, and the note says who wrote the text.
        let itself = Asked { itself: true, ..asked("What time is it?", &time) };
        let again = ask(provider(&server, "ask-itself"), &itself, DAY).await.unwrap();
        assert_eq!(again["note"], json!(NOTE_ITSELF));
        let mut same = again.clone();
        same["note"] = json!(NOTE_ANOTHER);
        assert_eq!(same, answer, "nothing but the note differs");
        let sent = bodies(&server).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1], "the two requests are one request");

        // A server that sends no timings, and an answer cut by the token limit.
        let cut = model_server(sse(&[r#"{"choices":[{"index":0,"delta":{"content":"One, two"},"finish_reason":"length"}]}"#])).await;
        let answer = ask(provider(&cut, "ask-cut"), &asked("Count.", &[]), DAY).await.unwrap();
        assert_eq!((answer["text"].as_str(), answer["ended"].as_str()), (Some("One, two"), Some("max_tokens")));
        assert!(answer.get("timings").is_none(), "{answer}");
    }

    #[test]
    fn the_tools_offered_are_none_the_named_ones_or_all_and_a_name_that_is_no_tool_refuses_the_call() {
        assert!(offered_tools(None).unwrap().is_empty());
        assert!(offered_tools(Some(&[])).unwrap().is_empty());
        assert!(offered_tools(Some(&[" ".to_string()])).unwrap().is_empty());
        let named: Vec<&str> = offered_tools(Some(&["recall".to_string(), " calculate ".to_string()])).unwrap().iter().map(|tool| tool.name).collect();
        assert_eq!(named, ["calculate", "recall"]);
        assert_eq!(
            offered_tools(Some(&["calculate".to_string(), "frobnicate".to_string(), "web_search".to_string()])).err().as_deref(),
            Some("not tools of the tool list: frobnicate, web_search. offer_tools takes names from the tool list, or `all` alone.")
        );
        assert_eq!(
            offered_tools(Some(&["all".to_string(), "calculate".to_string()])).err().as_deref(),
            Some("offer_tools takes `all` alone, or names from the tool list.")
        );
    }

    fn config(provider: &str, native_model: &str) -> crate::config::SystemConfig {
        let mut cfg: crate::config::SystemConfig = serde_json::from_value(json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase2",
            "created_at": "", "version": "test", "api_provider": provider,
        }))
        .expect("a configuration");
        cfg.openai_compat.native_model = native_model.to_string();
        cfg
    }

    /// Through the dispatcher, as the tool loop calls it: said when no
    /// model is loaded, and refused for a tool name that is none, each
    /// before any request. A native model that leads the session is no
    /// reason to refuse: the call gets as far as its tool names.
    #[tokio::test]
    async fn the_tool_says_when_no_model_is_loaded_and_is_open_to_a_session_a_native_model_leads() {
        let db = crate::db::WardsonDbClient::from_url("http://127.0.0.1:1");
        let trace = embra_tools_core::new_turn_trace_handle();
        let call = |cfg: crate::config::SystemConfig, input: JsonValue| {
            let (db, trace) = (&db, &trace);
            async move {
                let ctx = DispatchContext { db, config: &cfg, session_name: "main", config_tz: "UTC", trace, turn_index: 0, deleting_session: None };
                dispatch("native_ask", input, ctx).await.map(|out| out.text).map_err(|e| e.to_string())
            }
        };
        let said = call(config("anthropic", ""), json!({"prompt": "hi"})).await.unwrap_err();
        assert!(said.ends_with(NOT_LOADED), "{said}");
        for leading in ["native", "anthropic", "lm_studio"] {
            let said = call(config(leading, "qwen3.5-4b"), json!({"prompt": "hi", "offer_tools": ["frobnicate"]})).await.unwrap_err();
            assert!(said.contains("not tools of the tool list: frobnicate."), "{leading}: {said}");
            assert!(!said.contains("refused while"), "{leading}: {said}");
        }
        let said = call(config("anthropic", "qwen3.5-4b"), json!({"prompt": "hi", "offer_tools": ["frobnicate"]})).await.unwrap_err();
        assert!(said.contains("not tools of the tool list: frobnicate."), "{said}");
        // The prompt is the one field a call needs.
        assert!(call(config("anthropic", "qwen3.5-4b"), json!({})).await.is_err());
    }

    /// A server that takes the request and says nothing. `asked` is raised
    /// when the request has arrived, `gone` when the client has closed.
    struct Silent {
        base: String,
        asked: std::sync::Arc<tokio::sync::Notify>,
        gone: std::sync::Arc<tokio::sync::Notify>,
    }

    async fn silent_server() -> Silent {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a listener");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let (asked, gone) = (std::sync::Arc::new(tokio::sync::Notify::new()), std::sync::Arc::new(tokio::sync::Notify::new()));
        let (said, left) = (asked.clone(), gone.clone());
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buf).await;
            said.notify_one();
            // Held open, silent, until the client goes.
            while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
            left.notify_one();
        });
        Silent { base, asked, gone }
    }

    /// A stop ends the call: the turn loop drops the tool's future, and the
    /// request goes with it, so the server sees the connection close and
    /// stops its work.
    #[tokio::test]
    async fn a_call_that_is_dropped_closes_its_request() {
        let server = silent_server().await;
        let (_root, env) = machine("ask-stop");
        let provider = OpenAICompatProvider::new(OpenAiCompatPreset::Native, server.base.clone(), None, "qwen3.5-4b".to_string()).with_native_env(env);
        let call = tokio::spawn(async move { ask(provider, &asked("Think for a long time.", &[]), DAY).await });
        // The deadlines are ones no loaded machine reaches when a stop works.
        tokio::time::timeout(Duration::from_secs(120), server.asked.notified()).await.expect("the request arrives");
        call.abort();
        tokio::time::timeout(Duration::from_secs(120), server.gone.notified()).await.expect("the connection closes with the call");
    }

    /// A call that outlasts its limit is ended and says what ran out.
    #[tokio::test]
    async fn a_call_that_outlasts_its_limit_is_ended_and_says_so() {
        let server = silent_server().await;
        let (_root, env) = machine("ask-limit");
        let provider = OpenAICompatProvider::new(OpenAiCompatPreset::Native, server.base.clone(), None, "qwen3.5-4b".to_string()).with_native_env(env);
        let call = async { ask(provider, &asked("Think for a long time.", &[]), Duration::from_secs(570)).await };
        let clock = async {
            server.asked.notified().await;
            tokio::time::pause();
        };
        // A day on the paused clock: a call without its bound would be at
        // this deadline at once, where it would never return.
        let (answer, ()) = tokio::time::timeout(DAY, async { tokio::join!(call, clock) }).await.expect("the call ends at its limit");
        assert_eq!(
            answer.unwrap_err(),
            "the native model did not finish within 570 seconds, and the call was ended: one tool call runs 10 minutes at most. The first call after a model was loaded reads its whole prompt, and a later one reuses what the server has. A shorter prompt, fewer offered tools, thinking off or max_tokens make a call shorter."
        );
        assert_eq!(ASK_LIMIT, Duration::from_secs(570), "under the dispatcher's limit, with room to say so");
    }

    /// A server that takes the request, answers with the head of a stream
    /// and `frames`, and then says nothing more, as a model server that is
    /// still reading its prompt.
    async fn reading_server(frames: Vec<String>) -> Silent {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a listener");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let (asked, gone) = (std::sync::Arc::new(tokio::sync::Notify::new()), std::sync::Arc::new(tokio::sync::Notify::new()));
        let (said, left) = (asked.clone(), gone.clone());
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buf).await;
            let mut answer = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n".to_string();
            for frame in &frames {
                let data = format!("data: {frame}\n\n");
                answer.push_str(&format!("{:x}\r\n{data}\r\n", data.len()));
            }
            let _ = socket.write_all(answer.as_bytes()).await;
            said.notify_one();
            while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
            left.notify_one();
        });
        Silent { base, asked, gone }
    }

    /// A call that its limit ends while the model server reads says how far
    /// the server had come and that the same call again goes on from there.
    #[tokio::test]
    async fn a_call_ended_while_the_server_reads_says_how_far_it_came() {
        let frame = |processed: u64| {
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"role": "assistant", "content": null}}],
                   "prompt_progress": {"total": 23941, "cache": 4096, "processed": processed, "time_ms": 1000}})
            .to_string()
        };
        let server = reading_server(vec![frame(4096), frame(4608), frame(15360)]).await;
        let (_root, env) = machine("ask-limit-reading");
        let provider = OpenAICompatProvider::new(OpenAiCompatPreset::Native, server.base.clone(), None, "qwen3.5-4b".to_string()).with_native_env(env);
        let progress = ProgressSink::default();
        let seen = progress.clone();
        let call = async { ask_into(provider, &asked("Which tools do you have?", &[]), Duration::from_secs(570), progress).await };
        let clock = async {
            // The clock stops once the last frame has been read: from there
            // the server is silent, and only the call's limit can end it.
            // A deadline no loaded machine reaches when the frames are kept:
            // a parser that keeps none fails here and does not hang.
            let last_frame = async {
                while seen.lock().unwrap().map(|read| read.read) != Some(15360) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(120), last_frame).await.expect("the last frame is kept for the caller");
            tokio::time::pause();
        };
        let (answer, ()) = tokio::time::timeout(DAY, async { tokio::join!(call, clock) }).await.expect("the call ends at its limit");
        assert_eq!(
            answer.unwrap_err(),
            "the native model did not finish within 570 seconds, and the call was ended: one tool call runs 10 minutes at most. It had read 15360 of the request's 23941 tokens (4096 of them from its cache). What it read is kept: the same call again goes on from there. A shorter prompt, fewer offered tools, thinking off or max_tokens make a call shorter."
        );
    }

    /// The sentence of a call that ran out of time, by what the server had
    /// said of its reading: nothing, a part of the prompt, all of it.
    #[test]
    fn a_call_out_of_time_says_what_the_server_had_read() {
        let limit = Duration::from_secs(570);
        let silent = out_of_time(limit, None);
        assert!(silent.contains("The first call after a model was loaded reads its whole prompt") && !silent.contains("It had read"), "{silent}");
        let part = out_of_time(limit, Some(ReadProgress { total: 23402, cached: 0, read: 10240, ms: 558_000 }));
        assert!(part.contains(" It had read 10240 of the request's 23402 tokens. What it read is kept: the same call again goes on from there."), "{part}");
        assert!(!part.contains("from its cache"), "nothing came from the cache: {part}");
        let whole = out_of_time(limit, Some(ReadProgress { total: 300, cached: 0, read: 300, ms: 8_000 }));
        assert!(whole.contains(" It had read the whole request (300 tokens) and was writing its answer."), "{whole}");
        assert!(!whole.contains("goes on from there"), "there is nothing left to read: {whole}");
        for said in [&silent, &part, &whole] {
            assert!(said.starts_with("the native model did not finish within 570 seconds, and the call was ended: one tool call runs 10 minutes at most."), "{said}");
            assert!(said.ends_with(" A shorter prompt, fewer offered tools, thinking off or max_tokens make a call shorter."), "{said}");
        }
    }

    /// The three forms against a model server that is really there, printed:
    /// run by hand where one listens, with the model's id and the server's
    /// base in the environment.
    ///
    /// `EMBRA_NATIVE_URL=http://127.0.0.1:8091 EMBRA_NATIVE_MODEL=qwen3.5-4b cargo test -p embra-brain the_native_model_can_be_asked -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "asks a model server that has to be running; run by hand"]
    async fn the_native_model_can_be_asked() {
        let base = std::env::var("EMBRA_NATIVE_URL").expect("EMBRA_NATIVE_URL names the model server's base");
        let model = std::env::var("EMBRA_NATIVE_MODEL").expect("EMBRA_NATIVE_MODEL names the model it holds");
        let (_root, env) = machine("ask-real");
        let provider = || OpenAICompatProvider::new(OpenAiCompatPreset::Native, base.clone(), None, model.clone()).with_native_env(env.clone());
        let two = offered_tools(Some(&["calculate".to_string(), "time".to_string()])).unwrap();
        let all = offered_tools(Some(&[ALL_TOOLS.to_string()])).unwrap();
        let direct = Asked { prompt: "What is the capital of France? One word.", system: Some("Answer briefly."), thinking: false, max_tokens: Some(64), offered: &[], itself: false };
        for (form, asked) in [
            ("no tools, thinking off", direct),
            ("two tools by name", asked("What is 17 times 23? Use a tool.", &two)),
            ("the whole list", asked("What time is it right now? Use a tool.", &all)),
        ] {
            let started = std::time::Instant::now();
            match ask(provider(), &asked, ASK_LIMIT).await {
                Ok(answer) => println!("== {form} ({:.1} s)\n{}", started.elapsed().as_secs_f64(), serde_json::to_string_pretty(&answer).unwrap()),
                Err(why) => println!("== {form} ({:.1} s): FAILED: {why}", started.elapsed().as_secs_f64()),
            }
        }
    }

    #[test]
    fn an_answers_end_is_said_in_the_descriptions_words() {
        assert_eq!(ended_words(TurnOutcome::EndTurn), "end_turn");
        assert_eq!(ended_words(TurnOutcome::ToolUse), "tool_use");
        assert_eq!(ended_words(TurnOutcome::MaxTokens), "max_tokens");
        assert!(ended_words(TurnOutcome::EarlyStop(EarlyStopReason::StreamEnded)).starts_with("cut: "));
    }
}
