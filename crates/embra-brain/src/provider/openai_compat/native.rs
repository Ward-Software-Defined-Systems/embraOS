//! What a native model's request differs in (experimental). The model
//! server is llama.cpp's and the models are Qwen3.5: the request is the
//! presets' own, with these differences.
//!
//! - Thinking is switched on every request (`chat_template_kwargs`): the
//!   server's own default is on, and a request that says nothing thinks. It
//!   is on unless the operator's `/effort` is `low`. `reasoning_effort` is
//!   not sent: the model has no levels.
//! - The sampling is the model card's for the thinking state, sent with the
//!   request: the server's defaults are its own, not the model's.
//! - The reasoning of an earlier step of the turn goes back as
//!   `reasoning_content`, the name the server reads. The model's template
//!   then renders the step as the model wrote it, and the server continues
//!   from what it has: measured with one tool call, 369 tokens were reused
//!   and 36 read, against 289 and 54 with `reasoning`, which the server
//!   ignores. (No reasoning is kept from one turn to the next, which is
//!   what the model card asks for.)
//! - Text only: an image is a line that says one was attached. The models
//!   read images through a second file that is not installed in this step.
//! - While the server reads the model's file it answers 503: the request
//!   waits and is sent again, as long as the model wait allows.
//! - A request longer than the context window the model was loaded with is
//!   answered 400, with both figures: the operator reads them in a sentence.

use std::time::Duration;

use serde_json::json;

use super::wire::{OpenAIChatRequest, OpenAIMessage, Sampling, UserContent, UserPart};
use crate::provider::ProviderError;

/// Qwen3.5's model card, "thinking mode for general tasks" (read
/// 2026-11-22, the same for the 4B and the 9B; `repetition_penalty=1.0` is
/// the server's default and is not sent).
pub(super) const SAMPLING_THINKING: Sampling =
    Sampling { temperature: 1.0, top_p: 0.95, top_k: 20, min_p: 0.0, presence_penalty: 1.5 };
/// The card's "instruct (or non-thinking) mode for general tasks".
pub(super) const SAMPLING_DIRECT: Sampling =
    Sampling { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0.0, presence_penalty: 1.5 };

/// The `/effort` level that switches thinking off. Any other level, and
/// none, leaves it on (William, 2026-11-22: on by default).
pub(crate) const EFFORT_OFF: &str = "low";

/// What stands where an image was.
pub(super) const IMAGE_LINE: &str =
    "[An image was attached here. The native model reads text only (experimental), so the image was not sent.]";

/// How often a request is sent again while the server loads its model.
pub(super) const LOADING_POLL: Duration = Duration::from_secs(1);

/// Whether a native model thinks, by the operator's `/effort` for it.
pub(crate) fn thinks(effort: Option<&str>) -> bool {
    effort.map(str::trim) != Some(EFFORT_OFF)
}

/// Give a request a native model's shape.
pub(super) fn shape(body: &mut OpenAIChatRequest, thinking: bool) {
    body.reasoning_effort = None;
    body.chat_template_kwargs = Some(json!({ "enable_thinking": thinking }));
    body.sampling = Some(if thinking { SAMPLING_THINKING } else { SAMPLING_DIRECT });
}

/// The messages as a native model's server takes them: an earlier step's
/// reasoning under the name it reads, and every image as a line of text.
pub(super) fn messages(messages: Vec<OpenAIMessage>) -> Vec<OpenAIMessage> {
    messages
        .into_iter()
        .map(|message| match message {
            OpenAIMessage::Assistant { content, tool_calls, reasoning, reasoning_content } => OpenAIMessage::Assistant {
                content,
                tool_calls,
                reasoning: None,
                reasoning_content: reasoning.or(reasoning_content),
            },
            OpenAIMessage::User { content: UserContent::Parts(parts) } => {
                let lines: Vec<String> = parts
                    .into_iter()
                    .map(|part| match part {
                        UserPart::Text { text } => text,
                        UserPart::ImageUrl { .. } => IMAGE_LINE.to_string(),
                    })
                    .collect();
                OpenAIMessage::User { content: UserContent::Text(lines.join("\n")) }
            }
            other @ (OpenAIMessage::System { .. } | OpenAIMessage::User { .. } | OpenAIMessage::Tool { .. }) => other,
        })
        .collect()
}

/// Whether a 503's body is the server saying that it is still reading the
/// model's file (`{"error":{"message":"Loading model","type":"unavailable_error","code":503}}`).
pub(super) fn is_loading(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(|m| m.eq_ignore_ascii_case("loading model")))
        .unwrap_or(false)
}

/// What a 400 says when the request is longer than the context window the
/// model was loaded with, in the brain's words; `None` for any other answer.
/// The server's own (llama.cpp 1.0.0):
/// `{"error":{"code":400,"message":"request (6010 tokens) exceeds the available context size (2048 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":6010,"n_ctx":2048}}`.
pub(super) fn over_the_window(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type").and_then(|kind| kind.as_str()) != Some("exceed_context_size_error") {
        return None;
    }
    let count = |key: &str| error.get(key).and_then(serde_json::Value::as_u64);
    let what = match (count("n_prompt_tokens"), count("n_ctx")) {
        (Some(request), Some(loaded)) => {
            format!("the request is {request} tokens and the native model is loaded with a context window of {loaded}")
        }
        _ => "the request is longer than the context window the native model is loaded with".to_string(),
    };
    Some(format!(
        "{what}. /native context <tokens> loads it with a larger one, which takes more memory; or continue in a new session (/new <name>)"
    ))
}

/// A failed request as the operator reads it: a request over the context
/// window keeps its status and gets the sentence above for its body. Every
/// other failure is said as the presets say it.
pub(super) fn in_the_brains_words(e: ProviderError) -> ProviderError {
    match e {
        ProviderError::Http { status: 400, body } => match over_the_window(&body) {
            Some(sentence) => ProviderError::Http { status: 400, body: sentence },
            None => ProviderError::Http { status: 400, body },
        },
        other => other,
    }
}

/// What the operator reads when the model was still loading when the model
/// wait ran out.
pub(super) fn still_loading_sentence(waited: Duration) -> String {
    let minutes = waited.as_secs() / 60;
    let length = match (waited.as_secs().is_multiple_of(60), minutes) {
        (true, 1) => "1 minute".to_string(),
        (true, minutes) => format!("{minutes} minutes"),
        (false, _) => format!("{} seconds", waited.as_secs()),
    };
    format!(
        "the native model was still loading after {length}; the request was dropped. /native shows what the model server is doing, and /model-wait sets how long embraOS waits"
    )
}

/// A request to a native model's server, sent until the server answers it.
/// `first` is what the first sending came to and `again` sends once more.
/// While the server says that it is reading the model's file the request
/// waits a second and goes again, until the model wait has run out since the
/// first sending. Any other failure is the presets' own rule: a server that
/// could not be reached and a 5xx are tried once more after a second, and
/// everything else is said as it is.
pub(super) async fn until_loaded<T, F, Fut>(wait: Duration, first: Result<T, ProviderError>, mut again: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let started = tokio::time::Instant::now();
    let (mut attempt, mut tried_again) = (first, false);
    loop {
        match &attempt {
            Err(ProviderError::Http { status: 503, body }) if is_loading(body) => {
                if started.elapsed() >= wait {
                    return Err(ProviderError::Silent(still_loading_sentence(wait)));
                }
                tokio::time::sleep(LOADING_POLL).await;
            }
            Err(e) if super::retriable(e) && !tried_again => {
                tried_again = true;
                tokio::time::sleep(super::RETRY_DELAY).await;
            }
            _ => return attempt,
        }
        attempt = again().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::openai_compat::wire::{ImageUrlRef, OpenAIToolCall, OpenAIToolCallFunction};

    #[test]
    fn thinking_is_on_unless_the_effort_is_low() {
        for on in [None, Some("medium"), Some("high"), Some("xhigh"), Some("max"), Some(""), Some("off")] {
            assert!(thinks(on), "{on:?}");
        }
        assert!(!thinks(Some("low")));
        assert!(!thinks(Some(" low ")));
    }

    fn request() -> OpenAIChatRequest {
        OpenAIChatRequest {
            model: "qwen3.5-4b".into(),
            messages: vec![OpenAIMessage::System { content: "s".into() }],
            stream: true,
            tools: None,
            tool_choice: None,
            reasoning_effort: Some("high".into()),
            max_tokens: None,
            chat_template_kwargs: None,
            sampling: None,
        }
    }

    #[test]
    fn a_request_says_whether_to_think_and_carries_the_cards_sampling_for_it() {
        let mut thinking = request();
        shape(&mut thinking, true);
        let body = serde_json::to_value(&thinking).unwrap();
        assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": true}));
        assert_eq!(
            (body["temperature"].as_f64(), body["top_p"].as_f64(), body["top_k"].as_u64(), body["min_p"].as_f64(), body["presence_penalty"].as_f64()),
            (Some(1.0), Some(0.95), Some(20), Some(0.0), Some(1.5))
        );
        assert!(body.get("reasoning_effort").is_none(), "the model has no levels: {body}");
        assert!(body.get("sampling").is_none(), "the sampling's fields are the request's own: {body}");

        let mut direct = request();
        shape(&mut direct, false);
        let body = serde_json::to_value(&direct).unwrap();
        assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": false}));
        assert_eq!((body["temperature"].as_f64(), body["top_p"].as_f64(), body["presence_penalty"].as_f64()), (Some(0.7), Some(0.8), Some(1.5)));
        // As the text the server reads: no rounding of the card's figures.
        let text = serde_json::to_string(&direct).unwrap();
        assert!(text.contains("\"temperature\":0.7,\"top_p\":0.8,\"top_k\":20,\"min_p\":0.0,\"presence_penalty\":1.5"), "{text}");
    }

    #[test]
    fn another_presets_request_carries_none_of_it() {
        let body = serde_json::to_value(request()).unwrap();
        let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["messages", "model", "reasoning_effort", "stream"], "as before the fields existed");
    }

    #[test]
    fn an_earlier_steps_reasoning_goes_back_under_the_name_the_server_reads() {
        let call = OpenAIToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: OpenAIToolCallFunction { name: "time".into(), arguments: "{}".into() },
        };
        let out = messages(vec![
            OpenAIMessage::System { content: "s".into() },
            OpenAIMessage::User { content: UserContent::Text("what time is it".into()) },
            OpenAIMessage::Assistant { content: None, tool_calls: Some(vec![call]), reasoning: Some("I need the time tool.".into()), reasoning_content: None },
            OpenAIMessage::Tool { tool_call_id: "c1".into(), content: "12:34".into() },
            OpenAIMessage::Assistant { content: Some("It is 12:34.".into()), tool_calls: None, reasoning: None, reasoning_content: None },
        ]);
        let wire = serde_json::to_value(&out).unwrap();
        assert_eq!(wire[2]["reasoning_content"], "I need the time tool.");
        assert!(wire[2].get("reasoning").is_none(), "{}", wire[2]);
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "time");
        assert!(wire[4].get("reasoning_content").is_none() && wire[4].get("reasoning").is_none(), "a step without reasoning has none: {}", wire[4]);
        assert_eq!((wire[0]["role"].as_str(), wire[1]["content"].as_str(), wire[3]["content"].as_str()), (Some("system"), Some("what time is it"), Some("12:34")));
    }

    #[test]
    fn an_image_is_a_line_of_text() {
        let image = || UserPart::ImageUrl { image_url: ImageUrlRef { url: "data:image/png;base64,AAAA".into() } };
        let out = messages(vec![
            OpenAIMessage::User { content: UserContent::Parts(vec![image(), UserPart::Text { text: "what is this?".into() }]) },
            OpenAIMessage::User { content: UserContent::Parts(vec![image(), image()]) },
        ]);
        let wire = serde_json::to_string(&out).unwrap();
        assert!(!wire.contains("base64") && !wire.contains("image_url"), "no image leaves: {wire}");
        let value = serde_json::to_value(&out).unwrap();
        assert_eq!(value[0]["content"], format!("{IMAGE_LINE}\nwhat is this?"));
        assert_eq!(value[1]["content"], format!("{IMAGE_LINE}\n{IMAGE_LINE}"));
    }

    #[test]
    fn a_503_is_the_model_loading_only_when_the_server_says_so() {
        assert!(is_loading(r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#));
        assert!(is_loading(r#"{"error":{"message":"loading model"}}"#));
        for other in [
            "",
            "Service Unavailable",
            r#"{"error":{"code":503,"message":"no slot available"}}"#,
            r#"{"error":"Loading model"}"#,
            r#"{"message":"Loading model"}"#,
        ] {
            assert!(!is_loading(other), "{other}");
        }
    }

    #[test]
    fn the_sentence_of_a_load_that_outlasts_the_wait_names_the_wait() {
        assert!(still_loading_sentence(Duration::from_secs(7200)).starts_with("the native model was still loading after 120 minutes;"));
        assert!(still_loading_sentence(Duration::from_secs(60)).starts_with("the native model was still loading after 1 minute;"));
        assert!(still_loading_sentence(Duration::from_secs(90)).starts_with("the native model was still loading after 90 seconds;"));
    }

    /// What a sending comes to in these tests, and the count of sendings.
    type Answer = Result<&'static str, ProviderError>;
    type Sendings = std::rc::Rc<std::cell::Cell<u32>>;

    fn loading() -> Answer {
        Err(ProviderError::Http { status: 503, body: r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#.into() })
    }

    /// The sendings a test scripts: each call takes the next, and the last
    /// one is repeated.
    fn scripted(mut answers: Vec<Answer>) -> (impl FnMut() -> std::future::Ready<Answer>, Sendings) {
        let calls = Sendings::default();
        let counted = calls.clone();
        answers.reverse();
        let again = move || {
            counted.set(counted.get() + 1);
            let next = if answers.len() > 1 { answers.pop().unwrap() } else { copy(answers.last().unwrap()) };
            std::future::ready(next)
        };
        (again, calls)
    }

    fn copy(answer: &Answer) -> Answer {
        match answer {
            Ok(text) => Ok(text),
            Err(ProviderError::Http { status, body }) => Err(ProviderError::Http { status: *status, body: body.clone() }),
            Err(ProviderError::Connect(text)) => Err(ProviderError::Connect(text.clone())),
            Err(other) => panic!("not scripted: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waits_while_the_model_loads_and_goes_again_every_second() {
        let start = tokio::time::Instant::now();
        let (again, calls) = scripted(vec![loading(), loading(), loading(), Ok("the answer")]);
        let answer = within_a_day(until_loaded(Duration::from_secs(300), loading(), again)).await;
        assert_eq!(answer.unwrap(), "the answer");
        assert_eq!(calls.get(), 4, "sent again four times after the first");
        assert_eq!(start.elapsed(), Duration::from_secs(4), "a second before each");
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_that_outlasts_the_model_wait_ends_the_request() {
        let start = tokio::time::Instant::now();
        let (again, calls) = scripted(vec![loading()]);
        let err = within_a_day(until_loaded(Duration::from_secs(300), loading(), again)).await.unwrap_err();
        let ProviderError::Silent(text) = &err else { panic!("{err:?}") };
        assert!(text.starts_with("the native model was still loading after 5 minutes; the request was dropped."), "{text}");
        assert_eq!(start.elapsed(), Duration::from_secs(300), "at the wait's end, not before and not after");
        assert_eq!(calls.get(), 300);
        assert!(!super::super::retriable(&err), "and it is not tried again");
    }

    #[tokio::test(start_paused = true)]
    async fn any_other_failure_keeps_the_presets_rule() {
        // An answer at once: nothing is sent again.
        let (again, calls) = scripted(vec![Ok("never")]);
        assert_eq!(within_a_day(until_loaded(Duration::from_secs(300), Ok("at once"), again)).await.unwrap(), "at once");
        assert_eq!(calls.get(), 0);

        // A server that could not be reached is tried once more after a second, and then said.
        let refused = || Err(ProviderError::Connect("could not connect to 127.0.0.1:8091".into()));
        let start = tokio::time::Instant::now();
        let (again, calls) = scripted(vec![refused()]);
        let err = within_a_day(until_loaded(Duration::from_secs(300), refused(), again)).await.unwrap_err();
        assert!(matches!(err, ProviderError::Connect(_)), "{err:?}");
        assert_eq!((calls.get(), start.elapsed()), (1, Duration::from_secs(1)));

        // Refused, and then loading: the server came up meanwhile, and the request waits for the model.
        let (again, calls) = scripted(vec![loading(), loading(), Ok("loaded")]);
        assert_eq!(within_a_day(until_loaded(Duration::from_secs(300), refused(), again)).await.unwrap(), "loaded");
        assert_eq!(calls.get(), 3);

        // A 503 that is no load is a 5xx like any other: once more, then said.
        let busy = || Err(ProviderError::Http { status: 503, body: "no slot available".into() });
        let (again, calls) = scripted(vec![busy()]);
        let err = within_a_day(until_loaded(Duration::from_secs(300), busy(), again)).await.unwrap_err();
        assert!(matches!(err, ProviderError::Http { status: 503, .. }), "{err:?}");
        assert_eq!(calls.get(), 1);

        // A 4xx is the server's answer and is not sent again.
        let over = || Err(ProviderError::Http { status: 400, body: "exceed_context_size_error".into() });
        let (again, calls) = scripted(vec![Ok("never")]);
        let err = within_a_day(until_loaded(Duration::from_secs(300), over(), again)).await.unwrap_err();
        assert!(matches!(err, ProviderError::Http { status: 400, .. }), "{err:?}");
        assert_eq!(calls.get(), 0);
    }

    /// The server's own answer to a request of 6,010 tokens with a model
    /// loaded at 2,048 (llama.cpp 1.0.0, read from a run).
    const OVER: &str = r#"{"error":{"code":400,"message":"request (6010 tokens) exceeds the available context size (2048 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":6010,"n_ctx":2048}}"#;

    #[test]
    fn a_request_over_the_context_window_is_said_with_both_figures_and_the_two_ways_out() {
        assert_eq!(
            over_the_window(OVER).as_deref(),
            Some("the request is 6010 tokens and the native model is loaded with a context window of 2048. /native context <tokens> loads it with a larger one, which takes more memory; or continue in a new session (/new <name>)")
        );
        // Without its figures the answer is still said.
        let bare = over_the_window(r#"{"error":{"type":"exceed_context_size_error","n_ctx":"many"}}"#).unwrap();
        assert!(bare.starts_with("the request is longer than the context window the native model is loaded with. /native context"), "{bare}");
        for other in [
            "",
            "request too long",
            r#"{"error":{"code":400,"message":"tools param requires --jinja flag","type":"invalid_request_error"}}"#,
            r#"{"error":"exceed_context_size_error"}"#,
            r#"{"type":"exceed_context_size_error"}"#,
        ] {
            assert_eq!(over_the_window(other), None, "{other}");
        }

        // The status stays the server's, and it is what the operator reads after it.
        let said = in_the_brains_words(ProviderError::Http { status: 400, body: OVER.into() });
        assert!(said.to_string().starts_with("HTTP 400: the request is 6010 tokens and the native model is loaded with a context window of 2048."), "{said}");
        assert!(!super::super::retriable(&said), "the same request meets the same answer");
        // Any other failure is left as it is.
        let other = in_the_brains_words(ProviderError::Http { status: 400, body: "tools param requires --jinja flag".into() });
        assert_eq!(other.to_string(), "HTTP 400: tools param requires --jinja flag");
        let wrong_status = in_the_brains_words(ProviderError::Http { status: 500, body: OVER.into() });
        assert!(matches!(&wrong_status, ProviderError::Http { status: 500, body } if body == OVER), "{wrong_status:?}");
        let silent = in_the_brains_words(ProviderError::Silent("nothing for 5 minutes".into()));
        assert!(matches!(&silent, ProviderError::Silent(text) if text == "nothing for 5 minutes"), "{silent:?}");
    }

    /// A day on the paused clock around a wait under test: a wait that has
    /// lost its bound is at this deadline at once, where it would never end.
    async fn within_a_day<T>(wait: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(24 * 3600), wait).await.expect("the wait ends by its own bound")
    }
}
