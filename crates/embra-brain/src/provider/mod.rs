//! Pluggable LLM provider abstraction.
//!
//! The Brain's outbound LLM surface is fronted by [`LlmProvider`]. Each
//! provider implementation owns its wire types, request body construction,
//! streaming parser, and tool schema translator. The loop driver in
//! `grpc_service.rs` consumes only the neutral IR (`provider::ir`) and
//! the trait below — it has no provider-specific code paths.
//!
//! Submodules land progressively:
//! - `ir` (this stage) — neutral IR types.
//! - `anthropic` (Stage 2) — refactored from `crate::brain`.
//! - `gemini` (Stages 3–6) — new.

pub mod anthropic;
pub mod gateway;
pub mod gemini;
pub mod health;
pub mod image;
pub mod ir;
pub mod openai_compat;
pub mod schema_util;
pub(crate) mod sse;

pub use ir::*;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value as JsonValue;

/// Identity of a provider. Persisted as a string in WardSONDB
/// (`config.system.api_provider`, `sessions.<name>.meta.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    Gemini,
    Ollama,
    LmStudio,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lm_studio",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "ollama" => Some(Self::Ollama),
            "lm_studio" => Some(Self::LmStudio),
            _ => None,
        }
    }

    /// The name the operator reads.
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Gemini => "Gemini",
            ProviderKind::Ollama => "Ollama",
            ProviderKind::LmStudio => "LM Studio",
        }
    }

    /// Every provider, in the order the lists name them.
    pub const ALL: [ProviderKind; 4] =
        [ProviderKind::Anthropic, ProviderKind::Gemini, ProviderKind::Ollama, ProviderKind::LmStudio];

    /// The wire this provider speaks. Every place that treats the providers
    /// by family matches on this and on nothing else (no wildcard arm, no
    /// list of stored names), so a provider added to the enum has to be
    /// placed here before anything compiles. Guard
    /// `no_place_tells_the_providers_apart_by_a_wildcard_or_a_list_of_names`.
    pub fn wire(self) -> Wire {
        match self {
            ProviderKind::Anthropic => Wire::Anthropic,
            ProviderKind::Gemini => Wire::Gemini,
            ProviderKind::Ollama => Wire::OpenAiCompat(openai_compat::OpenAiCompatPreset::Ollama),
            ProviderKind::LmStudio => Wire::OpenAiCompat(openai_compat::OpenAiCompatPreset::LmStudio),
        }
    }

    /// The stored names as the end of a sentence: `'anthropic', 'gemini',
    /// 'ollama', or 'lm_studio'`.
    pub fn named_list() -> String {
        let names: Vec<String> = Self::ALL.iter().map(|k| format!("'{}'", k.as_str())).collect();
        match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{}, or {last}", rest.join(", ")),
            Some((last, _)) => last.clone(),
            None => String::new(),
        }
    }
}

/// The wire families: Anthropic's, Gemini's, and the OpenAI-compatible one
/// with the preset that speaks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    Gemini,
    OpenAiCompat(openai_compat::OpenAiCompatPreset),
}

/// The operator's name for a provider stored as a string
/// (`sessions.<name>.meta.provider`); a string that is no provider is
/// shown as stored.
pub fn provider_label(kind: &str) -> &str {
    match ProviderKind::from_str(kind) {
        Some(known) => known.label(),
        None => kind,
    }
}

/// Per-provider tool manifest, built from the static registry where a
/// turn starts (`grpc_service.rs`). The input is the same on every turn, so
/// the bytes are. The `wire_json` is whatever shape the provider's API
/// expects in the request body.
pub struct ToolManifest {
    /// Pre-serialized JSON ready to splice into a request body. For
    /// Anthropic this is `[{name, description, input_schema}, ...]`; for
    /// Gemini it is `[{functionDeclarations: [...]}]`.
    pub wire_json: JsonValue,
}

/// The system prompt, and the session it is sent for. The text is stable
/// across turns (no per-turn state injection), which is what prompt
/// caching — Anthropic's and Gemini's — depends on. Gemini's cache
/// manager derives its staleness fingerprint from this text, the tool
/// manifest and the model (`gemini::cache::compute_fingerprint`).
pub struct SystemPromptBundle {
    pub text: String,
    /// Active session name. The Anthropic provider ignores this; the
    /// Gemini provider uses it to scope its Context Cache (switching
    /// sessions invalidates and recreates the cache).
    pub session_name: String,
}

/// One event from a streaming turn. `Complete` carries the assembled
/// neutral-IR turn — the loop driver consumes that and ignores the
/// per-block deltas (which are forwarded to the TUI for live UX).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// User-visible text delta (drives the TUI's typing feel).
    TextDelta(String),
    /// Reasoning / thinking delta. Routed to the live expression-panel
    /// surface for operator visibility; gated by `LlmRequestOptions::
    /// include_reasoning` per turn.
    ///
    /// Privacy contract — this variant MUST NOT be appended to text
    /// accumulators (`full_response` / `accum_text`), MUST NOT be
    /// persisted to session history, and MUST NOT be replayed to the
    /// model as Text blocks on subsequent turns. Provider signatures
    /// (Anthropic `signature_delta`, Gemini `thoughtSignature`) ride
    /// `Block::ProviderOpaque` for IR round-trip and are NOT carried
    /// here. See `grpc_service.rs::stream_brain_to_grpc` for the
    /// load-bearing privacy guard.
    ReasoningDelta(String),
    /// One block in the assistant turn finalized. Carries no payload
    /// here — the assembled blocks come back together in `Complete`.
    BlockComplete,
    /// Terminal event — turn assembled, ready for the loop driver.
    Complete(AssistantTurn),
    /// Provider-side error (4xx, 5xx, decode, network). Surfaced to
    /// the operator; non-fatal for the gRPC stream.
    Error(String),
}

/// Outcome of a key-validation probe. Drives wizard UX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationResult {
    Valid,
    InvalidKey,
    /// 403 — key is shaped correctly but not authorized (often missing
    /// billing on the Gemini side).
    Forbidden,
    /// 5xx, network, timeout. Wizard re-prompts.
    NetworkError,
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// The server could not be reached: refused, unreachable, or not there
    /// within the connect bound. Nothing was sent, so a provider may try
    /// once more. The text is the whole sentence the operator reads.
    #[error("{0}")]
    Connect(String),
    /// The server was reached and sent nothing for the whole wait the
    /// provider allows. The request was dropped; it is never tried again,
    /// since a server that takes this long would take as long once more.
    /// The text is the whole sentence the operator reads.
    #[error("{0}")]
    Silent(String),
    #[error("network: {0}")]
    Network(String),
    #[error("decode: {0}")]
    Decode(String),
}

/// Per-turn request-shaping knobs the brain reads from `SystemConfig`
/// and threads through `stream_turn`. Additive struct — new fields are
/// `Default`-derived so older provider impls compile through additions.
#[derive(Debug, Clone, Copy, Default)]
pub struct LlmRequestOptions {
    /// When `true`, the provider's request body opts in to reasoning /
    /// thought streaming where applicable (Anthropic `display: summarized`,
    /// Gemini `includeThoughts: true`). When `false`, providers omit
    /// those fields and suppress emission of `StreamEvent::ReasoningDelta`
    /// even if the model returns reasoning unsolicited (belt-and-
    /// suspenders).
    pub include_reasoning: bool,
}

/// Outbound LLM surface. One impl per provider.
///
/// `stream_turn` is the hot path — it consumes the full turn-history,
/// system prompt, and tool manifest, and returns a stream of
/// [`StreamEvent`]s ending in `Complete(AssistantTurn)`.
///
/// `build_tool_manifest` is called where a turn starts, with the
/// descriptors of the static registry. The returned manifest is reused
/// across every `stream_turn` call of that turn's tool loop.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Human-readable model identifier for the status bar.
    /// Anthropic: `"opus-5"`. Gemini: `"gemini-3.1-pro"` or `"gemini-4-argon"`.
    fn display_name(&self) -> &str;

    /// Which provider this is. Test-only: the running brain always knows
    /// the kind from config (it picked the provider by it), while the
    /// tests of the provider factory need it to tell what was built —
    /// `display_name()` cannot tell Ollama from LM Studio.
    #[cfg(test)]
    fn kind(&self) -> ProviderKind;

    /// The base every URL of this provider is built on. Test-only: the
    /// tests of the provider factory read it to tell a direct provider from
    /// one routed through the provider gateway.
    #[cfg(test)]
    fn base_url(&self) -> &str;

    /// The model wait this provider's calls run under, for the providers
    /// that have one (the two local presets). Test-only: the tests of the
    /// provider factory read it to see the stored wait arrive.
    #[cfg(test)]
    fn model_wait(&self) -> Option<std::time::Duration> {
        None
    }

    /// Probe the provider's model-listing endpoint with the given key.
    /// Called by the config wizard; never on the hot path.
    async fn validate_key(&self, key: &str) -> ValidationResult;

    /// Stream one assistant turn. Returns a `BoxStream` whose terminal
    /// event is `StreamEvent::Complete(AssistantTurn)`.
    async fn stream_turn(
        &self,
        messages: &[ApiMessage],
        system: &SystemPromptBundle,
        tools: &ToolManifest,
        options: LlmRequestOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError>;

    /// Translate the registry's typed-args descriptors into the
    /// provider's request-body tool shape. Registry descriptors only: a
    /// dynamic Guardian tool is none and never reaches a manifest.
    fn build_tool_manifest(
        &self,
        descriptors: &[&'static crate::tools::registry::ToolDescriptor],
    ) -> ToolManifest;
}

#[cfg(test)]
mod tests {
    use super::openai_compat::OpenAiCompatPreset;
    use super::*;

    /// One stored name, one label and one wire for each provider, and the
    /// preset a provider is read both ways.
    #[test]
    fn every_provider_has_one_stored_name_one_label_and_one_wire() {
        // A provider added to the enum stops this match from compiling: it
        // gets its place here, in `ALL` and in `wire`.
        let place = |kind: ProviderKind| match kind {
            ProviderKind::Anthropic => 0,
            ProviderKind::Gemini => 1,
            ProviderKind::Ollama => 2,
            ProviderKind::LmStudio => 3,
        };
        assert_eq!(ProviderKind::ALL.len(), 4, "as many as the match above has arms");
        let (mut names, mut labels) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        for (at, kind) in ProviderKind::ALL.into_iter().enumerate() {
            assert_eq!(place(kind), at, "{kind:?}");
            assert_eq!(ProviderKind::from_str(kind.as_str()), Some(kind), "{kind:?}");
            assert!(names.insert(kind.as_str()), "{kind:?} shares a stored name");
            assert!(labels.insert(kind.label()), "{kind:?} shares a label");
            assert_eq!(provider_label(kind.as_str()), kind.label());
            match kind.wire() {
                Wire::OpenAiCompat(preset) => {
                    assert_eq!(preset.as_kind(), kind);
                    assert_eq!(OpenAiCompatPreset::of(kind), Some(preset));
                    assert!(OpenAiCompatPreset::ALL.contains(&preset), "{preset:?}");
                }
                Wire::Anthropic | Wire::Gemini => assert_eq!(OpenAiCompatPreset::of(kind), None, "{kind:?}"),
            }
        }
        for preset in OpenAiCompatPreset::ALL {
            assert_eq!(preset.as_kind().wire(), Wire::OpenAiCompat(preset));
        }
        assert_eq!(ProviderKind::from_str("openai"), None);
        assert_eq!(provider_label("openai"), "openai", "a name that is no provider is shown as stored");
    }

    /// The sentence a mistyped provider gets ends with this list.
    #[test]
    fn the_named_list_is_the_stored_names_in_order() {
        assert_eq!(ProviderKind::named_list(), "'anthropic', 'gemini', 'ollama', or 'lm_studio'");
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Whether an arm's first words name a provider: a kind, a preset, a
    /// wire (behind `Some(` and a path, or bare) or a stored name.
    fn names_a_provider(arm: &str) -> bool {
        let arm = arm.strip_prefix("Some(").unwrap_or(arm);
        let bare = arm.trim_start_matches(|c: char| c.is_ascii_lowercase() || c == '_' || c == ':');
        ["ProviderKind::", "OpenAiCompatPreset::", "Wire::"].iter().any(|set| bare.starts_with(set))
            || ProviderKind::ALL.iter().any(|kind| arm.starts_with(&format!("\"{}\"", kind.as_str())))
    }

    /// The lines of `source` where an arm that names a provider has a
    /// wildcard or a bare binding for a sibling (`_ => ..`, `other => ..`):
    /// the arm a provider added later falls into without anyone deciding.
    /// Siblings are the arms at the same indentation in the same block.
    fn wildcards_beside_a_provider_arm(source: &str) -> Vec<usize> {
        let lines: Vec<&str> = source.lines().collect();
        let indent = |line: &str| line.len() - line.trim_start().len();
        let catches_the_rest = |line: &str| {
            line.trim().split_once("=>").is_some_and(|(pattern, _)| {
                let pattern = pattern.trim();
                pattern.starts_with("_ if ")
                    || (!pattern.is_empty()
                        && pattern.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            })
        };
        let mut found = std::collections::BTreeSet::new();
        for (at, line) in lines.iter().enumerate() {
            if !line.contains("=>") || !names_a_provider(line.trim()) {
                continue;
            }
            let depth = indent(line);
            let in_block = |other: &str| other.trim().is_empty() || indent(other) >= depth;
            let before = lines[..at].iter().rev().take_while(|other| in_block(other)).count();
            let after = lines[at + 1..].iter().take_while(|other| in_block(other)).count();
            for (sibling, other) in lines.iter().enumerate().take(at + after + 1).skip(at - before) {
                if indent(other) == depth && catches_the_rest(other) {
                    found.insert(sibling + 1);
                }
            }
        }
        found.into_iter().collect()
    }

    /// No place in the brain tells the providers apart by a wildcard arm or
    /// by a list of stored names: three wildcards once meant LM Studio for
    /// every preset that was not Ollama, and a provider added to the enum
    /// would have been LM Studio there without a compile error. A family
    /// decision matches on `ProviderKind::wire()`, a preset on its own
    /// variants, and every arm names what it is for.
    ///
    /// Not read: this file (the names, the labels and the wire are defined
    /// here), `provider/image/` (the image provider's own set), `migrations/`
    /// (a step reads what was stored when it was written), and tests.
    #[test]
    fn no_place_tells_the_providers_apart_by_a_wildcard_or_a_list_of_names() {
        // The scan finds the shapes it is for, and leaves a match alone that
        // names every arm.
        let preset_by_wildcard = "    let preset = match kind {\n        ProviderKind::Ollama => A,\n        _ => B,\n    };\n";
        assert_eq!(wildcards_beside_a_provider_arm(preset_by_wildcard), vec![3]);
        let name_by_wildcard = "match provider {\n    \"gemini\" => a(),\n\n    // the rest\n    _ => b(),\n}\n";
        assert_eq!(wildcards_beside_a_provider_arm(name_by_wildcard), vec![5]);
        let by_binding = "match active {\n    Some(crate::provider::Wire::Gemini) => a(),\n    cloud => b(cloud),\n}\n";
        assert_eq!(wildcards_beside_a_provider_arm(by_binding), vec![3]);
        let guarded = "match kind {\n    ProviderKind::Gemini => a(),\n    _ if x => b(),\n    ProviderKind::Anthropic => c(),\n}\n";
        assert_eq!(wildcards_beside_a_provider_arm(guarded), vec![3]);
        let named = "match kind.wire() {\n    Wire::Gemini => {\n        match other {\n            _ => a(),\n        }\n    }\n    Wire::Anthropic | Wire::OpenAiCompat(_) => b(),\n}\nmatch n {\n    _ => c(),\n}\n";
        assert_eq!(wildcards_beside_a_provider_arm(named), Vec::<usize>::new());

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        files.sort();
        // Two stored names offered as one pattern, in either order.
        let mut lists = Vec::new();
        for a in ProviderKind::ALL {
            for b in ProviderKind::ALL {
                if a != b {
                    lists.push(format!("\"{}\" | \"{}\"", a.as_str(), b.as_str()));
                }
            }
        }
        let (mut read, mut hits) = (0usize, Vec::new());
        for file in files {
            let name = file.strip_prefix(&src).unwrap().display().to_string();
            let test_file = name.ends_with("tests.rs");
            if name == "provider/mod.rs" || name.starts_with("provider/image/") || name.starts_with("migrations/") || test_file {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            // What stands above a file's first test module.
            let production = text.split("\n#[cfg(test)]\n").next().unwrap_or("");
            read += 1;
            for line in wildcards_beside_a_provider_arm(production) {
                hits.push(format!("src/{name}:{line}: a wildcard arm beside an arm that names a provider"));
            }
            for (n, line) in production.lines().enumerate() {
                if lists.iter().any(|list| line.contains(list.as_str())) {
                    hits.push(format!("src/{name}:{}: two stored names as one pattern", n + 1));
                }
            }
        }
        assert!(read > 100, "the brain's sources were read: {read} files");
        assert!(
            hits.is_empty(),
            "match on ProviderKind::wire() or on the preset, with every arm named:\n{}",
            hits.join("\n")
        );
    }
}
