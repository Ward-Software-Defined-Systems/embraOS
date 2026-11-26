//! What a native model's request differs in (experimental). The model
//! server is llama.cpp's and the models are of a few families
//! ([`Dialect`]): the request is the presets' own, with these differences.
//!
//! - Thinking is switched on every request (`chat_template_kwargs`): the
//!   server's own default is on, and a request that says nothing thinks. It
//!   is on unless the operator's `/effort` is the family's word for off. A
//!   `reasoning_effort` is sent only to a family whose template has levels,
//!   and only a word of that template's own. A family whose template has no
//!   switch is asked for its least level in place of off.
//! - The sampling is the model card's for the thinking state, sent with the
//!   request: the server's defaults are its own, not the model's.
//! - The reasoning of an earlier step of the turn goes back as
//!   `reasoning_content`, the name the server reads. The model's template
//!   then renders the step as the model wrote it, and the server continues
//!   from what it has: measured with one tool call, 369 tokens were reused
//!   and 36 read, against 289 and 54 with `reasoning`, which the server
//!   ignores. (No reasoning is kept from one turn to the next, which is
//!   what the model card asks for.)
//! - An image goes as an image when the model server was started with the
//!   model's image file (embrad's status says so), as the presets send one.
//!   A server without the file answers an image with an error, so there an
//!   image is a line that says one was attached and how the file is got.
//! - While the server reads the model's file it answers 503: the request
//!   waits and is sent again, as long as the model wait allows.
//! - The server is asked how far it has read the prompt (`return_progress`):
//!   while it reads, every piece of the prompt is a frame of the stream
//!   with the count so far. A call that is ended from outside knows what
//!   was read (`native_ask` at its limit); a read that takes minutes is no
//!   silence either.
//! - A request longer than the context window the model was loaded with is
//!   answered 400, with both figures: the operator reads them in a sentence.

use std::time::Duration;

use serde_json::json;

use embra_boot::native_model::{self as contract, Family};

use super::wire::{OpenAIChatRequest, OpenAIMessage, Sampling, UserContent, UserPart};
use crate::provider::ProviderError;

/// What a family of models takes in a request, from its own model card.
pub(crate) struct Dialect {
    /// The card's sampling for "thinking mode".
    pub(crate) thinking: Sampling,
    /// The card's sampling for "instruct (or non-thinking) mode".
    pub(crate) direct: Sampling,
    /// The levels of effort the family's chat template takes when it
    /// thinks, least first; none when thinking is on or off and nothing
    /// between. A word that is no level is answered HTTP 500 by the server
    /// (the template raises "Unexpected reasoning effort"), so no other
    /// word is ever sent.
    pub(crate) levels: &'static [&'static str],
    /// The level the template takes when none is named.
    pub(crate) own_level: Option<&'static str>,
    /// The stored `/effort` word that switches thinking off. Any other, and
    /// none, leaves it on (William, 2026-11-22: on by default).
    pub(crate) off: &'static str,
    /// The level a family is asked for when thinking is off and its template
    /// has no switch: the template's least, at which the model still thinks
    /// a little. None for a family that can stop thinking.
    pub(crate) least: Option<&'static str>,
}

/// Qwen3.5's model card (read 2026-11-22, the same for the 4B and the 9B;
/// `repetition_penalty=1.0` is the server's default and is not sent). The
/// word for off is the level `low`: the family has no levels, and the one
/// set of levels stays the configuration's.
const QWEN35: Dialect = Dialect {
    thinking: Sampling { temperature: 1.0, top_p: 0.95, top_k: 20, min_p: 0.0, presence_penalty: 1.5 },
    direct: Sampling { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0.0, presence_penalty: 1.5 },
    levels: &[],
    own_level: None,
    off: "low",
    least: None,
};

/// Qwen3.8's model card and chat template (read 2026-11-23): thinking is
/// sampled without a presence penalty, and its depth is one of three
/// levels, `xhigh` when none is named.
const QWEN38: Dialect = Dialect {
    thinking: Sampling { temperature: 1.0, top_p: 0.95, top_k: 20, min_p: 0.0, presence_penalty: 0.0 },
    direct: Sampling { temperature: 0.7, top_p: 0.8, top_k: 20, min_p: 0.0, presence_penalty: 1.5 },
    levels: &["low", "medium", "xhigh"],
    own_level: Some("xhigh"),
    off: "off",
    least: None,
};

/// Inkling 2's model card and chat template (read 2026-11-25, and tried on
/// the build server with llama.cpp v1.1.0). The template writes the depth of
/// thinking as a number from a word: `none` 0.0, `minimal` 0.1, `low` 0.2,
/// `medium` 0.7, `high` 0.9, `xhigh` and `max` 0.99; `high` when none is
/// named. It has no switch (`enable_thinking` is read by nothing in it), and
/// the card says of the least: "Setting reasoning effort to its minimum does
/// not fully disable reasoning". So off is the level `none`, and the model
/// thinks a little even then. Any other word is answered HTTP 500 by the
/// template ("Unknown reasoning_effort"). The card's sampling is a
/// temperature (1.0, its generation configuration) and nothing else: the
/// rest is the model server's own, which is what the model was tried with
/// in two sessions it led.
const INKLING2: Dialect = Dialect {
    thinking: Sampling { temperature: 1.0, top_p: 0.95, top_k: 40, min_p: 0.05, presence_penalty: 0.0 },
    direct: Sampling { temperature: 1.0, top_p: 0.95, top_k: 40, min_p: 0.05, presence_penalty: 0.0 },
    levels: &["minimal", "low", "medium", "high", "xhigh", "max"],
    own_level: Some("high"),
    off: "none",
    least: Some("none"),
};

/// What a family takes.
pub(crate) fn dialect(family: Family) -> &'static Dialect {
    match family {
        Family::Qwen35 => &QWEN35,
        Family::Qwen38 => &QWEN38,
        Family::Inkling2 => &INKLING2,
    }
}

/// The family of the catalogue's model of this name. A name that is none
/// of the catalogue's is shaped as the first family was: no request is
/// lost to a name.
pub(crate) fn family_of(model_id: &str) -> Family {
    contract::by_id(model_id).map(|model| model.family).unwrap_or(Family::Qwen35)
}

/// How a request thinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Thinks {
    Off,
    /// On, at a level of the family when the operator set one of them.
    On(Option<&'static str>),
}

/// What stands where an image was, for a model server that was started
/// without the model's image file.
pub(super) const IMAGE_LINE: &str =
    "[An image was attached here. It was not sent: the native model is loaded without its image file, which an install of the model brings (/native install).]";

/// How often a request is sent again while the server loads its model.
pub(super) const LOADING_POLL: Duration = Duration::from_secs(1);

/// How a model of `family` thinks, by the operator's `/effort` for it: off
/// by the family's word for it (and by `off` in every family), at a level
/// when the word is one of the family's, and on without one otherwise.
pub(crate) fn thinking(family: Family, effort: Option<&str>) -> Thinks {
    let dialect = dialect(family);
    match effort.map(str::trim) {
        Some(word) if word == dialect.off || word == "off" => Thinks::Off,
        Some(word) => Thinks::On(dialect.levels.iter().copied().find(|level| *level == word)),
        None => Thinks::On(None),
    }
}

/// Whether a native model thinks, by the operator's `/effort` for it.
pub(crate) fn thinks(family: Family, effort: Option<&str>) -> bool {
    thinking(family, effort) != Thinks::Off
}

/// Give a request a native model's shape: the thinking switch, the card's
/// sampling for it, the level of effort when the family has levels and the
/// operator named one (or, with thinking off, the least level of a family
/// that has no switch), and the reading progress asked for.
pub(super) fn shape(body: &mut OpenAIChatRequest, family: Family, effort: Option<&str>) {
    let dialect = dialect(family);
    let thinks = thinking(family, effort);
    body.reasoning_effort = match thinks {
        Thinks::On(Some(level)) => Some(level.to_string()),
        Thinks::On(None) => None,
        Thinks::Off => dialect.least.map(str::to_string),
    };
    body.return_progress = Some(true);
    body.chat_template_kwargs = Some(json!({ "enable_thinking": thinks != Thinks::Off }));
    body.sampling = Some(if thinks == Thinks::Off { dialect.direct } else { dialect.thinking });
}

/// The messages as a native model's server takes them: an earlier step's
/// reasoning under the name it reads, and an image as it is when the server
/// reads images, or as a line of text when it does not.
pub(super) fn messages(messages: Vec<OpenAIMessage>, images: bool) -> Vec<OpenAIMessage> {
    messages
        .into_iter()
        .map(|message| match message {
            OpenAIMessage::Assistant { content, tool_calls, reasoning, reasoning_content } => OpenAIMessage::Assistant {
                content,
                tool_calls,
                reasoning: None,
                reasoning_content: reasoning.or(reasoning_content),
            },
            OpenAIMessage::User { content: UserContent::Parts(parts) } if !images => {
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
pub(crate) fn is_loading(body: &str) -> bool {
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
    let length = super::span_words(waited);
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

    /// The first family thinks or it does not: the level `low` is its
    /// stored word for off, and no level is ever sent.
    #[test]
    fn thinking_is_on_unless_the_effort_is_low() {
        for on in [None, Some("medium"), Some("high"), Some("xhigh"), Some("max"), Some("")] {
            assert_eq!(thinking(Family::Qwen35, on), Thinks::On(None), "{on:?}");
            assert!(thinks(Family::Qwen35, on));
        }
        for off in ["low", " low ", "off"] {
            assert_eq!(thinking(Family::Qwen35, Some(off)), Thinks::Off, "{off:?}");
        }
        // Every model of the catalogue has a family the brain knows.
        for model in &contract::CATALOGUE {
            assert_eq!(family_of(model.id), model.family);
        }
        assert_eq!(family_of("not-a-model"), Family::Qwen35);
    }

    /// The second family has three levels: one of them is sent as it is,
    /// any other word is none (the template would refuse the request), and
    /// `off` alone switches thinking off, since `low` is a level here.
    #[test]
    fn a_family_with_levels_sends_its_own_and_no_other_word() {
        let d = dialect(Family::Qwen38);
        assert_eq!((d.levels, d.own_level, d.off), (&["low", "medium", "xhigh"][..], Some("xhigh"), "off"));
        for level in ["low", "medium", "xhigh"] {
            assert_eq!(thinking(Family::Qwen38, Some(level)), Thinks::On(Some(level)));
        }
        for no_level in [None, Some("high"), Some("max"), Some(""), Some("XHIGH")] {
            assert_eq!(thinking(Family::Qwen38, no_level), Thinks::On(None), "{no_level:?}");
        }
        assert_eq!(thinking(Family::Qwen38, Some("off")), Thinks::Off);

        let shaped = |effort: Option<&str>| {
            let mut body = request();
            body.reasoning_effort = Some("max".to_string());
            shape(&mut body, Family::Qwen38, effort);
            serde_json::to_value(&body).unwrap()
        };
        // A level: sent, with the card's sampling for thinking (no presence penalty).
        let medium = shaped(Some("medium"));
        assert_eq!(medium["reasoning_effort"], "medium");
        assert_eq!(medium["chat_template_kwargs"], json!({"enable_thinking": true}));
        assert_eq!((medium["temperature"].as_f64(), medium["top_p"].as_f64(), medium["presence_penalty"].as_f64()), (Some(1.0), Some(0.95), Some(0.0)));
        // No level, and a word that is none: thinking on, and no word in the request.
        for effort in [None, Some("high"), Some("max")] {
            let body = shaped(effort);
            assert!(body.get("reasoning_effort").is_none(), "{effort:?}: {body}");
            assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": true}));
        }
        // Off: the direct sampling, and no level.
        let off = shaped(Some("off"));
        assert!(off.get("reasoning_effort").is_none());
        assert_eq!(off["chat_template_kwargs"], json!({"enable_thinking": false}));
        assert_eq!((off["temperature"].as_f64(), off["presence_penalty"].as_f64()), (Some(0.7), Some(1.5)));
    }

    /// The third family has no switch: each of its levels is sent as it
    /// is, off is its least level and is sent as that word, no word is the
    /// template's own, and a word the template does not know never leaves.
    #[test]
    fn a_family_without_a_switch_is_asked_for_its_least_and_no_unknown_word() {
        let d = dialect(Family::Inkling2);
        assert_eq!((d.levels, d.own_level, d.off, d.least), (&["minimal", "low", "medium", "high", "xhigh", "max"][..], Some("high"), "none", Some("none")));
        // The two families before it can stop thinking: they have no least.
        assert_eq!((dialect(Family::Qwen35).least, dialect(Family::Qwen38).least), (None, None));
        for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(thinking(Family::Inkling2, Some(level)), Thinks::On(Some(level)));
        }
        for off in ["none", " none ", "off"] {
            assert_eq!(thinking(Family::Inkling2, Some(off)), Thinks::Off, "{off:?}");
        }
        for no_level in [None, Some("extreme"), Some(""), Some("HIGH"), Some("0.5")] {
            assert_eq!(thinking(Family::Inkling2, no_level), Thinks::On(None), "{no_level:?}");
        }

        let shaped = |effort: Option<&str>| {
            let mut body = request();
            body.reasoning_effort = Some("extreme".to_string());
            shape(&mut body, Family::Inkling2, effort);
            serde_json::to_value(&body).unwrap()
        };
        // Every word the template knows, and only those, as the request's `reasoning_effort`.
        let known = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
        for (stored, sent) in [("minimal", "minimal"), ("low", "low"), ("medium", "medium"), ("high", "high"), ("xhigh", "xhigh"), ("max", "max"), ("none", "none"), ("off", "none")] {
            let body = shaped(Some(stored));
            assert_eq!(body["reasoning_effort"], sent, "{stored}");
            assert!(known.contains(&body["reasoning_effort"].as_str().unwrap()));
            // The card's temperature, with the server's own sampling beside it, whatever the level.
            assert_eq!(
                (body["temperature"].as_f64(), body["top_p"].as_f64(), body["top_k"].as_u64(), body["min_p"].as_f64(), body["presence_penalty"].as_f64()),
                (Some(1.0), Some(0.95), Some(40), Some(0.05), Some(0.0)),
                "{stored}"
            );
            assert_eq!(body["return_progress"], json!(true));
        }
        // No word, and a word that is none of the template's: nothing is sent, and the template takes its own.
        for effort in [None, Some("extreme"), Some("")] {
            let body = shaped(effort);
            assert!(body.get("reasoning_effort").is_none(), "{effort:?}: {body}");
            assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": true}));
        }
        // The switch still travels (the template reads nothing of it): one shape of request for every family.
        assert_eq!(shaped(Some("none"))["chat_template_kwargs"], json!({"enable_thinking": false}));
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
            return_progress: None,
            sampling: None,
        }
    }

    #[test]
    fn a_request_says_whether_to_think_and_carries_the_cards_sampling_for_it() {
        let mut thinking = request();
        shape(&mut thinking, Family::Qwen35, Some("high"));
        let body = serde_json::to_value(&thinking).unwrap();
        assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": true}));
        assert_eq!(
            (body["temperature"].as_f64(), body["top_p"].as_f64(), body["top_k"].as_u64(), body["min_p"].as_f64(), body["presence_penalty"].as_f64()),
            (Some(1.0), Some(0.95), Some(20), Some(0.0), Some(1.5))
        );
        assert!(body.get("reasoning_effort").is_none(), "the model has no levels: {body}");
        assert_eq!(body["return_progress"], json!(true), "the server is asked how far it has read: {body}");
        assert!(body.get("sampling").is_none(), "the sampling's fields are the request's own: {body}");

        let mut direct = request();
        shape(&mut direct, Family::Qwen35, Some("low"));
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
        let out = messages_as_text(vec![
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

    /// The messages for a server that was started without the image file.
    fn messages_as_text(list: Vec<OpenAIMessage>) -> Vec<OpenAIMessage> {
        messages(list, false)
    }

    /// A server that was started with the model's image file is sent an
    /// image as the presets send one; the reasoning still goes back under
    /// the name the server reads.
    #[test]
    fn an_image_goes_as_an_image_to_a_server_that_reads_images() {
        let image = || UserPart::ImageUrl { image_url: ImageUrlRef { url: "data:image/png;base64,AAAA".into() } };
        let out = messages(
            vec![
                OpenAIMessage::User { content: UserContent::Parts(vec![image(), UserPart::Text { text: "what is this?".into() }]) },
                OpenAIMessage::Assistant { content: Some("A dot.".into()), tool_calls: None, reasoning: Some("One pixel.".into()), reasoning_content: None },
            ],
            true,
        );
        let wire = serde_json::to_value(&out).unwrap();
        assert_eq!(wire[0]["content"][0], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}));
        assert_eq!(wire[0]["content"][1], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(wire[1]["reasoning_content"], "One pixel.");
        assert!(wire[1].get("reasoning").is_none(), "{}", wire[1]);
        assert!(!wire.to_string().contains("An image was attached"), "{wire}");
    }

    #[test]
    fn an_image_is_a_line_of_text() {
        let image = || UserPart::ImageUrl { image_url: ImageUrlRef { url: "data:image/png;base64,AAAA".into() } };
        let out = messages_as_text(vec![
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
