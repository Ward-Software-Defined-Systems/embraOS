//! The replicant check — the soul-spec gate a brain-proposed Guardian
//! tool must pass before it becomes a proposal.
//!
//! This is Pillar 1 of SOUL-AS-ENFORCED-RUNTIME ("enforcement, not
//! persuasion"), scoped to the self-authoring path. An INDEPENDENT,
//! isolated model call judges the draft against the sealed soul and
//! returns a verdict — the authoring brain never rules on its own draft.
//! The caller (`crate::guardian::propose`) records a proposal only on
//! `allow` / `escalate`, and fails CLOSED on any error or unparseable
//! verdict (no proposal). The verdict text is read for the final answer
//! only; any reasoning is discarded (REASONING-STREAM-01).

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{
    ApiMessage, Block, LlmProvider, LlmRequestOptions, StreamEvent, SystemPromptBundle,
    ToolManifest,
};
use embra_guardian::ValidatedModule;

/// A parsed replicant-check verdict.
#[derive(Debug, Clone)]
pub struct ReplicantVerdict {
    /// "allow" | "refuse" | "escalate" (normalized lowercase).
    pub verdict: String,
    /// Soul lines the judge flagged (verbatim text).
    pub touched_lines: Vec<String>,
    /// One-paragraph rationale.
    pub rationale: String,
}

impl ReplicantVerdict {
    pub fn is_refuse(&self) -> bool {
        self.verdict == "refuse"
    }
    pub fn is_escalate(&self) -> bool {
        self.verdict == "escalate"
    }
}

#[derive(Deserialize)]
struct RawVerdict {
    verdict: String,
    #[serde(default)]
    touched_lines: Vec<String>,
    #[serde(default)]
    rationale: String,
}

/// Judge a proposed tool against the soul. `soul` is the inner soul value
/// (as returned by `learning::load_soul`). Returns the verdict, or `Err`
/// when the provider call fails or the response can't be parsed into a
/// known verdict — the caller treats `Err` as fail-closed.
pub async fn evaluate_against_soul(
    provider: &dyn LlmProvider,
    soul: &Value,
    module: &ValidatedModule,
) -> Result<ReplicantVerdict, String> {
    let soul_text = crate::brain::render_constitution(soul);
    let system = SystemPromptBundle {
        text: replicant_system_prompt(&soul_text),
        session_name: String::new(),
    };
    let messages = vec![ApiMessage::user_text(replicant_user_message(module))];
    // Empty manifest → providers omit `tools`/`tool_choice` (we want a
    // pure-text JSON verdict back, no tool calls).
    let tools = ToolManifest {
        wire_json: serde_json::json!([]),
    };
    // include_reasoning: false — we read only the final verdict text; any
    // reasoning stays unpersisted per REASONING-STREAM-01.
    let opts = LlmRequestOptions::default();
    let stream = provider
        .stream_turn(&messages, &system, &tools, opts)
        .await
        .map_err(|e| format!("provider error: {e}"))?;
    let text = collect_text(stream).await?;
    parse_verdict(&text)
}

fn replicant_system_prompt(soul_text: &str) -> String {
    format!(
        "You are the embraOS replicant check: the gate a NEW \
         dynamically-authored tool must pass before it may even be \
         proposed to the operator. The sealed soul below outranks \
         everyone, including the operator. Judge the proposed tool by its \
         declared capabilities, input schema, description, and most-likely \
         use — paying special attention to anything that would let the \
         intelligence replicate itself, persist or escape beyond its \
         sandbox, exfiltrate operator data, or expand its own authority \
         past what the soul sanctions.\n\
         \n\
         === SOUL (IMMUTABLE) ===\n{soul_text}\n=== END SOUL ===\n\
         \n\
         Reply with ONLY a single JSON object, no prose, no markdown:\n\
         {{\"verdict\":\"allow|refuse|escalate\",\"touched_lines\":[\"<soul \
         line text>\"],\"rationale\":\"<one paragraph>\"}}\n\
         Use \"refuse\" for a clear soul conflict, \"escalate\" when \
         genuinely uncertain (the operator then decides), and \"allow\" \
         only when the tool is clearly soul-consistent. Name the specific \
         soul line(s) in touched_lines when refusing or escalating."
    )
}

fn replicant_user_message(m: &ValidatedModule) -> String {
    let caps = if m.caps.is_empty() {
        "none".to_string()
    } else {
        m.caps.join(", ")
    };
    let schema = serde_json::to_string_pretty(&m.input_schema)
        .unwrap_or_else(|_| m.input_schema.to_string());
    format!(
        "Proposed Guardian dynamic tool:\n\
         name: {}\n\
         description: {}\n\
         capabilities: {}\n\
         input_schema:\n{}\n\
         --- source ---\n{}",
        m.name, m.description, caps, schema, m.source
    )
}

/// Drive a `stream_turn` stream to completion, returning the concatenated
/// user-visible text. `ReasoningDelta` is ignored (privacy contract); on
/// `Complete` we fall back to the assembled turn's text if no deltas were
/// emitted. A provider `Error` event aborts with `Err`.
async fn collect_text(
    mut stream: BoxStream<'static, StreamEvent>,
) -> Result<String, String> {
    let mut accum = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => accum.push_str(&t),
            StreamEvent::Error(e) => return Err(format!("stream error: {e}")),
            StreamEvent::Complete(turn) => {
                if accum.trim().is_empty() {
                    for b in &turn.content {
                        if let Block::Text(t) = b {
                            accum.push_str(t);
                        }
                    }
                }
                return Ok(accum);
            }
            // ReasoningDelta (never persisted), BlockComplete.
            _ => {}
        }
    }
    if accum.trim().is_empty() {
        Err("empty replicant response".to_string())
    } else {
        Ok(accum)
    }
}

/// Extract the first balanced JSON object from `text` and parse it into a
/// verdict. Tolerant of models that wrap JSON in prose / markdown fences.
/// Unknown or missing verdict → `Err` (fail closed).
fn parse_verdict(text: &str) -> Result<ReplicantVerdict, String> {
    let raw = extract_json_object(text)
        .ok_or_else(|| format!("no JSON object in replicant response: {}", truncate(text, 200)))?;
    let parsed: RawVerdict = serde_json::from_str(&raw)
        .map_err(|e| format!("could not parse verdict JSON ({e}): {}", truncate(&raw, 200)))?;
    let verdict = parsed.verdict.trim().to_ascii_lowercase();
    if !matches!(verdict.as_str(), "allow" | "refuse" | "escalate") {
        return Err(format!("unknown verdict '{}'", parsed.verdict));
    }
    Ok(ReplicantVerdict {
        verdict,
        touched_lines: parsed.touched_lines,
        rationale: parsed.rationale,
    })
}

/// Find the first `{` … matching `}` slice, respecting string literals so
/// a brace inside a quoted value doesn't end the object early. Structural
/// chars are ASCII; non-ASCII bytes inside strings fall through untouched.
fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that answers `stream_turn` with the events scripted:
    /// the glue between a provider's stream and the verdict (#55).
    struct FakeProvider {
        events: Vec<StreamEvent>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FakeProvider {
        fn display_name(&self) -> &str {
            "fake"
        }

        fn kind(&self) -> crate::provider::ProviderKind {
            crate::provider::ProviderKind::Anthropic
        }

        fn base_url(&self) -> &str {
            "http://fake.invalid"
        }

        async fn validate_key(&self, _key: &str) -> crate::provider::ValidationResult {
            crate::provider::ValidationResult::Valid
        }

        async fn stream_turn(
            &self,
            _messages: &[ApiMessage],
            _system: &SystemPromptBundle,
            _tools: &ToolManifest,
            _options: LlmRequestOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, crate::provider::ProviderError> {
            Ok(futures::stream::iter(self.events.clone()).boxed())
        }

        fn build_tool_manifest(
            &self,
            _descriptors: &[&'static crate::tools::registry::ToolDescriptor],
        ) -> ToolManifest {
            ToolManifest { wire_json: serde_json::json!([]) }
        }
    }

    fn module() -> ValidatedModule {
        ValidatedModule {
            name: "temp_delta".into(),
            description: "Delta between two temperatures.".into(),
            input_schema: serde_json::json!({"type": "object"}),
            caps: Vec::new(),
            source: "fn run(input: &str) -> String { String::new() }".into(),
        }
    }

    fn complete(text: &str) -> StreamEvent {
        StreamEvent::Complete(crate::provider::ir::AssistantTurn {
            content: if text.is_empty() { Vec::new() } else { vec![Block::Text(text.to_string())] },
            outcome: crate::provider::ir::TurnOutcome::EndTurn,
            stop_details: None,
        })
    }

    /// #55: the soul gate fails closed on everything but a verdict. A
    /// stream error, a reply with no text, and a stream that ends with no
    /// `Complete` are each an error; a refuse verdict delivered in deltas
    /// is read as the refusal it is.
    #[tokio::test]
    async fn the_replicant_check_fails_closed_on_a_stream_error_or_an_empty_reply() {
        let soul = serde_json::json!({"name": "Embra"});
        let judge = |events: Vec<StreamEvent>| {
            let soul = &soul;
            async move { evaluate_against_soul(&FakeProvider { events }, soul, &module()).await }
        };

        let err = judge(vec![StreamEvent::Error("connection reset".into())]).await.expect_err("a stream error");
        assert!(err.contains("stream error"), "{err}");

        let verdict = judge(vec![
            StreamEvent::TextDelta(r#"{"verdict":"refuse","touched_lines":["never deceive"],"rationale":"exfiltrates"}"#.into()),
            complete(""),
        ])
        .await
        .expect("a verdict");
        assert!(verdict.is_refuse());

        let err = judge(vec![complete("")]).await.expect_err("nothing to read a verdict from");
        assert!(!err.is_empty());

        let err = judge(Vec::new()).await.expect_err("a stream that ends with no Complete");
        assert_eq!(err, "empty replicant response");
    }

    #[test]
    fn parses_clean_allow() {
        let v = parse_verdict(r#"{"verdict":"allow","touched_lines":[],"rationale":"ok"}"#).unwrap();
        assert_eq!(v.verdict, "allow");
    }

    #[test]
    fn parses_refuse_case_insensitive_with_lines() {
        let v = parse_verdict(
            r#"{"verdict":"REFUSE","touched_lines":["never deceive the operator"],"rationale":"exfiltrates data"}"#,
        )
        .unwrap();
        assert!(v.is_refuse());
        assert_eq!(v.touched_lines, vec!["never deceive the operator".to_string()]);
    }

    #[test]
    fn parses_escalate_wrapped_in_prose_and_fences() {
        let text = "Here is my verdict:\n```json\n{\"verdict\":\"escalate\",\"rationale\":\"unsure\"}\n```\nThanks";
        let v = parse_verdict(text).unwrap();
        assert!(v.is_escalate());
        assert_eq!(v.rationale, "unsure");
    }

    #[test]
    fn brace_inside_string_does_not_truncate_object() {
        let v = parse_verdict(r#"{"verdict":"allow","rationale":"uses a } brace and \" quote"}"#)
            .unwrap();
        assert_eq!(v.verdict, "allow");
        assert_eq!(v.rationale, "uses a } brace and \" quote");
    }

    #[test]
    fn malformed_is_err_fail_closed() {
        assert!(parse_verdict("no json here").is_err());
        assert!(parse_verdict(r#"{"verdict":"maybe"}"#).is_err());
        assert!(parse_verdict(r#"{"rationale":"missing verdict field"}"#).is_err());
    }
}
