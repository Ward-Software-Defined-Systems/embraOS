//! The service's tests, one module per concern, as they were in the one file.

use super::*;

#[cfg(test)]
mod tool_only_placeholder_tests {
    use super::render_tool_only_placeholder;

    #[test]
    fn single_tool_name() {
        assert_eq!(
            render_tool_only_placeholder(&["file_read".to_string()]),
            "[tool calls: file_read]"
        );
    }

    #[test]
    fn multiple_names_join_in_order() {
        let names = vec![
            "knowledge_query".to_string(),
            "file_read".to_string(),
            "git_status".to_string(),
        ];
        assert_eq!(
            render_tool_only_placeholder(&names),
            "[tool calls: knowledge_query, file_read, git_status]"
        );
    }

    #[test]
    fn caps_at_eight_with_ellipsis() {
        let names: Vec<String> = (0..10).map(|i| format!("tool_{}", i)).collect();
        let out = render_tool_only_placeholder(&names);
        assert!(out.starts_with("[tool calls: tool_0, tool_1, tool_2, tool_3, tool_4, tool_5, tool_6, tool_7, …]"));
    }

    #[test]
    fn exactly_eight_no_ellipsis() {
        let names: Vec<String> = (0..8).map(|i| format!("t{}", i)).collect();
        let out = render_tool_only_placeholder(&names);
        assert!(!out.contains("…"));
    }
}

#[cfg(test)]
mod transcript_segments_tests {
    use super::{final_assistant_text, turn_text, TranscriptSegments, STREAM_FAILED_MARKER};
    use crate::provider::{AssistantTurn, Block, EarlyStopReason, StreamEvent, TurnOutcome};
    use futures::stream;
    use tokio::sync::mpsc;

    #[test]
    fn single_segment_joined_is_byte_identical() {
        // No-tool turns must persist exactly as they did pre-accumulator.
        let mut segs = TranscriptSegments::default();
        let text = "A single plain answer.\n\nWith its own paragraphs.";
        segs.push(text);
        assert_eq!(segs.joined(), text);
    }

    #[test]
    fn segments_join_with_blank_line_separator() {
        let mut segs = TranscriptSegments::default();
        segs.push("a");
        segs.push("b");
        segs.push("c");
        assert_eq!(segs.joined(), "a\n\nb\n\nc");
    }

    #[test]
    fn whitespace_only_segments_are_skipped() {
        let mut segs = TranscriptSegments::default();
        segs.push("");
        segs.push("  \n");
        // Empty accumulator joins to "" so final_assistant_text's
        // placeholder chain still fires for text-less turns.
        assert_eq!(segs.joined(), "");
        let names = vec!["file_read".to_string()];
        assert_eq!(
            final_assistant_text(&segs.joined(), &names, TurnOutcome::EndTurn),
            "[tool calls: file_read]"
        );
    }

    #[test]
    fn a_failed_continuation_persists_its_marker_after_the_text() {
        // The loop pushes the marker as a segment and breaks with the
        // previous turn's outcome (ToolUse): no suffix, the marker last.
        let mut segs = TranscriptSegments::default();
        segs.push("Reading the dump now.");
        segs.push(STREAM_FAILED_MARKER);
        let persisted = final_assistant_text(&segs.joined(), &[], TurnOutcome::ToolUse);
        assert_eq!(
            persisted,
            "Reading the dump now.\n\n(turn ended by a provider error; see the error above)"
        );
    }

    #[test]
    fn final_assistant_text_over_joined_segments_suffixes_once() {
        let mut segs = TranscriptSegments::default();
        segs.push("Let me check the logs first.");
        segs.push("Here is the answer.");
        let stopped = final_assistant_text(
            &segs.joined(),
            &[],
            TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop),
        );
        assert_eq!(
            stopped,
            "Let me check the logs first.\n\nHere is the answer.\n\n(response interrupted by operator stop)"
        );
        assert_eq!(stopped.matches("(response interrupted").count(), 1);
        let refused = final_assistant_text(
            &segs.joined(),
            &[],
            TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
        );
        assert!(refused.ends_with("(response interrupted by provider refusal)"));
        assert_eq!(refused.matches("(response interrupted").count(), 1);
    }

    #[test]
    fn diagnostic_segment_appends_after_real_segments() {
        let mut segs = TranscriptSegments::default();
        segs.push("Ran the sweep.");
        segs.push("(model ended turn silently after 2 side-effectful tool calls; see `turn_trace` for details)");
        let joined = segs.joined();
        assert!(joined.starts_with("Ran the sweep.\n\n(model ended turn silently"));
    }

    /// The write-side bug regression: drive `collect_response` once per
    /// tool-loop iteration (the exact collect → turn_text → push seam the
    /// loop driver uses) and assert BOTH segments reach the persisted
    /// text, where the old single-slot `last_response_text` kept only the
    /// final iteration's.
    #[tokio::test]
    async fn multi_iteration_collect_response_feeds_all_segments() {
        let narration = "Let me check the logs first.";
        let answer = "Here is the answer.";
        let iterations: Vec<Vec<StreamEvent>> = vec![
            vec![
                StreamEvent::TextDelta(narration.to_string()),
                StreamEvent::Complete(AssistantTurn {
                    content: vec![
                        Block::Text(narration.to_string()),
                        Block::ToolCall {
                            id: "t1".to_string(),
                            name: "system_logs".to_string(),
                            args: serde_json::json!({}),
                            provider_opaque: None,
                        },
                    ],
                    outcome: TurnOutcome::ToolUse,
                    stop_details: None,
                }),
            ],
            vec![
                StreamEvent::TextDelta(answer.to_string()),
                StreamEvent::Complete(AssistantTurn {
                    content: vec![Block::Text(answer.to_string())],
                    outcome: TurnOutcome::EndTurn,
                    stop_details: None,
                }),
            ],
        ];

        let mut segs = TranscriptSegments::default();
        let mut last_response_text = String::new();
        for events in iterations {
            let (tx, mut rx) = mpsc::channel(64);
            let turn = super::collect_response(Box::pin(stream::iter(events)), &tx, "Embra", None)
                .await
                .expect("collect_response ok")
                .expect("got Complete turn");
            drop(tx);
            while rx.recv().await.is_some() {}
            last_response_text = turn_text(&turn);
            segs.push(&last_response_text);
        }

        let persisted = final_assistant_text(&segs.joined(), &[], TurnOutcome::EndTurn);
        assert_eq!(persisted, format!("{narration}\n\n{answer}"));
        // The old single-slot shape would have persisted only the final
        // iteration's text — the narration segment is the regression.
        assert_eq!(last_response_text, answer);
        assert!(persisted.contains(narration));
    }
}

#[cfg(test)]
mod turn_trace_preview_tests {
    use super::{preview_str, TURN_TRACE_PREVIEW_MAX};

    #[test]
    fn trace_preview_cap_is_2048_and_truncates_boundary_safe() {
        // Guards a silent revert to the historical 200-byte previews.
        assert_eq!(TURN_TRACE_PREVIEW_MAX, 2048);
        // '€' is 3 bytes; 2048 % 3 != 0 forces a mid-char boundary the
        // truncation must snap back from.
        let big = "€".repeat(1000); // 3000 bytes
        let preview = preview_str(&big, TURN_TRACE_PREVIEW_MAX);
        assert!(preview.ends_with('…'));
        let body = preview.strip_suffix('…').unwrap();
        assert!(body.len() <= TURN_TRACE_PREVIEW_MAX);
        assert_eq!(body.len() % 3, 0); // whole '€'s only
        // ≤-cap input passes through untouched.
        let small = "x".repeat(TURN_TRACE_PREVIEW_MAX);
        assert_eq!(preview_str(&small, TURN_TRACE_PREVIEW_MAX), small);
    }
}

#[cfg(test)]
mod cap_results_tests {
    use super::synthesize_cap_results;
    use crate::provider::ir::Block;
    use serde_json::json;

    fn tool_call(id: &str, name: &str) -> Block {
        Block::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args: json!({}),
            provider_opaque: None,
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = synthesize_cap_results(&[], "cap reached");
        assert!(out.is_empty());
    }

    #[test]
    fn ignores_non_toolcall_blocks() {
        let blocks = vec![
            Block::Text("model thinking out loud".into()),
            Block::ProviderOpaque(json!({"thinking": "..."})),
        ];
        let out = synthesize_cap_results(&blocks, "cap reached");
        assert!(out.is_empty());
    }

    #[test]
    fn synthesize_cap_results_tool_results_have_no_images() {
        // Cap-synthesized results are text-only error results; an image
        // here would be replayed to the API as if a tool had produced it.
        let out = synthesize_cap_results(&[tool_call("call_a", "image_view")], "cap");
        match &out[0] {
            Block::ToolResult { images, is_error, .. } => {
                assert!(images.is_empty());
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn one_synthetic_result_per_toolcall() {
        let blocks = vec![
            tool_call("call_a", "file_read"),
            Block::Text("interleaved prose".into()),
            tool_call("call_b", "git_log"),
            tool_call("call_c", "knowledge_query"),
        ];
        let out = synthesize_cap_results(&blocks, "cap reached");
        assert_eq!(out.len(), 3);
        let ids: Vec<&str> = out
            .iter()
            .map(|b| match b {
                Block::ToolResult { call_id, .. } => call_id.as_str(),
                _ => panic!("expected only ToolResult blocks"),
            })
            .collect();
        assert_eq!(ids, vec!["call_a", "call_b", "call_c"]);
        for b in &out {
            match b {
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    assert!(*is_error, "synthesized result must be flagged is_error");
                    assert_eq!(content, "cap reached");
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn preserves_original_call_id_order() {
        let blocks: Vec<Block> = (0..5)
            .map(|i| tool_call(&format!("c{i}"), "any_tool"))
            .collect();
        let out = synthesize_cap_results(&blocks, "cap");
        let ids: Vec<String> = out
            .iter()
            .map(|b| match b {
                Block::ToolResult { call_id, .. } => call_id.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec!["c0", "c1", "c2", "c3", "c4"]);
    }
}

#[cfg(test)]
mod iter_cap_parser_tests {
    use super::parse_iter_cap_value;

    #[test]
    fn valid_mid_range() {
        assert_eq!(parse_iter_cap_value("250"), Ok(250));
    }

    #[test]
    fn boundary_low_inclusive() {
        assert_eq!(parse_iter_cap_value("1"), Ok(1));
    }

    #[test]
    fn boundary_high_inclusive() {
        assert_eq!(parse_iter_cap_value("1000"), Ok(1000));
    }

    #[test]
    fn zero_is_out_of_range() {
        let err = parse_iter_cap_value("0").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn above_thousand_is_out_of_range() {
        let err = parse_iter_cap_value("1001").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn non_numeric_is_parse_error() {
        let err = parse_iter_cap_value("abc").unwrap_err();
        assert!(err.contains("not a positive integer"), "got: {err}");
    }

    #[test]
    fn negative_is_parse_error() {
        // usize parse fails on the leading '-' before any range check
        let err = parse_iter_cap_value("-5").unwrap_err();
        assert!(err.contains("not a positive integer"), "got: {err}");
    }
}

#[cfg(test)]
mod native_loop_tests {
    use super::*;

    #[test]
    fn legacy_user_message_converts_to_user_text_block() {
        let m = Message::user("hello");
        let api = legacy_message_to_api(&m);
        match api {
            ApiMessage::User { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    Block::Text(text) => assert_eq!(text, "hello"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn legacy_assistant_message_converts_to_assistant_text_block() {
        let m = Message::assistant("response");
        let api = legacy_message_to_api(&m);
        assert!(matches!(api, ApiMessage::Assistant { .. }));
    }

    #[test]
    fn turn_text_concatenates_text_blocks_skipping_thinking_and_tool_call() {
        let turn = AssistantTurn {
            content: vec![
                Block::ProviderOpaque(serde_json::json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": "sig"
                })),
                Block::Text("Hello".into()),
                Block::ToolCall {
                    id: "t1".into(),
                    name: "time".into(),
                    args: serde_json::json!({}),
                    provider_opaque: None,
                },
                Block::Text(", world".into()),
            ],
            outcome: TurnOutcome::EndTurn,
            stop_details: None,
        };
        assert_eq!(turn_text(&turn), "Hello, world");
    }

    #[test]
    fn display_model_maps_known_providers() {
        let cfg = empty_config();
        assert_eq!(display_model_for("anthropic", &cfg), "opus-5");
        assert_eq!(display_model_for("gemini", &cfg), "gemini-3.1-pro");
        let mut argon = empty_config();
        argon.gemini_model = Some("gemini-4-argon".into());
        assert_eq!(display_model_for("gemini", &argon), "gemini-4-argon");
        // Unknown defaults to anthropic display (defensive — never
        // shows an empty model name).
        assert_eq!(display_model_for("unknown", &cfg), "opus-5");
    }

    #[test]
    fn display_model_openai_compat_uses_configured_model_id() {
        let mut cfg = empty_config();
        cfg.openai_compat.ollama_model = "gpt-oss:20b".to_string();
        cfg.openai_compat.lm_studio_model = "qwen3.6:35b".to_string();
        assert_eq!(display_model_for("ollama", &cfg), "gpt-oss:20b");
        assert_eq!(display_model_for("lm_studio", &cfg), "qwen3.6:35b");
    }

    #[test]
    fn display_model_openai_compat_unconfigured_falls_back_to_label() {
        // Empty model field → falls back to a sensible placeholder
        // rather than emitting empty Brain: token in the status bar.
        let cfg = empty_config();
        assert_eq!(display_model_for("ollama", &cfg), "ollama");
        assert_eq!(display_model_for("lm_studio", &cfg), "lm_studio");
    }

    fn empty_config() -> config::SystemConfig {
        config::SystemConfig {
            name: "Embra".to_string(),
            api_key: String::new(),
            timezone: "UTC".to_string(),
            deployment_mode: "test".to_string(),
            created_at: String::new(),
            version: String::new(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: "anthropic".to_string(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            guardian_secrets: None,
            guardian_private_hosts: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            provider_gateway: None,
            openai_compat: config::OpenAiCompatConfig::default(),
        }
    }

    /// `/model` and the wizard store the alias; the turn, learning and
    /// the probe get the API id.
    #[test]
    fn a_persisted_gemini_alias_resolves_to_its_api_id() {
        assert_eq!(
            resolve_gemini_model_id_inner(None, Some("gemini-3.1-pro")),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            resolve_gemini_model_id_inner(None, Some("gemini-4-argon")),
            "gemini-4-argon"
        );
        assert_eq!(
            resolve_gemini_model_id_inner(Some("argon"), Some("gemini-3.1-pro")),
            "gemini-4-argon"
        );
    }

    #[test]
    fn resolve_gemini_model_falls_back_to_default() {
        assert_eq!(
            resolve_gemini_model_id_inner(None, None),
            "gemini-3.1-pro-preview"
        );
    }

    #[test]
    fn resolve_gemini_model_uses_config_field_when_no_env() {
        assert_eq!(
            resolve_gemini_model_id_inner(None, Some("gemini-3.1-pro-preview-customtools")),
            "gemini-3.1-pro-preview-customtools"
        );
    }

    #[test]
    fn resolve_gemini_model_env_overrides_config() {
        assert_eq!(
            resolve_gemini_model_id_inner(Some("env-override-id"), Some("config-id")),
            "env-override-id"
        );
    }

    #[test]
    fn resolve_gemini_model_empty_strings_skip_to_next_layer() {
        // Whitespace-only env value falls through to config; empty
        // config field falls through to the default.
        assert_eq!(
            resolve_gemini_model_id_inner(Some("   "), Some("from-config")),
            "from-config"
        );
        assert_eq!(
            resolve_gemini_model_id_inner(Some(""), Some("")),
            "gemini-3.1-pro-preview"
        );
    }

    #[test]
    fn providers_compatible_strict_equality() {
        assert!(providers_compatible("anthropic", "anthropic"));
        assert!(providers_compatible("gemini", "gemini"));
        assert!(!providers_compatible("anthropic", "gemini"));
        assert!(!providers_compatible("gemini", "anthropic"));
        // Empty/missing is no longer a wildcard. The caller
        // (`read_session_provider`) defaults missing fields to
        // `"anthropic"` before reaching this function, so empty
        // values reaching this function only happen on programmer
        // error and should NOT match.
        assert!(!providers_compatible("", "anthropic"));
        assert!(!providers_compatible("gemini", ""));
    }

    #[test]
    fn providers_compatible_openai_compat_self_pairs() {
        // Sprint 5: each OpenAI-compat preset is compatible only with
        // itself (Locked Decision #2: distinct cross-provider session
        // groups, hard-block on mismatch).
        assert!(providers_compatible("ollama", "ollama"));
        assert!(providers_compatible("lm_studio", "lm_studio"));
    }

    #[test]
    fn providers_compatible_openai_compat_cross_pairs_blocked() {
        // Ollama and LM Studio are distinct providers — sessions
        // started under one cannot resume under the other.
        assert!(!providers_compatible("ollama", "lm_studio"));
        assert!(!providers_compatible("lm_studio", "ollama"));
    }

    #[test]
    fn providers_compatible_openai_compat_vs_anthropic_gemini_blocked() {
        // OpenAI-compat presets cross-block against Anthropic/Gemini.
        for openai_compat in ["ollama", "lm_studio"] {
            for other in ["anthropic", "gemini"] {
                assert!(
                    !providers_compatible(openai_compat, other),
                    "{openai_compat} vs {other} should not match"
                );
                assert!(
                    !providers_compatible(other, openai_compat),
                    "{other} vs {openai_compat} should not match"
                );
            }
        }
    }

    #[test]
    fn build_openai_compat_provider_unconfigured_errors() {
        // Empty endpoint/model in config → preset reports unconfigured.
        let cfg = empty_config();
        let err = build_openai_compat_provider(ProviderKind::Ollama, &cfg)
            .err()
            .expect("expected error");
        assert!(err.contains("not configured"), "got: {err}");
        let err = build_openai_compat_provider(ProviderKind::LmStudio, &cfg)
            .err()
            .expect("expected error");
        assert!(err.contains("not configured"), "got: {err}");
    }

    #[test]
    fn build_openai_compat_provider_configured_succeeds() {
        let mut cfg = empty_config();
        cfg.openai_compat.ollama_endpoint = "http://localhost:11434".to_string();
        cfg.openai_compat.ollama_model = "gpt-oss:20b".to_string();
        let provider = build_openai_compat_provider(ProviderKind::Ollama, &cfg).unwrap();
        assert_eq!(provider.kind(), ProviderKind::Ollama);
        assert_eq!(provider.display_name(), "gpt-oss:20b");
    }

    #[test]
    fn a_switch_never_carries_a_level_and_says_what_the_new_model_runs_at() {
        use crate::provider::openai_compat::OpenAiCompatPreset::Ollama;
        // William, 2026-10-22: `/effort low` on Qwen3.8, then a switch to
        // V4 Pro. V4 Pro gets its own preset, and Qwen3.8 keeps `low`.
        let mut cfg = empty_config();
        set_local_model(&mut cfg, Ollama, "qwen3.8:27b-mxfp8");
        set_effort_field(&mut cfg, ProviderKind::Ollama, Some("low".into()));
        set_local_model(&mut cfg, Ollama, "deepseek-v4-pro:cloud");
        assert_eq!(resolve_provider_effort(ProviderKind::Ollama, &cfg), None);
        assert_eq!(
            local_model_set_message(&cfg, Ollama, "deepseek-v4-pro:cloud"),
            "Ollama model set to deepseek-v4-pro:cloud. Effort: not set — embraOS sends \
             reasoning_effort=max automatically. Takes effect on your next message."
        );
        set_local_model(&mut cfg, Ollama, "qwen3.8:27b-mxfp8");
        assert_eq!(resolve_provider_effort(ProviderKind::Ollama, &cfg).as_deref(), Some("low"));
        assert_eq!(
            local_model_set_message(&cfg, Ollama, "qwen3.8:27b-mxfp8"),
            "Ollama model set to qwen3.8:27b-mxfp8. Effort: low, set with /effort for this \
             model. Takes effect on your next message."
        );
    }

    #[test]
    fn build_openai_compat_provider_reads_bearer_from_env_per_call() {
        use std::sync::Mutex;
        // Serialize env-var access across tests in this file to avoid
        // races. Only this and one sister test mutate this var.
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let mut cfg = empty_config();
        cfg.openai_compat.ollama_endpoint = "http://localhost:11434".to_string();
        cfg.openai_compat.ollama_model = "m".to_string();
        // SAFETY: serialized via LOCK; single-threaded test runtime here.
        unsafe {
            std::env::set_var("EMBRA_OLLAMA_BEARER", "secret-from-env");
        }
        // build_openai_compat_provider reads env each call; we can't
        // observe the bearer field directly (it's private) but we can
        // verify construction succeeds without panic.
        let _ = build_openai_compat_provider(ProviderKind::Ollama, &cfg).unwrap();
        unsafe {
            std::env::remove_var("EMBRA_OLLAMA_BEARER");
        }
    }

    #[test]
    fn build_provider_from_config_dispatches_by_kind() {
        // Anthropic (falls back to the supplied key when config has none).
        let mut cfg = empty_config();
        cfg.api_provider = "anthropic".to_string();
        let p = build_provider_from_config(&cfg, "sk-test", None).unwrap();
        assert_eq!(p.kind(), ProviderKind::Anthropic);

        // Gemini builds cacheless when gemini_cache_db is None (the
        // one-shot path used by the replicant check).
        cfg.api_provider = "gemini".to_string();
        let p = build_provider_from_config(&cfg, "g-test", None).unwrap();
        assert_eq!(p.kind(), ProviderKind::Gemini);

        // Unconfigured OpenAI-compat preset → Err (loop → SystemMessage;
        // replicant check → fail closed, no proposal).
        cfg.api_provider = "ollama".to_string();
        assert!(build_provider_from_config(&cfg, "", None).is_err());
    }

    /// With the provider gateway stored and on, every kind is built on the
    /// gateway's prefixed base; off, each on its own host and the presets
    /// on their stored endpoint.
    #[test]
    fn build_provider_from_config_routes_every_kind_through_the_gateway_when_on() {
        let mut cfg = empty_config();
        cfg.provider_gateway = Some(config::ProviderGateway {
            url: "http://gw:4000".into(),
            enabled: true,
        });
        cfg.openai_compat.ollama_endpoint = "http://mac:11434".into();
        cfg.openai_compat.ollama_model = "qwen3.8:27b".into();
        cfg.openai_compat.lm_studio_endpoint = "http://mac:1234".into();
        cfg.openai_compat.lm_studio_model = "qwen/qwen3.8-27b".into();
        for (kind, base) in [
            ("anthropic", "http://gw:4000/anthropic"),
            ("gemini", "http://gw:4000/gemini"),
            ("ollama", "http://gw:4000/ollama"),
            ("lm_studio", "http://gw:4000/lmstudio"),
        ] {
            cfg.api_provider = kind.to_string();
            let p = build_provider_from_config(&cfg, "k", None).unwrap();
            assert_eq!(p.base_url(), base, "{kind}");
        }
        cfg.provider_gateway.as_mut().unwrap().enabled = false;
        for (kind, base) in [
            ("anthropic", "https://api.anthropic.com"),
            ("gemini", "https://generativelanguage.googleapis.com"),
            ("ollama", "http://mac:11434"),
            ("lm_studio", "http://mac:1234"),
        ] {
            cfg.api_provider = kind.to_string();
            let p = build_provider_from_config(&cfg, "k", None).unwrap();
            assert_eq!(p.base_url(), base, "{kind} off");
        }
        // A preset set up through the gateway, no direct endpoint: built on
        // the gateway's prefix while it is on, refused while it is off with
        // the two ways out, and refused with the URL way when it is gone.
        cfg.api_provider = "ollama".to_string();
        cfg.openai_compat.ollama_endpoint = String::new();
        let err = build_provider_from_config(&cfg, "k", None).err().expect("refused");
        assert!(err.starts_with("Cannot start a turn — Ollama has no direct endpoint"), "{err}");
        assert!(err.contains("/local-ai-gateway on") && err.contains("/provider --setup ollama"), "{err}");
        cfg.provider_gateway.as_mut().unwrap().enabled = true;
        let p = build_provider_from_config(&cfg, "k", None).unwrap();
        assert_eq!(p.base_url(), "http://gw:4000/ollama");
        cfg.provider_gateway = None;
        let err = build_provider_from_config(&cfg, "k", None).err().expect("refused");
        assert!(err.contains("no longer stored") && err.contains("/local-ai-gateway <url>"), "{err}");
        cfg.openai_compat.ollama_model = String::new();
        let err = build_provider_from_config(&cfg, "k", None).err().expect("refused");
        assert!(err.contains("Ollama is not configured. /provider --setup ollama"), "{err}");
    }

    /// The probe's target carries the same base the turn is built on.
    #[test]
    fn provider_probe_target_carries_the_routed_base_for_every_kind() {
        let mut cfg = empty_config();
        cfg.anthropic_api_key = Some("sk-a".into());
        cfg.gemini_api_key = Some("g".into());
        cfg.openai_compat.ollama_endpoint = "http://mac:11434".into();
        cfg.openai_compat.ollama_model = "qwen3.8:27b".into();
        cfg.openai_compat.lm_studio_endpoint = "http://mac:1234".into();
        cfg.openai_compat.lm_studio_model = "m".into();
        for (kind, direct) in [
            ("anthropic", "https://api.anthropic.com"),
            ("gemini", "https://generativelanguage.googleapis.com"),
            ("ollama", "http://mac:11434"),
            ("lm_studio", "http://mac:1234"),
        ] {
            cfg.api_provider = kind.to_string();
            assert_eq!(provider_probe_target(&cfg, "").unwrap().endpoint, direct, "{kind}");
        }
        cfg.provider_gateway = Some(config::ProviderGateway {
            url: "http://gw:4000".into(),
            enabled: true,
        });
        for (kind, routed) in [
            ("anthropic", "http://gw:4000/anthropic"),
            ("gemini", "http://gw:4000/gemini"),
            ("ollama", "http://gw:4000/ollama"),
            ("lm_studio", "http://gw:4000/lmstudio"),
        ] {
            cfg.api_provider = kind.to_string();
            assert_eq!(provider_probe_target(&cfg, "").unwrap().endpoint, routed, "{kind} on");
        }
        // A preset without a direct endpoint: probed through the gateway
        // while it is on, no target (unconfigured) while it is off.
        cfg.api_provider = "ollama".to_string();
        cfg.openai_compat.ollama_endpoint = String::new();
        assert_eq!(provider_probe_target(&cfg, "").unwrap().endpoint, "http://gw:4000/ollama");
        cfg.provider_gateway.as_mut().unwrap().enabled = false;
        assert!(provider_probe_target(&cfg, "").is_none(), "no direct endpoint, gateway off");
    }

    #[test]
    fn write_credential_state_sets_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmpdir = std::env::temp_dir().join(format!("embra-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("test_credential");
        let path_str = path.to_str().unwrap();
        config::write_credential_state(path_str, "secret").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {:o}", mode);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&tmpdir);
    }

    #[test]
    fn write_credential_state_creates_parent_dirs() {
        let tmpdir = std::env::temp_dir().join(format!(
            "embra-test-deep-{}",
            std::process::id()
        ));
        let nested = tmpdir.join("a").join("b");
        let path = nested.join("creds");
        let path_str = path.to_str().unwrap();
        config::write_credential_state(path_str, "x").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    /// The continuation failure names each appended result by its tool,
    /// with its byte size and the total; a result whose call is unknown
    /// keeps its call id.
    #[test]
    fn the_continuation_failure_names_each_tool_result_and_its_size() {
        let call = |id: &str, name: &str| Block::ToolCall {
            id: id.into(),
            name: name.into(),
            args: serde_json::json!({}),
            provider_opaque: None,
        };
        let result = |id: &str, content: String| Block::ToolResult {
            call_id: id.into(),
            content,
            is_error: false,
            images: Vec::new(),
        };
        let calls = vec![call("c1", "file_read"), call("c2", "git_log")];
        let results = vec![
            result("c1", "x".repeat(1_048_576)),
            result("c2", "y".repeat(2048)),
            result("orphan", "z".into()),
        ];
        assert_eq!(
            tool_result_sizes(&calls, &results),
            "file_read 1048576 B, git_log 2048 B, orphan 1 B (3 results, 1050625 B total)"
        );
        assert_eq!(tool_result_sizes(&calls, &[]), " (0 results, 0 B total)");
    }

    #[test]
    fn turn_text_empty_without_text_blocks() {
        let turn = AssistantTurn {
            content: vec![Block::ProviderOpaque(serde_json::json!({
                "type": "thinking",
                "thinking": "",
                "signature": "sig"
            }))],
            outcome: TurnOutcome::ToolUse,
            stop_details: None,
        };
        assert_eq!(turn_text(&turn), "");
    }
}

#[cfg(test)]
mod setup_reconfigure_tests {
    //! Tests for the OpenAI-compat post-wizard reconfigure state
    //! machine (`/provider --setup <ollama|lm_studio>`). State-machine
    //! transitions that don't hit WardSONDB are exercised here; the
    //! complete-and-persist path depends on a live WardSONDB and is
    //! covered by QEMU E2E smoke.
    use super::*;
    use crate::provider::openai_compat::OpenAiCompatPreset;
    use embra_common::proto::brain::ConversationResponse;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fake_db() -> Arc<WardsonDbClient> {
        // Connection is never made in tests that don't reach
        // complete_openai_compat_setup. Port 1 won't connect.
        Arc::new(WardsonDbClient::from_url("http://127.0.0.1:1"))
    }

    fn make_state(
        preset: OpenAiCompatPreset,
        endpoint: Option<&str>,
        bearer: Option<&str>,
        model: Option<&str>,
        step: OpenAiCompatSetupStep,
    ) -> OpenAiCompatSetupState {
        OpenAiCompatSetupState {
            preset,
            current_endpoint: endpoint.map(String::from),
            current_bearer: bearer.map(String::from),
            current_model: model.map(String::from),
            step,
            model_only: false,
            gateway: None,
        }
    }

    async fn run_step(
        state: &mut OpenAiCompatSetupState,
        input: &str,
    ) -> (Vec<ConversationResponse>, SetupStepOutcome) {
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        let pending = Arc::new(Mutex::new(None));
        let db = fake_db();
        let outcome =
            handle_pending_openai_compat_step(state, input, &tx, &db, &pending).await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(Ok(r)) = rx.recv().await {
            events.push(r);
        }
        (events, outcome)
    }

    fn last_setup_prompt(events: &[ConversationResponse]) -> Option<&SetupPrompt> {
        events.iter().rev().find_map(|e| match &e.response_type {
            Some(conversation_response::ResponseType::Setup(p)) => Some(p),
            _ => None,
        })
    }

    // ===========================================================
    // Helper-fn tests
    // ===========================================================

    #[test]
    fn bearer_state_path_mappings() {
        assert_eq!(
            bearer_state_path(OpenAiCompatPreset::Ollama),
            "/embra/state/bearer_ollama"
        );
        assert_eq!(
            bearer_state_path(OpenAiCompatPreset::LmStudio),
            "/embra/state/bearer_lm_studio"
        );
    }

    #[test]
    fn bearer_env_var_mappings() {
        assert_eq!(
            bearer_env_var(OpenAiCompatPreset::Ollama),
            "EMBRA_OLLAMA_BEARER"
        );
        assert_eq!(
            bearer_env_var(OpenAiCompatPreset::LmStudio),
            "EMBRA_LM_STUDIO_BEARER"
        );
    }

    #[test]
    fn non_empty_returns_some_for_non_empty_string() {
        assert_eq!(non_empty("hello"), Some("hello".to_string()));
        assert_eq!(non_empty(""), None);
    }

    // ===========================================================
    // AwaitingEndpoint transitions
    // ===========================================================

    #[tokio::test]
    async fn endpoint_with_existing_bearer_emits_3way_selector() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://old:11434"),
            Some("old-token"),
            Some("gpt-oss:20b"),
            OpenAiCompatSetupStep::AwaitingEndpoint,
        );
        let (events, _) = run_step(&mut state, "http://new:11434").await;
        let prompt = last_setup_prompt(&events).expect("expected SetupPrompt");
        assert_eq!(prompt.field_type, SetupFieldType::Selector as i32);
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.options[0], BEARER_KEEP);
        assert_eq!(prompt.options[1], BEARER_CHANGE);
        assert_eq!(prompt.options[2], BEARER_REMOVE);
        assert_eq!(prompt.default_value, BEARER_KEEP);
        assert!(matches!(
            state.step,
            OpenAiCompatSetupStep::AwaitingBearerChoice { .. }
        ));
    }

    #[tokio::test]
    async fn endpoint_no_existing_bearer_emits_2way_selector() {
        let mut state = make_state(
            OpenAiCompatPreset::LmStudio,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingEndpoint,
        );
        let (events, _) = run_step(&mut state, "http://localhost:1234").await;
        let prompt = last_setup_prompt(&events).expect("expected SetupPrompt");
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0], BEARER_SET);
        assert_eq!(prompt.options[1], BEARER_SKIP);
        assert_eq!(prompt.default_value, BEARER_SKIP);
    }

    #[tokio::test]
    async fn endpoint_empty_falls_back_to_current() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://existing:11434"),
            None,
            None,
            OpenAiCompatSetupStep::AwaitingEndpoint,
        );
        let (_, _) = run_step(&mut state, "").await;
        // Endpoint captured into next step's state.
        let OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint } = &state.step else {
            panic!("expected AwaitingBearerChoice");
        };
        assert_eq!(endpoint, "http://existing:11434");
    }

    #[tokio::test]
    async fn endpoint_empty_falls_back_to_preset_default_when_no_current() {
        let mut state = make_state(
            OpenAiCompatPreset::LmStudio,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingEndpoint,
        );
        let (_, _) = run_step(&mut state, "").await;
        let OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint } = &state.step else {
            panic!("expected AwaitingBearerChoice");
        };
        assert_eq!(endpoint, "http://localhost:1234");
    }

    #[tokio::test]
    async fn endpoint_normalizes_bare_host() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingEndpoint,
        );
        let (_, _) = run_step(&mut state, "myserver").await;
        let OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint } = &state.step else {
            panic!("expected AwaitingBearerChoice");
        };
        assert_eq!(endpoint, "http://myserver:11434");
    }

    // ===========================================================
    // AwaitingBearerChoice transitions
    // ===========================================================

    #[tokio::test]
    async fn bearer_choice_change_advances_to_token_step() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://x:11434"),
            Some("old"),
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: "http://x:11434".to_string(),
            },
        );
        let (events, _) = run_step(&mut state, BEARER_CHANGE).await;
        let prompt = last_setup_prompt(&events).expect("expected SetupPrompt");
        // #20: a bearer is asked for as a secret, never echoed by a client.
        assert_eq!(prompt.field_type, SetupFieldType::Secret as i32);
        assert!(matches!(
            state.step,
            OpenAiCompatSetupStep::AwaitingBearerToken { .. }
        ));
    }

    #[tokio::test]
    async fn bearer_choice_set_no_current_advances_to_token_step() {
        let mut state = make_state(
            OpenAiCompatPreset::LmStudio,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: "http://localhost:1234".to_string(),
            },
        );
        let (_, _) = run_step(&mut state, BEARER_SET).await;
        assert!(matches!(
            state.step,
            OpenAiCompatSetupStep::AwaitingBearerToken { .. }
        ));
    }

    #[tokio::test]
    async fn bearer_choice_unknown_value_re_emits_selector() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://x"),
            Some("t"),
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: "http://x".to_string(),
            },
        );
        let (events, _) = run_step(&mut state, "definitely-not-a-choice").await;
        // Got an Error system message; state still AwaitingBearerChoice.
        let has_error = events.iter().any(|e| matches!(
            &e.response_type,
            Some(conversation_response::ResponseType::System(m)) if m.msg_type == SystemMessageType::Error as i32
        ));
        assert!(has_error);
        assert!(matches!(
            state.step,
            OpenAiCompatSetupStep::AwaitingBearerChoice { .. }
        ));
    }

    // ===========================================================
    // AwaitingBearerChoice → probe via wiremock
    // ===========================================================

    #[tokio::test]
    async fn bearer_choice_remove_advances_to_probe_then_model_selection() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "m1"}, {"id": "m2"}]
            })))
            .mount(&server)
            .await;

        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some(&server.uri()),
            Some("had-bearer"),
            Some("m1"),
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: server.uri(),
            },
        );
        let (events, _) = run_step(&mut state, BEARER_REMOVE).await;

        let prompt = last_setup_prompt(&events).expect("expected SetupPrompt");
        assert_eq!(prompt.field_type, SetupFieldType::Selector as i32);
        assert_eq!(prompt.options, vec!["m1".to_string(), "m2".to_string()]);
        // Default-prefer the current model when still in list.
        assert_eq!(prompt.default_value, "m1");

        let OpenAiCompatSetupStep::AwaitingModelSelection {
            bearer, models, ..
        } = &state.step
        else {
            panic!("expected AwaitingModelSelection");
        };
        assert!(bearer.is_none(), "Remove path → bearer = None");
        assert_eq!(models.len(), 2);
    }

    #[tokio::test]
    async fn bearer_choice_keep_advances_to_probe_with_current_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer keep-me",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "m1"}]
            })))
            .mount(&server)
            .await;

        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some(&server.uri()),
            Some("keep-me"),
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: server.uri(),
            },
        );
        let (_, _) = run_step(&mut state, BEARER_KEEP).await;
        let OpenAiCompatSetupStep::AwaitingModelSelection { bearer, .. } = &state.step
        else {
            panic!("expected AwaitingModelSelection");
        };
        assert_eq!(bearer.as_deref(), Some("keep-me"));
    }

    #[tokio::test]
    async fn bearer_token_captures_value_and_advances_to_probe() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(wiremock::matchers::header("authorization", "Bearer fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": "x"}]
            })))
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerToken {
                endpoint: server.uri(),
            },
        );
        let (_, _) = run_step(&mut state, "fresh").await;
        let OpenAiCompatSetupStep::AwaitingModelSelection { bearer, .. } = &state.step
        else {
            panic!("expected AwaitingModelSelection");
        };
        assert_eq!(bearer.as_deref(), Some("fresh"));
    }

    // ===========================================================
    // Recovery paths (probe failure / 0 models / invalid model)
    // ===========================================================

    #[tokio::test]
    async fn probe_failure_resets_to_endpoint() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://127.0.0.1:1"), // unreachable
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: "http://127.0.0.1:1".to_string(),
            },
        );
        let (events, _) = run_step(&mut state, BEARER_SKIP).await;
        // Saw an Error message + reset.
        assert!(matches!(state.step, OpenAiCompatSetupStep::AwaitingEndpoint));
        // Final SetupPrompt is the re-emitted Endpoint Text prompt.
        let prompt = last_setup_prompt(&events).expect("expected SetupPrompt");
        assert_eq!(prompt.field_type, SetupFieldType::Text as i32);
    }

    #[tokio::test]
    async fn zero_models_resets_to_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": []
            })))
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some(&server.uri()),
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: server.uri(),
            },
        );
        let (_, _) = run_step(&mut state, BEARER_SKIP).await;
        assert!(matches!(state.step, OpenAiCompatSetupStep::AwaitingEndpoint));
    }

    #[tokio::test]
    async fn invalid_model_selection_resets_to_endpoint() {
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://x:11434"),
            None,
            None,
            OpenAiCompatSetupStep::AwaitingModelSelection {
                endpoint: "http://x:11434".to_string(),
                bearer: None,
                models: vec!["a".to_string(), "b".to_string()],
            },
        );
        let (events, _) = run_step(&mut state, "not-in-list").await;
        assert!(matches!(state.step, OpenAiCompatSetupStep::AwaitingEndpoint));
        let has_error = events.iter().any(|e| matches!(
            &e.response_type,
            Some(conversation_response::ResponseType::System(m)) if m.msg_type == SystemMessageType::Error as i32
        ));
        assert!(has_error);
    }

    #[tokio::test]
    async fn model_selection_default_falls_to_first_when_current_not_in_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "new-a"}, {"id": "new-b"}]
            })))
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some(&server.uri()),
            None,
            Some("stale-model-not-on-server"),
            OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: server.uri(),
            },
        );
        let (events, _) = run_step(&mut state, BEARER_SKIP).await;
        let prompt = last_setup_prompt(&events).expect("expected Selector");
        assert_eq!(prompt.default_value, "new-a");
    }

    /// The console's selector starts on the first option whatever the
    /// default: the current model leads the list, so Enter keeps it.
    #[tokio::test]
    async fn the_current_model_leads_the_wizards_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": "a"}, {"id": "b"}, {"id": "c"}]
            })))
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::LmStudio,
            Some(&server.uri()),
            None,
            Some("c"),
            OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint: server.uri() },
        );
        let (events, _) = run_step(&mut state, BEARER_SKIP).await;
        let prompt = last_setup_prompt(&events).expect("expected Selector");
        assert_eq!(prompt.options, vec!["c".to_string(), "a".to_string(), "b".to_string()]);
        assert_eq!(prompt.default_value, "c");
    }

    // ===========================================================
    // `/model` on a local server: the reconfigure flow's model step alone
    // ===========================================================

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_local_model_must_be_one_the_server_lists() {
        let models = names(&["qwen3.8:27b", "Gemma4:12b", "gemma4:31b"]);
        let ollama = OpenAiCompatPreset::Ollama;
        assert_eq!(local_model_choice(ollama, &models, "qwen3.8:27b"), Ok("qwen3.8:27b".to_string()));
        // The one listed name that matches ignoring case is that name.
        assert_eq!(local_model_choice(ollama, &models, "QWEN3.8:27B"), Ok("qwen3.8:27b".to_string()));
        // An exact name wins; two names that differ only in case are no choice.
        let twins = names(&["Model", "model"]);
        assert_eq!(local_model_choice(ollama, &twins, "model"), Ok("model".to_string()));
        assert!(local_model_choice(ollama, &twins, "MODEL").is_err());
        let refused = local_model_choice(ollama, &models, "llama4").unwrap_err();
        assert!(refused.contains("'llama4' is not a model Ollama lists"), "{refused}");
        assert!(refused.contains("It lists: qwen3.8:27b, Gemma4:12b, gemma4:31b."), "{refused}");
        assert!(refused.ends_with("/model alone picks from the list."), "{refused}");
        let many: Vec<String> = (0..15).map(|i| format!("m{i}")).collect();
        let refused = local_model_choice(OpenAiCompatPreset::LmStudio, &many, "x").unwrap_err();
        assert!(refused.contains("not a model LM Studio lists"), "{refused}");
        assert!(refused.contains("m11 and 3 more."), "{refused}");
    }

    #[test]
    fn model_selection_starts_at_the_model_step_and_keeps_the_endpoint_and_bearer() {
        let (prompt, state) = local_model_selection(
            OpenAiCompatPreset::LmStudio,
            "http://studio:1234".to_string(),
            Some("tok".to_string()),
            Some("qwen3.8-27b".to_string()),
            names(&["gemma4-31b", "qwen3.8-27b"]),
            None,
        );
        assert_eq!(prompt.field_type, SetupFieldType::Selector as i32);
        assert_eq!(prompt.prompt, "Select an LM Studio model:");
        // The current model leads: the console's selector starts on the
        // first option, so Enter keeps the model.
        assert_eq!(prompt.options, names(&["qwen3.8-27b", "gemma4-31b"]));
        assert_eq!(prompt.default_value, "qwen3.8-27b");
        assert!(state.model_only);
        let OpenAiCompatSetupStep::AwaitingModelSelection { endpoint, bearer, models } = &state.step else {
            panic!("expected AwaitingModelSelection");
        };
        assert_eq!(endpoint, "http://studio:1234");
        assert_eq!(bearer.as_deref(), Some("tok"));
        assert_eq!(models.len(), 2);
        // A current model the server no longer lists: the first is chosen.
        let (prompt, _) =
            local_model_selection(OpenAiCompatPreset::Ollama, "e".into(), None, Some("gone".into()), names(&["a", "b"]), None);
        assert_eq!(prompt.default_value, "a");
    }

    /// `/model` never asked for an endpoint: a pick outside the list ends
    /// the flow and names the way back, where the reconfigure flow restarts
    /// at the endpoint step.
    #[tokio::test]
    async fn a_model_only_pick_outside_the_list_ends_the_flow() {
        let (_, mut state) =
            local_model_selection(OpenAiCompatPreset::Ollama, "http://ollama:11434".into(), None, None, names(&["a", "b"]), None);
        let (events, outcome) = run_step(&mut state, "c").await;
        assert!(matches!(outcome, SetupStepOutcome::Error));
        assert!(last_setup_prompt(&events).is_none(), "no endpoint prompt");
        let error = events
            .iter()
            .find_map(|e| match &e.response_type {
                Some(conversation_response::ResponseType::System(m)) => Some(m.content.clone()),
                _ => None,
            })
            .expect("an error");
        assert!(error.contains("Run /model again"), "{error}");
    }

    /// The pick saves the model and nothing else: the stored config is read
    /// and written back with the endpoint as it was.
    #[tokio::test]
    async fn a_model_only_pick_moves_the_model_alone() {
        let db_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {
                "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
                "created_at": "", "version": "test", "api_provider": "ollama",
                "openai_compat": {"ollama_endpoint": "http://ollama:11434", "ollama_model": "a"},
            }})))
            .mount(&db_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/config.system"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {}})))
            .mount(&db_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {"_id": "config"}})))
            .mount(&db_server)
            .await;
        let (_, mut state) = local_model_selection(
            OpenAiCompatPreset::Ollama,
            "http://ollama:11434".into(),
            None,
            Some("a".into()),
            names(&["a", "b"]),
            None,
        );
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        let pending = Arc::new(Mutex::new(None));
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let outcome = handle_pending_openai_compat_step(&mut state, "b", &tx, &db, &pending).await;
        drop(tx);
        assert!(matches!(outcome, SetupStepOutcome::Complete));
        let mut said = Vec::new();
        while let Some(Ok(r)) = rx.recv().await {
            if let Some(conversation_response::ResponseType::System(m)) = r.response_type {
                said.push(m.content);
            }
        }
        assert_eq!(
            said,
            vec!["Ollama model set to b. Effort: not set — nothing is sent; the server's per-model \
                  default applies. Takes effect on your next message."
                .to_string()]
        );
        let saved: Vec<serde_json::Value> = db_server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| serde_json::from_slice(&r.body).expect("a config document"))
            .collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["openai_compat"]["ollama_model"], "b");
        assert_eq!(saved[0]["openai_compat"]["ollama_endpoint"], "http://ollama:11434");
        assert!(pending.lock().await.is_none());
    }

    fn gw(uri: &str) -> config::ProviderGateway {
        config::ProviderGateway { url: uri.to_string(), enabled: true }
    }

    fn system_texts(events: &[ConversationResponse]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match &e.response_type {
                Some(conversation_response::ResponseType::System(m)) => Some(m.content.clone()),
                _ => None,
            })
            .collect()
    }

    /// With the gateway on, the models are listed through its prefix, not
    /// at the typed endpoint, and the Info says so.
    #[tokio::test]
    async fn with_the_gateway_on_the_setup_probe_goes_through_its_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list", "data": [{"id": "qwen3.8:27b"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            Some("http://mac:11434"),
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint: "http://mac:11434".to_string() },
        );
        state.gateway = Some(gw(&server.uri()));
        let (events, outcome) = run_step(&mut state, BEARER_SKIP).await;
        assert!(matches!(outcome, SetupStepOutcome::Advance));
        assert!(matches!(state.step, OpenAiCompatSetupStep::AwaitingModelSelection { ref endpoint, .. } if endpoint == "http://mac:11434"));
        let prompt = last_setup_prompt(&events).expect("the model selector");
        assert_eq!(prompt.prompt, "Select a model:");
        let texts = system_texts(&events);
        assert!(texts.iter().any(|t| t == &format!("Probing {}/ollama for the models the gateway's upstream lists…", server.uri())), "{texts:?}");
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.url.path() == "/ollama/v1/models"));
    }

    /// `none` is a blank direct endpoint while the gateway is on, and is
    /// refused with the reason while it is off, the endpoint asked again.
    #[tokio::test]
    async fn none_is_a_blank_endpoint_while_the_gateway_is_on_and_refused_off() {
        let mut on = make_state(OpenAiCompatPreset::Ollama, None, None, None, OpenAiCompatSetupStep::AwaitingEndpoint);
        on.gateway = Some(gw("http://gw:4000"));
        let (_, outcome) = run_step(&mut on, "NONE").await;
        assert!(matches!(outcome, SetupStepOutcome::Advance));
        assert!(matches!(on.step, OpenAiCompatSetupStep::AwaitingBearerChoice { ref endpoint } if endpoint.is_empty()));
        let mut on = make_state(OpenAiCompatPreset::Ollama, None, None, None, OpenAiCompatSetupStep::AwaitingEndpoint);
        on.gateway = Some(gw("http://gw:4000"));
        run_step(&mut on, "").await;
        assert!(matches!(on.step, OpenAiCompatSetupStep::AwaitingBearerChoice { ref endpoint } if endpoint.is_empty()), "nothing with nothing current is none too");

        let mut off = make_state(OpenAiCompatPreset::Ollama, None, None, None, OpenAiCompatSetupStep::AwaitingEndpoint);
        let (events, outcome) = run_step(&mut off, "none").await;
        assert!(matches!(outcome, SetupStepOutcome::Advance));
        assert!(matches!(off.step, OpenAiCompatSetupStep::AwaitingEndpoint));
        assert!(system_texts(&events).iter().any(|t| t.starts_with("none needs the gateway:")), "{:?}", system_texts(&events));
        let prompt = last_setup_prompt(&events).expect("the endpoint asked again");
        assert_eq!(prompt.prompt, "Re-enter the Ollama endpoint URL:");
        assert_eq!(prompt.default_value, "http://localhost:11434");
    }

    /// A probe that fails through the gateway names the gateway's upstream
    /// setting and the way to set the preset up directly, and the endpoint
    /// is asked again with `none` allowed.
    #[tokio::test]
    async fn a_failed_probe_through_the_gateway_names_the_upstream_and_asks_the_endpoint_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ollama/v1/models"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::Ollama,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint: String::new() },
        );
        state.gateway = Some(gw(&server.uri()));
        let (events, _) = run_step(&mut state, BEARER_SKIP).await;
        assert!(matches!(state.step, OpenAiCompatSetupStep::AwaitingEndpoint));
        let texts = system_texts(&events);
        let failed = texts.iter().find(|t| t.starts_with("Probe through the gateway at")).unwrap_or_else(|| panic!("{texts:?}"));
        assert!(failed.contains("[backends.ollama] upstream must answer") && failed.contains("/local-ai-gateway off"), "{failed}");
        let prompt = last_setup_prompt(&events).expect("the endpoint asked again");
        assert_eq!(prompt.prompt, "Re-enter the Ollama endpoint URL (none is allowed):");
        assert_eq!(prompt.default_value, "none");
    }

    /// A pick after `none` saves a blank endpoint, and the acknowledgement
    /// says the preset answers through the gateway only. LM Studio here, so
    /// the bearer step's env-var clearing touches no other test's variable.
    #[tokio::test]
    async fn a_blank_endpoint_is_saved_and_the_ack_says_through_the_gateway_only() {
        let db_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {
                "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
                "created_at": "", "version": "test", "api_provider": "anthropic",
                "provider_gateway": {"url": "http://gw:4000", "enabled": true},
            }})))
            .mount(&db_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/config.system"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {}})))
            .mount(&db_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {"_id": "config"}})))
            .mount(&db_server)
            .await;
        let mut state = make_state(
            OpenAiCompatPreset::LmStudio,
            None,
            None,
            None,
            OpenAiCompatSetupStep::AwaitingModelSelection { endpoint: String::new(), bearer: None, models: names(&["m"]) },
        );
        state.gateway = Some(gw("http://gw:4000"));
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        let pending = Arc::new(Mutex::new(None));
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let outcome = handle_pending_openai_compat_step(&mut state, "m", &tx, &db, &pending).await;
        drop(tx);
        assert!(matches!(outcome, SetupStepOutcome::Complete));
        let mut said = Vec::new();
        while let Some(Ok(r)) = rx.recv().await {
            if let Some(conversation_response::ResponseType::System(m)) = r.response_type {
                said.push(m.content);
            }
        }
        let ack = said.iter().find(|s| s.starts_with("LM Studio reconfigured:")).unwrap_or_else(|| panic!("{said:?}"));
        assert!(ack.contains("endpoint=none (through the gateway only), bearer=none, model=m."), "{ack}");
        let saved: Vec<serde_json::Value> = db_server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| serde_json::from_slice(&r.body).expect("a config document"))
            .collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["openai_compat"]["lm_studio_endpoint"], "");
        assert_eq!(saved[0]["openai_compat"]["lm_studio_model"], "m");
    }

    /// The endpoint prompt offers `none` while the gateway is on and the
    /// preset's default otherwise.
    #[test]
    fn the_endpoint_prompt_offers_none_while_the_gateway_is_on() {
        let mut state = make_state(OpenAiCompatPreset::Ollama, None, None, None, OpenAiCompatSetupStep::AwaitingEndpoint);
        let p = endpoint_prompt(&state, false);
        assert_eq!(p.prompt, "Enter the Ollama endpoint URL (current: <unconfigured>):");
        assert_eq!(p.default_value, "http://localhost:11434");
        state.gateway = Some(gw("http://gw:4000"));
        let p = endpoint_prompt(&state, false);
        assert_eq!(p.prompt, "Enter the Ollama endpoint URL for when the gateway is off (current: none; none is allowed):");
        assert_eq!(p.default_value, "none");
        state.current_endpoint = Some("http://mac:11434".into());
        let p = endpoint_prompt(&state, true);
        assert_eq!(p.prompt, "Re-enter the Ollama endpoint URL (none is allowed):");
        assert_eq!(p.default_value, "http://mac:11434");
    }

    /// `/model` lists from the routed base and never from the preset's
    /// default: a preset without a direct endpoint has nothing to list
    /// while the gateway is off.
    #[test]
    fn a_preset_without_a_direct_path_is_not_listed_while_the_gateway_is_off() {
        let mut cfg = crate::config::tests_support::minimal_cfg();
        cfg.openai_compat.ollama_model = "m".into();
        assert_eq!(local_route(&cfg, OpenAiCompatPreset::Ollama), None);
        cfg.provider_gateway = Some(gw("http://gw:4000"));
        assert_eq!(local_route(&cfg, OpenAiCompatPreset::Ollama), Some("http://gw:4000/ollama".to_string()));
        cfg.provider_gateway.as_mut().unwrap().enabled = false;
        cfg.openai_compat.ollama_endpoint = "http://mac:11434".into();
        assert_eq!(local_route(&cfg, OpenAiCompatPreset::Ollama), Some("http://mac:11434".to_string()));
    }
}

#[cfg(test)]
mod reasoning_delta_privacy_tests {
    use super::*;
    use crate::provider::{AssistantTurn, Block, StreamEvent, TurnOutcome};
    use embra_common::proto::brain;
    use futures::stream;
    use tokio::sync::mpsc;

    /// Drive `collect_response` with a synthetic stream that interleaves
    /// `TextDelta` and `ReasoningDelta` events. The load-bearing
    /// privacy assertion: reasoning text MUST NOT appear in the
    /// `StreamDone.full_response` payload (which is what `turn_text`
    /// would later persist to session history).
    #[tokio::test]
    async fn reasoning_delta_does_not_leak_into_full_response() {
        let secret = "INTERNAL CHAIN OF THOUGHT — DO NOT PERSIST";
        let visible = "Hello operator.";
        let events: Vec<StreamEvent> = vec![
            StreamEvent::ReasoningDelta(secret.to_string()),
            StreamEvent::TextDelta(visible.to_string()),
            StreamEvent::ReasoningDelta(" more secret reasoning".to_string()),
            StreamEvent::Complete(AssistantTurn {
                content: vec![Block::Text(visible.to_string())],
                outcome: TurnOutcome::EndTurn,
                stop_details: None,
            }),
        ];
        let brain_rx = Box::pin(stream::iter(events));
        let (tx, mut rx) = mpsc::channel(64);

        let turn = collect_response(brain_rx, &tx, "Embra", None)
            .await
            .expect("collect_response ok")
            .expect("got Complete turn");

        drop(tx);
        let mut frames = Vec::new();
        while let Some(frame) = rx.recv().await {
            frames.push(frame.expect("ok frame"));
        }

        // The terminal Done frame's full_response must contain the
        // visible text and NOTHING from reasoning.
        let done_payload: Vec<&str> = frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::Done(d)) => {
                    Some(d.full_response.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(done_payload.len(), 1, "expected one Done frame");
        assert_eq!(done_payload[0], visible);
        assert!(
            !done_payload[0].contains("INTERNAL"),
            "reasoning text leaked into Done.full_response: {:?}",
            done_payload[0]
        );

        // Reasoning frames went out as the dedicated proto variant.
        let reasoning_texts: Vec<&str> = frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::ReasoningDelta(r)) => {
                    Some(r.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning_texts,
            vec![secret, " more secret reasoning"],
            "reasoning shards should arrive as dedicated proto frames in order"
        );

        // Token frames carry only the visible text.
        let tokens: Vec<&str> = frames
            .iter()
            .filter_map(|f| match &f.response_type {
                Some(conversation_response::ResponseType::Token(t)) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tokens, vec![visible]);

        // The returned AssistantTurn used by the loop driver carries
        // only the visible Block::Text — reasoning was not assembled
        // into Block::Text, only sent as deltas.
        let turn_text_value = turn_text(&turn);
        assert_eq!(turn_text_value, visible);
        assert!(
            !turn_text_value.contains("INTERNAL"),
            "turn_text leaked reasoning"
        );
    }

    #[tokio::test]
    async fn reasoning_delta_does_not_toggle_first_token_thinking() {
        // The "Embra is thinking..." indicator is gated to the first
        // *real* TextDelta. A ReasoningDelta arriving first must NOT
        // toggle is_thinking=false — that toggle is what dismisses the
        // operator-facing typing animation.
        let events: Vec<StreamEvent> = vec![
            StreamEvent::ReasoningDelta("thinking out loud".to_string()),
            StreamEvent::TextDelta("real text".to_string()),
            StreamEvent::Complete(AssistantTurn {
                content: vec![Block::Text("real text".to_string())],
                outcome: TurnOutcome::EndTurn,
                stop_details: None,
            }),
        ];
        let brain_rx = Box::pin(stream::iter(events));
        let (tx, mut rx) = mpsc::channel(64);

        let _ = collect_response(brain_rx, &tx, "Embra", None).await.unwrap();
        drop(tx);

        let mut frames = Vec::new();
        while let Some(frame) = rx.recv().await {
            frames.push(frame.expect("ok frame"));
        }

        // Find the index of the first Thinking{is_thinking:false}
        // frame and the first ReasoningDelta frame. The ReasoningDelta
        // must come first; the thinking-off toggle must come AFTER the
        // first TextDelta.
        let mut first_thinking_off: Option<usize> = None;
        let mut first_reasoning: Option<usize> = None;
        let mut first_token: Option<usize> = None;
        for (i, frame) in frames.iter().enumerate() {
            match &frame.response_type {
                Some(conversation_response::ResponseType::Thinking(t)) if !t.is_thinking => {
                    first_thinking_off.get_or_insert(i);
                }
                Some(conversation_response::ResponseType::ReasoningDelta(_)) => {
                    first_reasoning.get_or_insert(i);
                }
                Some(conversation_response::ResponseType::Token(_)) => {
                    first_token.get_or_insert(i);
                }
                _ => {}
            }
        }

        let r = first_reasoning.expect("expected ReasoningDelta frame");
        let t = first_token.expect("expected Token frame");
        let off = first_thinking_off.expect("expected thinking-off frame");
        assert!(
            r < off,
            "ReasoningDelta arrived AFTER thinking-off (frames: {frames:#?})"
        );
        assert!(
            off <= t,
            "thinking-off should fire on first Token, not ReasoningDelta"
        );
    }

    #[test]
    fn show_reasoning_helper_defaults_to_true() {
        let mut cfg = config::SystemConfig {
            name: "Embra".to_string(),
            api_key: String::new(),
            timezone: "UTC".to_string(),
            deployment_mode: "test".to_string(),
            created_at: String::new(),
            version: String::new(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: "anthropic".to_string(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            guardian_secrets: None,
            guardian_private_hosts: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            provider_gateway: None,
            openai_compat: config::OpenAiCompatConfig::default(),
        };
        assert!(cfg.show_reasoning(), "None should default to on");
        cfg.show_reasoning = Some(true);
        assert!(cfg.show_reasoning());
        cfg.show_reasoning = Some(false);
        assert!(!cfg.show_reasoning());
    }

    // Suppress unused-import warning when this module is built but not
    // exercised in feature combinations.
    #[allow(dead_code)]
    fn _unused() {
        let _ = brain::ReasoningDelta {
            text: String::new(),
        };
    }
}

#[cfg(test)]
mod soul_sealed_mode_change_tests {
    //! Guards the Learning→Operational `ModeChange` shared by the
    //! learning-completion path and the onboarding-stage wake arm added in
    //! `embra-brain-broadcaster`. Clients parse this message for the status
    //! bar, so the `Soul sealed — Name: … — TZ: … — Brain: …` shape and the
    //! from/to modes are load-bearing.
    use super::soul_sealed_mode_change;
    use crate::config::{OpenAiCompatConfig, SystemConfig};
    use embra_common::proto::brain::{
        ConversationResponse, ModeTransition, OperatingMode, conversation_response::ResponseType,
    };

    fn cfg(name: &str, tz: &str, provider: &str) -> SystemConfig {
        SystemConfig {
            name: name.to_string(),
            api_key: String::new(),
            timezone: tz.to_string(),
            deployment_mode: "phase1".into(),
            created_at: String::new(),
            version: "test".into(),
            github_token: None,
            kg_temporal_window_secs: 1800,
            kg_max_traversal_depth: 3,
            kg_traversal_depth_ceiling: 5,
            kg_edge_candidate_limit: 50,
            kg_traversal_edge_limit: 500,
            kg_traversal_node_budget: 1000,
            api_provider: provider.to_string(),
            gemini_model: None,
            anthropic_model: None,
            anthropic_effort: None,
            gemini_effort: None,
            embedding_enabled: None,
            embedding_model: None,
            image_provider: None,
            image_model: None,
            git_tokens: None,
            guardian_secrets: None,
            guardian_private_hosts: None,
            anthropic_api_key: None,
            gemini_api_key: None,
            max_tool_iterations: None,
            show_reasoning: None,
            provider_gateway: None,
            openai_compat: OpenAiCompatConfig::default(),
        }
    }

    fn mode_change(resp: &ConversationResponse) -> &ModeTransition {
        match resp.response_type.as_ref().expect("response_type is set") {
            ResponseType::ModeChange(m) => m,
            other => panic!("expected ModeChange, got {other:?}"),
        }
    }

    #[test]
    fn announces_learning_to_operational() {
        let resp = soul_sealed_mode_change(&cfg("Aria", "America/New_York", "anthropic"));
        let m = mode_change(&resp);
        assert_eq!(m.from_mode, OperatingMode::Learning as i32);
        assert_eq!(m.to_mode, OperatingMode::Operational as i32);
    }

    #[test]
    fn message_carries_name_tz_and_brain_for_status_bar() {
        let resp = soul_sealed_mode_change(&cfg("Aria", "America/New_York", "anthropic"));
        let msg = &mode_change(&resp).message;
        assert!(msg.contains("Soul sealed"), "got: {msg}");
        assert!(msg.contains("Name: Aria"), "got: {msg}");
        assert!(msg.contains("TZ: America/New_York"), "got: {msg}");
        // anthropic default display model
        assert!(msg.contains("Brain: opus-5"), "got: {msg}");
    }

    #[test]
    fn brain_field_shows_the_selected_gemini_model() {
        let mut c = cfg("Embra", "Etc/UTC", "gemini");
        c.gemini_model = Some("gemini-4-argon".into());
        let resp = soul_sealed_mode_change(&c);
        assert!(
            mode_change(&resp).message.contains("Brain: gemini-4-argon"),
            "got: {}",
            mode_change(&resp).message
        );
    }

    #[test]
    fn brain_field_reflects_provider() {
        let resp = soul_sealed_mode_change(&cfg("Embra", "Etc/UTC", "gemini"));
        assert!(
            mode_change(&resp).message.contains("Brain: gemini-3.1-pro"),
            "got: {}",
            mode_change(&resp).message
        );
    }
}

#[cfg(test)]
mod anthropic_model_tests {
    //! Anthropic model resolution + the `/model` choice parser (added in
    //! `embra-brain-broadcaster`; Fable 5 + the 4.8 default landed on
    //! `llm-provider-optimizations`). The API id is load-bearing (sent in
    //! the request body); the inner resolver is pure so the env var isn't
    //! mutated across the suite (same discipline as the Gemini resolver).
    use super::{
        canonicalize_legacy_anthropic_alias, normalize_anthropic_model,
        parse_anthropic_model_choice, resolve_anthropic_model_inner, ANTHROPIC_MODEL_CHOICES,
    };

    /// `/model` prints `ANTHROPIC_MODEL_CHOICES`; every entry must be the
    /// canonical display of a selectable arm.
    #[test]
    fn listed_choices_are_the_canonical_displays() {
        for choice in ANTHROPIC_MODEL_CHOICES {
            let (_, display) = parse_anthropic_model_choice(choice)
                .unwrap_or_else(|| panic!("'{choice}' is listed but not selectable"));
            assert_eq!(display, choice);
        }
    }

    /// The wizard persists the alias `anthropic_model_from_label` returns.
    /// An alias with no selectable arm would reach the API as a raw model
    /// id and 400 every turn.
    #[test]
    fn wizard_seeded_aliases_are_selectable() {
        for label in crate::config::PROVIDER_WIZARD_LABELS {
            let Some(alias) = crate::config::anthropic_model_from_label(label) else {
                continue;
            };
            let (_, display) = parse_anthropic_model_choice(&alias).unwrap_or_else(|| {
                panic!("wizard label '{label}' seeds '{alias}', which /model does not accept")
            });
            assert_eq!(display, alias, "{label}");
            assert!(ANTHROPIC_MODEL_CHOICES.contains(&display), "{label}");
        }
    }

    #[test]
    fn defaults_to_opus_5_when_unset() {
        let (id, disp) = resolve_anthropic_model_inner(None, None);
        assert_eq!(id, "claude-opus-5");
        assert_eq!(disp, "opus-5");
    }

    #[test]
    fn config_field_selects_opus_5() {
        let (id, disp) = resolve_anthropic_model_inner(None, Some("opus-5"));
        assert_eq!(id, "claude-opus-5");
        assert_eq!(disp, "opus-5");
    }

    #[test]
    fn config_field_selects_fable_5() {
        let (id, disp) = resolve_anthropic_model_inner(None, Some("fable-5"));
        assert_eq!(id, "claude-fable-5");
        assert_eq!(disp, "fable-5");
    }

    #[test]
    fn env_overrides_config() {
        let (id, _) = resolve_anthropic_model_inner(Some("fable-5"), Some("opus-5"));
        assert_eq!(id, "claude-fable-5");
    }

    #[test]
    fn blank_values_fall_through_to_default() {
        let (id, _) = resolve_anthropic_model_inner(Some("  "), Some(""));
        assert_eq!(id, "claude-opus-5");
    }

    #[test]
    fn aliases_canonicalize() {
        for s in ["opus-5", "opus5", "opus", "5", "claude-opus-5", "OPUS-5"] {
            assert_eq!(
                parse_anthropic_model_choice(s),
                Some(("claude-opus-5", "opus-5")),
                "input: {s}"
            );
        }
        for s in ["opus-4.8", "4.8", "opus4.8", "claude-opus-4-8", "OPUS-4.8"] {
            assert_eq!(
                parse_anthropic_model_choice(s),
                Some(("claude-opus-4-8", "opus-4.8")),
                "input: {s}"
            );
        }
        for s in ["fable-5", "fable5", "fable", "claude-fable-5", "FABLE-5"] {
            assert_eq!(
                parse_anthropic_model_choice(s),
                Some(("claude-fable-5", "fable-5")),
                "input: {s}"
            );
        }
        for s in ["opus-5.5", "opus5.5", "5.5", "claude-opus-5-5", "OPUS-5.5"] {
            assert_eq!(
                parse_anthropic_model_choice(s),
                Some(("claude-opus-5-5", "opus-5.5")),
                "input: {s}"
            );
        }
        for s in ["fable-5.1", "fable5.1", "claude-fable-5-1", "FABLE-5.1"] {
            assert_eq!(
                parse_anthropic_model_choice(s),
                Some(("claude-fable-5-1", "fable-5.1")),
                "input: {s}"
            );
        }
    }

    /// `5.1` alone is not an alias: numeric-only aliases name Opus
    /// versions (`5`, `5.5`, `4.8`), and a bare `5.1` would read as one.
    #[test]
    fn bare_5_1_is_not_an_alias() {
        assert_eq!(parse_anthropic_model_choice("5.1"), None);
    }

    #[test]
    fn config_field_selects_opus_4_8() {
        let (id, disp) = resolve_anthropic_model_inner(None, Some("opus-4.8"));
        assert_eq!(id, "claude-opus-4-8");
        assert_eq!(disp, "opus-4.8");
    }

    #[test]
    fn config_field_selects_opus_5_5() {
        let (id, disp) = resolve_anthropic_model_inner(None, Some("opus-5.5"));
        assert_eq!(id, "claude-opus-5-5");
        assert_eq!(disp, "opus-5.5");
    }

    #[test]
    fn config_field_selects_fable_5_1() {
        let (id, disp) = resolve_anthropic_model_inner(None, Some("fable-5.1"));
        assert_eq!(id, "claude-fable-5-1");
        assert_eq!(disp, "fable-5.1");
    }

    /// The 400-trap guard: the persisted `"opus-4.7"` alias (wizard-seeded
    /// pre-2026-07-06, or the frozen v9 backfill) is no longer SELECTABLE
    /// but must keep RESOLVING to its real API id — a bare-alias
    /// passthrough would 400 every turn on an existing instance.
    #[test]
    fn legacy_aliases_resolve_but_are_not_selectable() {
        for (s, id, disp) in [
            ("opus-4.7", "claude-opus-4-7", "opus-4.7"),
            ("4.7", "claude-opus-4-7", "opus-4.7"),
            ("claude-opus-4-7", "claude-opus-4-7", "opus-4.7"),
        ] {
            assert_eq!(parse_anthropic_model_choice(s), None, "selectable: {s}");
            assert_eq!(
                canonicalize_legacy_anthropic_alias(s),
                Some((id, disp)),
                "legacy: {s}"
            );
            let (rid, rdisp) = normalize_anthropic_model(s);
            assert_eq!((rid.as_str(), rdisp.as_str()), (id, disp), "resolve: {s}");
        }
        // Full resolver path: a persisted legacy config keeps working.
        let (id, disp) = resolve_anthropic_model_inner(None, Some("opus-4.7"));
        assert_eq!(id, "claude-opus-4-7");
        assert_eq!(disp, "opus-4.7");
    }

    #[test]
    fn unknown_choice_rejected_but_resolver_passes_through() {
        // `/model` rejects unknown choices…
        assert_eq!(parse_anthropic_model_choice("opus-9"), None);
        // …but env/config passthrough allows a future id this build predates.
        let (id, disp) = normalize_anthropic_model("claude-opus-6");
        assert_eq!(id, "claude-opus-6");
        assert_eq!(disp, "claude-opus-6");
    }
}

#[cfg(test)]
mod gemini_model_tests {
    //! The Gemini line-up: `/model` choices, aliases and the arm of
    //! `handle_model_command` that saves them.
    use super::*;
    use embra_common::proto::brain::ConversationResponse;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn gemini_aliases_canonicalize() {
        for (input, id, alias) in [
            ("gemini-3.1-pro", "gemini-3.1-pro-preview", "gemini-3.1-pro"),
            ("3.1-pro", "gemini-3.1-pro-preview", "gemini-3.1-pro"),
            ("Gemini-3.1-Pro-Preview", "gemini-3.1-pro-preview", "gemini-3.1-pro"),
            (" argon ", "gemini-4-argon", "gemini-4-argon"),
            ("4-argon", "gemini-4-argon", "gemini-4-argon"),
            ("gemini-4-argon", "gemini-4-argon", "gemini-4-argon"),
        ] {
            assert_eq!(parse_gemini_model_choice(input), Some((id, alias)), "{input}");
        }
    }

    #[test]
    fn an_unknown_gemini_model_is_not_selectable() {
        for input in ["gemini-3.1-pro-preview-customtools", "gemini-4", "3.1", "opus-5", ""] {
            assert_eq!(parse_gemini_model_choice(input), None, "{input}");
        }
        // Not selectable, but sent as typed when it comes from env or config.
        assert_eq!(
            resolve_gemini_model_id_inner(None, Some("gemini-3.1-pro-preview-customtools")),
            "gemini-3.1-pro-preview-customtools"
        );
    }

    #[test]
    fn gemini_wizard_seeded_aliases_are_selectable() {
        for label in crate::config::GEMINI_WIZARD_LABELS {
            let alias = crate::config::gemini_model_from_label(label).expect(label);
            let (_, parsed) = parse_gemini_model_choice(&alias).unwrap_or_else(|| {
                panic!("wizard label '{label}' seeds '{alias}', which /model does not accept")
            });
            assert_eq!(parsed, alias, "{label}");
            assert!(GEMINI_MODEL_CHOICES.contains(&parsed), "{label}");
        }
    }

    #[test]
    fn listed_gemini_choices_are_the_canonical_displays() {
        for choice in GEMINI_MODEL_CHOICES {
            let (id, alias) = parse_gemini_model_choice(choice).expect(choice);
            assert_eq!(alias, choice);
            assert_eq!(crate::provider::gemini::display_name_for(id), choice);
        }
    }

    /// A WardSONDB that holds a Gemini config and accepts a save.
    async fn gemini_config_db() -> MockServer {
        let db_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {
                "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
                "created_at": "", "version": "test", "api_provider": "gemini",
            }})))
            .mount(&db_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/config.system"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {}})))
            .mount(&db_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {"_id": "config"}})))
            .mount(&db_server)
            .await;
        db_server
    }

    /// Run `/model <args>` and return what was said, the status bar's
    /// ModeChange messages, and the config documents saved.
    async fn run_model_command(
        db_server: &MockServer,
        args: &str,
    ) -> (Vec<String>, Vec<String>, Vec<serde_json::Value>) {
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let session_mgr = Arc::new(RwLock::new(SessionManager::new(WardsonDbClient::from_url(
            &db_server.uri(),
        ))));
        let pending = Arc::new(Mutex::new(None));
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        handle_model_command(args, &tx, &db, &session_mgr, &pending).await;
        drop(tx);
        let (mut said, mut status) = (Vec::new(), Vec::new());
        while let Some(Ok(r)) = rx.recv().await {
            match r.response_type {
                Some(conversation_response::ResponseType::System(m)) => said.push(m.content),
                Some(conversation_response::ResponseType::ModeChange(m)) => status.push(m.message),
                _ => {}
            }
        }
        let saved = db_server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| serde_json::from_slice(&r.body).expect("a config document"))
            .collect();
        (said, status, saved)
    }

    #[tokio::test]
    async fn a_gemini_model_pick_is_saved_probed_and_shown() {
        let db_server = gemini_config_db().await;
        let (said, status, saved) = run_model_command(&db_server, "argon").await;
        assert_eq!(
            said,
            vec!["Gemini model set to gemini-4-argon. Takes effect on your next message.".to_string()]
        );
        assert_eq!(status.len(), 1);
        assert!(status[0].contains("Brain: gemini-4-argon"), "{}", status[0]);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["gemini_model"], "gemini-4-argon");
        assert_eq!(saved[0]["api_provider"], "gemini");

        // The alias is saved, not the API id (`gemini-3.1-pro-preview`):
        // the resolver gives the turn its id.
        let db_server = gemini_config_db().await;
        let (said, _, saved) = run_model_command(&db_server, "3.1-pro").await;
        assert_eq!(
            said,
            vec!["Gemini model set to gemini-3.1-pro. Takes effect on your next message.".to_string()]
        );
        assert_eq!(saved[0]["gemini_model"], "gemini-3.1-pro");
    }

    #[tokio::test]
    async fn an_unknown_gemini_model_saves_nothing() {
        let db_server = gemini_config_db().await;
        let (said, status, saved) = run_model_command(&db_server, "gemini-5").await;
        assert_eq!(
            said,
            vec!["'gemini-5' is not a recognized Gemini model. Options: gemini-3.1-pro, gemini-4-argon.".to_string()]
        );
        assert!(status.is_empty());
        assert!(saved.is_empty());

        // No name: the current model and the options, nothing saved.
        let (said, _, saved) = run_model_command(&db_server, "").await;
        assert!(said[0].starts_with("Gemini model: gemini-3.1-pro. Options: gemini-3.1-pro, gemini-4-argon."), "{}", said[0]);
        assert!(saved.is_empty());
    }
}

#[cfg(test)]
mod key_setup_model_step_tests {
    //! The model step that ends `/provider --setup <anthropic|gemini>`.
    use super::*;
    use embra_common::proto::brain::ConversationResponse;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(provider: &str, anthropic: Option<&str>, gemini: Option<&str>) -> config::SystemConfig {
        let mut v = json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
            "created_at": "", "version": "test", "api_provider": provider,
        });
        if let Some(a) = anthropic {
            v["anthropic_model"] = json!(a);
        }
        if let Some(g) = gemini {
            v["gemini_model"] = json!(g);
        }
        serde_json::from_value(v).expect("a config")
    }

    fn labels(list: &[&str]) -> Vec<String> {
        list.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn a_saved_key_leads_to_the_model_step_on_both_cloud_providers() {
        for kind in [ProviderKind::Anthropic, ProviderKind::Gemini] {
            assert_eq!(step_after_saved_key(kind), Some(KeySetupStep::Model(kind)));
        }
        assert_eq!(step_after_saved_key(ProviderKind::Ollama), None);
        assert_eq!(step_after_saved_key(ProviderKind::LmStudio), None);
    }

    #[test]
    fn the_model_step_puts_the_current_model_first() {
        let p = cloud_model_selection(ProviderKind::Anthropic, &cfg("anthropic", Some("opus-5.5"), None));
        assert_eq!(p.field_type, SetupFieldType::Selector as i32);
        assert_eq!(p.options.len(), 5);
        assert_eq!(p.options[0], "Anthropic Claude Opus 5.5");
        assert_eq!(p.default_value, "Anthropic Claude Opus 5.5");

        let p = cloud_model_selection(ProviderKind::Gemini, &cfg("gemini", None, Some("gemini-4-argon")));
        assert_eq!(p.options, labels(&["Google Gemini 4 Argon", "Google Gemini 3.1 Pro"]));
        // No stored choice: the default is current, and it leads already.
        let p = cloud_model_selection(ProviderKind::Gemini, &cfg("gemini", None, None));
        assert_eq!(p.options, labels(&["Google Gemini 3.1 Pro", "Google Gemini 4 Argon"]));

        // A current model that no label names keeps the wizard order and
        // is named in the prompt.
        let p = cloud_model_selection(ProviderKind::Anthropic, &cfg("anthropic", Some("opus-4.7"), None));
        assert_eq!(p.options, labels(&config::ANTHROPIC_WIZARD_LABELS));
        assert!(p.prompt.contains("opus-4.7"), "{}", p.prompt);
        let p = cloud_model_selection(
            ProviderKind::Gemini,
            &cfg("gemini", None, Some("gemini-3.1-pro-preview-customtools")),
        );
        assert_eq!(p.options, labels(&config::GEMINI_WIZARD_LABELS));
        assert!(p.prompt.contains("customtools"), "{}", p.prompt);
    }

    #[test]
    fn a_pick_maps_a_wizard_label_or_a_typed_alias() {
        let (a, g) = (ProviderKind::Anthropic, ProviderKind::Gemini);
        assert_eq!(model_alias_for_pick(a, "Anthropic Claude Fable 5.1").as_deref(), Some("fable-5.1"));
        assert_eq!(model_alias_for_pick(a, " opus-5 ").as_deref(), Some("opus-5"));
        assert_eq!(model_alias_for_pick(a, "Google Gemini 4 Argon"), None);
        assert_eq!(model_alias_for_pick(g, "Google Gemini 4 Argon").as_deref(), Some("gemini-4-argon"));
        assert_eq!(model_alias_for_pick(g, "3.1-pro").as_deref(), Some("gemini-3.1-pro"));
        assert_eq!(model_alias_for_pick(g, "Anthropic Claude Opus 5"), None);
        assert_eq!(model_alias_for_pick(g, "hello there"), None);
    }

    #[test]
    fn a_slash_command_cancels_the_model_step_but_not_the_key_step() {
        let key = Some(KeySetupStep::Key(ProviderKind::Gemini));
        assert_eq!(key_setup_after_slash(key), (key, None));
        assert_eq!(
            key_setup_after_slash(Some(KeySetupStep::Model(ProviderKind::Anthropic))),
            (None, Some(ProviderKind::Anthropic))
        );
        assert_eq!(key_setup_after_slash(None), (None, None));
    }

    /// A WardSONDB that holds a config on `provider` and accepts a save.
    async fn config_db(provider: &str) -> MockServer {
        let db_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config.system/docs/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {
                "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase1",
                "created_at": "", "version": "test", "api_provider": provider,
            }})))
            .mount(&db_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/config.system"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {}})))
            .mount(&db_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/config.system/docs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "meta": {}, "data": {"_id": "config"}})))
            .mount(&db_server)
            .await;
        db_server
    }

    /// Answer the model step and return what was said, the status bar's
    /// messages, the configs saved and the step left pending.
    async fn run_pick(
        db_server: &MockServer,
        target: ProviderKind,
        answer: &str,
    ) -> (Vec<String>, Vec<String>, Vec<serde_json::Value>, Option<KeySetupStep>) {
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let session_mgr = Arc::new(RwLock::new(SessionManager::new(WardsonDbClient::from_url(
            &db_server.uri(),
        ))));
        let pending = Arc::new(Mutex::new(Some(KeySetupStep::Model(target))));
        let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
        handle_setup_model_pick(target, answer, &tx, &db, &session_mgr, &pending).await;
        drop(tx);
        let (mut said, mut status) = (Vec::new(), Vec::new());
        while let Some(Ok(r)) = rx.recv().await {
            match r.response_type {
                Some(conversation_response::ResponseType::System(m)) => said.push(m.content),
                Some(conversation_response::ResponseType::ModeChange(m)) => status.push(m.message),
                _ => {}
            }
        }
        let saved = db_server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| serde_json::from_slice(&r.body).expect("a config document"))
            .collect();
        let left = *pending.lock().await;
        (said, status, saved, left)
    }

    #[tokio::test]
    async fn a_setup_pick_is_saved_and_ends_the_flow() {
        // Gemini is the active provider: the pick takes effect at once.
        let db_server = config_db("gemini").await;
        let (said, status, saved, left) =
            run_pick(&db_server, ProviderKind::Gemini, "Google Gemini 4 Argon").await;
        assert_eq!(
            said,
            vec!["Gemini model set to gemini-4-argon. Takes effect on your next message.".to_string()]
        );
        assert!(status[0].contains("Brain: gemini-4-argon"), "{}", status[0]);
        assert_eq!(saved[0]["gemini_model"], "gemini-4-argon");
        assert_eq!(left, None);

        // Anthropic set up while Gemini is active: saved for a later switch.
        let db_server = config_db("gemini").await;
        let (said, status, saved, left) =
            run_pick(&db_server, ProviderKind::Anthropic, "Anthropic Claude Opus 5.5").await;
        assert_eq!(
            said,
            vec!["Anthropic model set to opus-5.5. Use /provider anthropic to switch, or stay on the current provider.".to_string()]
        );
        assert!(status.is_empty());
        assert_eq!(saved[0]["anthropic_model"], "opus-5.5");
        assert_eq!(left, None);
    }

    /// #20: `/provider --setup anthropic|gemini` asks for the key with the
    /// `Secret` field type, so neither client shows what is typed.
    #[tokio::test]
    async fn provider_setup_asks_for_the_key_with_a_secret_field() {
        for (target, arg, word) in [
            (ProviderKind::Anthropic, "--setup anthropic", "Anthropic"),
            (ProviderKind::Gemini, "--setup gemini", "Gemini"),
        ] {
            let db_server = config_db("gemini").await;
            let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
            let session_mgr = Arc::new(RwLock::new(SessionManager::new(
                WardsonDbClient::from_url(&db_server.uri()),
            )));
            let pending_key_setup = Arc::new(Mutex::new(None));
            let ctx = ServiceCtx {
                db,
                session_mgr,
                in_turn: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pending_provider: Arc::new(Mutex::new(None)),
                pending_key_setup: pending_key_setup.clone(),
                pending_openai_compat_setup: Arc::new(Mutex::new(None)),
                pending_session_delete: Arc::new(Mutex::new(None)),
            };
            let (tx, mut rx) = mpsc::channel::<Result<ConversationResponse, Status>>(64);
            handle_provider_command(arg, &tx, &ctx).await;
            drop(tx);
            let mut prompt = None;
            while let Some(Ok(r)) = rx.recv().await {
                if let Some(conversation_response::ResponseType::Setup(p)) = r.response_type {
                    prompt = Some(p);
                }
            }
            let p = prompt.expect("a key prompt");
            assert!(p.prompt.contains(word) && p.prompt.contains("API key"), "{}", p.prompt);
            assert_eq!(p.field_type, SetupFieldType::Secret as i32);
            assert_eq!(*pending_key_setup.lock().await, Some(KeySetupStep::Key(target)));
        }
    }

    #[tokio::test]
    async fn an_unknown_setup_pick_saves_nothing_and_ends_the_flow() {
        let db_server = config_db("gemini").await;
        let (said, status, saved, left) =
            run_pick(&db_server, ProviderKind::Gemini, "what is the weather").await;
        assert_eq!(
            said,
            vec!["'what is the weather' is not a Gemini model; the model stays gemini-3.1-pro. /model changes it later.".to_string()]
        );
        assert!(status.is_empty() && saved.is_empty());
        assert_eq!(left, None);
    }
}

#[cfg(test)]
mod anthropic_effort_tests {
    //! `/effort` allowlist + resolver. Unlike the model resolver there is
    //! deliberately NO unknown-value passthrough — an invalid effort would
    //! 400 every turn, so bogus env/config values fall through to `"max"`.
    use super::{parse_effort_choice, resolve_anthropic_effort_inner};

    #[test]
    fn effort_allowlist_accepts_five_levels_rejects_junk() {
        for s in ["low", "medium", "high", "xhigh", "max", " HIGH ", "Max"] {
            assert!(
                parse_effort_choice(s).is_some(),
                "should accept: {s}"
            );
        }
        for s in ["", "maximum", "x-high", "ultra", "0.8"] {
            assert_eq!(parse_effort_choice(s), None, "should reject: {s}");
        }
    }

    #[test]
    fn resolve_effort_defaults_to_max_and_env_overrides_config() {
        assert_eq!(resolve_anthropic_effort_inner(None, None), "max");
        assert_eq!(resolve_anthropic_effort_inner(None, Some("high")), "high");
        assert_eq!(
            resolve_anthropic_effort_inner(Some("low"), Some("high")),
            "low"
        );
        // Invalid values fall through to the next source, then the default.
        assert_eq!(
            resolve_anthropic_effort_inner(Some("bogus"), Some("xhigh")),
            "xhigh"
        );
        assert_eq!(
            resolve_anthropic_effort_inner(Some("bogus"), Some("also-bogus")),
            "max"
        );
    }

    #[test]
    fn optional_effort_resolver_has_no_default_and_env_wins() {
        // Gemini / OpenAI-compat: `None` = provider default (byte-identical
        // request); env > config; invalid values fall through.
        use super::resolve_optional_effort_inner as r;
        assert_eq!(r(None, None), None);
        assert_eq!(r(None, Some("xhigh")).as_deref(), Some("xhigh"));
        assert_eq!(r(Some("low"), Some("xhigh")).as_deref(), Some("low"));
        assert_eq!(r(Some("bogus"), Some("medium")).as_deref(), Some("medium"));
        assert_eq!(r(Some("bogus"), Some("nope")), None);
    }
}

#[cfg(test)]
mod terminal_outcome_tests {
    //! Refusal-as-error + max_tokens truncation surfacing. A terminal
    //! refusal must produce an Error frame (never a silent `(no response)`
    //! persist, never a fallback-model retry) and honest history text;
    //! max_tokens produces a Warning frame. Pure-helper tests — the loop
    //! driver has no full fake-provider harness, so `terminal_outcome_notice`
    //! and `final_assistant_text` are the designed test seams.
    use super::{final_assistant_text, terminal_outcome_notice};
    use crate::provider::ir::{EarlyStopReason, StopDetails, TurnOutcome};
    use embra_common::proto::brain::SystemMessageType;

    #[test]
    fn refusal_notice_is_error_frame_with_category_and_explanation() {
        let details = StopDetails {
            category: Some("cyber".into()),
            explanation: Some("Request declined by safety classifier.".into()),
        };
        let (kind, msg) = terminal_outcome_notice(
            TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
            Some(&details),
        )
        .expect("refusal must produce a frame");
        assert_eq!(kind, SystemMessageType::Error);
        assert!(msg.contains("refusal, category: cyber"), "{msg}");
        assert!(msg.contains("Request declined by safety classifier."), "{msg}");
        assert!(msg.contains("No fallback model was used."), "{msg}");
    }

    #[test]
    fn refusal_notice_guards_missing_details() {
        // category can be null and explanation absent even on a refusal —
        // and stop_details itself can be missing entirely.
        for details in [
            None,
            Some(StopDetails {
                category: None,
                explanation: None,
            }),
        ] {
            let (kind, msg) = terminal_outcome_notice(
                TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
                details.as_ref(),
            )
            .expect("refusal must produce a frame");
            assert_eq!(kind, SystemMessageType::Error);
            assert!(msg.contains("(refusal)"), "{msg}");
            assert!(!msg.contains("category:"), "{msg}");
        }
    }

    #[test]
    fn max_tokens_notice_is_warning_frame() {
        let (kind, msg) =
            terminal_outcome_notice(TurnOutcome::MaxTokens, None).expect("frame");
        assert_eq!(kind, SystemMessageType::Warning);
        assert!(msg.contains("max_tokens"), "{msg}");

        // Non-refusal early stops get a generic Error frame (Gemini
        // Safety/Recitation/Malformed + Anthropic StopSequence).
        let (kind, msg) =
            terminal_outcome_notice(TurnOutcome::EarlyStop(EarlyStopReason::Safety), None)
                .expect("frame");
        assert_eq!(kind, SystemMessageType::Error);
        assert!(msg.contains("Safety"), "{msg}");
    }

    /// A context-window stop is truncation, not a refusal: a Warning with
    /// the operator's remedy, never the silent end of turn it used to be
    /// and never the generic "Provider stopped" Error.
    #[test]
    fn context_window_stop_warns_and_names_the_remedy() {
        let (kind, msg) = terminal_outcome_notice(
            TurnOutcome::EarlyStop(EarlyStopReason::ContextWindow),
            None,
        )
        .expect("frame");
        assert_eq!(kind, SystemMessageType::Warning);
        assert!(msg.contains("context window"), "{msg}");
        assert!(msg.contains("/new"), "{msg}");
        assert!(!msg.contains("Provider stopped"), "{msg}");
        // Persisted like any other truncation: the partial text, unmarked.
        assert_eq!(
            final_assistant_text(
                "partial",
                &[],
                TurnOutcome::EarlyStop(EarlyStopReason::ContextWindow)
            ),
            "partial"
        );
    }

    #[test]
    fn end_turn_tool_use_pause_produce_no_notice() {
        for outcome in [TurnOutcome::EndTurn, TurnOutcome::ToolUse, TurnOutcome::Pause] {
            assert_eq!(terminal_outcome_notice(outcome, None), None);
        }
    }

    #[test]
    fn a_stream_ended_turn_gets_a_warning_notice() {
        let outcome = TurnOutcome::EarlyStop(EarlyStopReason::StreamEnded);
        let (kind, msg) =
            terminal_outcome_notice(outcome, None).expect("a closed stream must produce a frame");
        assert_eq!(kind, SystemMessageType::Warning);
        assert!(msg.contains("closed the stream"), "{msg}");
        // The partial text is persisted unmarked, like MaxTokens.
        assert_eq!(final_assistant_text("partial", &[], outcome), "partial");
    }

    #[test]
    fn final_assistant_text_marks_refused_empty_turn() {
        // Empty refusal → explicit marker, taking precedence over the
        // tool-only placeholder.
        let text = final_assistant_text(
            "  ",
            &["web_search".to_string()],
            TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
        );
        assert_eq!(text, "(request declined by provider: refusal)");
    }

    #[test]
    fn final_assistant_text_appends_refusal_suffix_to_partial() {
        let text = final_assistant_text(
            "I was saying",
            &[],
            TurnOutcome::EarlyStop(EarlyStopReason::Refusal),
        );
        assert_eq!(
            text,
            "I was saying\n\n(response interrupted by provider refusal)"
        );
    }

    #[test]
    fn final_assistant_text_keeps_tool_placeholder_and_no_response_fallbacks() {
        // Non-refusal behavior is byte-identical to the pre-extraction
        // inline chain.
        assert_eq!(
            final_assistant_text("hello", &[], TurnOutcome::EndTurn),
            "hello"
        );
        assert_eq!(
            final_assistant_text("", &["time".to_string()], TurnOutcome::ToolUse),
            "[tool calls: time]"
        );
        assert_eq!(
            final_assistant_text("", &[], TurnOutcome::EndTurn),
            "(no response)"
        );
        // MaxTokens keeps the partial verbatim — the Warning frame carries
        // the truncation signal, not the transcript.
        assert_eq!(
            final_assistant_text("partial", &[], TurnOutcome::MaxTokens),
            "partial"
        );
    }
}

#[cfg(test)]
mod cross_provider_message_tests {
    use super::cross_provider_block_message;

    /// With a session attached, `/close` must come FIRST — naming
    /// `/provider` alone sends the operator into its
    /// "Cannot switch providers while a session is active" refusal.
    #[test]
    fn active_session_message_orders_close_provider_switch() {
        let msg = cross_provider_block_message("main", "anthropic", "gemini", Some("scratch"));
        assert_eq!(
            msg,
            "Session 'main' was recorded under anthropic; the active provider is gemini. \
             To open it: /close (detaches the active session 'scratch'), then /provider anthropic, \
             then /switch main. To stay on gemini instead: /new <name> for a fresh session."
        );
        // The order is the contract: /close strictly before /provider,
        // /provider strictly before /switch.
        let close = msg.find("/close").unwrap();
        let provider = msg.find("/provider").unwrap();
        let switch = msg.find("/switch").unwrap();
        assert!(close < provider && provider < switch);
    }

    /// With no active session (fresh attach, or right after /close),
    /// `/provider` works directly — no /close step, no false ceremony.
    #[test]
    fn no_active_session_message_skips_close() {
        let msg = cross_provider_block_message("main", "anthropic", "ollama", None);
        assert!(!msg.contains("/close"));
        assert!(msg.contains("To open it: /provider anthropic, then /switch main."));
        assert!(msg.contains("To stay on ollama instead: /new <name> for a fresh session."));
    }
}

#[cfg(test)]
mod resume_briefing_gate_tests {
    //! Threshold semantics of the resume-briefing idle gate
    //! (session-ux-fixes). The full attach flow (meta read + cooldown +
    //! synthetic dispatch) needs a live WardSONDB, so it is QEMU-verified;
    //! the pure decision fn is locked here.
    use super::{resume_briefing_idle_ok, RESUME_BRIEFING_MIN_IDLE_SECS};
    use chrono::{Duration, Utc};

    #[test]
    fn one_second_under_threshold_suppresses() {
        let now = Utc::now();
        let last_active = now - Duration::seconds(RESUME_BRIEFING_MIN_IDLE_SECS - 1);
        let (ok, idle) = resume_briefing_idle_ok(last_active, now);
        assert!(!ok, "idle below the threshold must suppress the briefing");
        assert_eq!(idle, RESUME_BRIEFING_MIN_IDLE_SECS - 1);
    }

    #[test]
    fn exactly_threshold_fires() {
        let now = Utc::now();
        let last_active = now - Duration::seconds(RESUME_BRIEFING_MIN_IDLE_SECS);
        let (ok, idle) = resume_briefing_idle_ok(last_active, now);
        assert!(ok, "idle at the threshold must fire (>= comparison)");
        assert_eq!(idle, RESUME_BRIEFING_MIN_IDLE_SECS);
    }

    #[test]
    fn future_last_active_suppresses() {
        // Clock skew / a just-written last_active slightly ahead of `now`
        // must read as not-idle, never as a giant idle span.
        let now = Utc::now();
        let last_active = now + Duration::seconds(30);
        let (ok, idle) = resume_briefing_idle_ok(last_active, now);
        assert!(!ok, "negative idle must suppress");
        assert!(idle < 0, "the reported idle must expose the skew for the log line");
    }
}

#[cfg(test)]
mod no_session_message_tests {
    use super::no_session_block_message;

    /// With a resumable session on record, resuming it is the first
    /// suggestion (named /switch), starting fresh the second.
    #[test]
    fn names_most_recent_session_switch_before_new() {
        let msg = no_session_block_message(Some("scratch"));
        assert_eq!(
            msg,
            "No session is attached, so this message was not processed. \
             To resume 'scratch': /switch scratch. To start fresh: /new <name>. \
             /sessions lists all sessions."
        );
        // The order is the contract: resume the named session before
        // the start-fresh alternative.
        let switch = msg.find("/switch").unwrap();
        let new = msg.find("/new").unwrap();
        assert!(switch < new);
        assert!(msg.contains("/sessions"));
    }

    /// With nothing resumable (e.g. the only session was /close'd —
    /// Closed sessions are filtered by get_most_recent_active), the
    /// message offers only /new; no dangling /switch to nowhere.
    #[test]
    fn without_a_session_offers_only_new() {
        let msg = no_session_block_message(None);
        assert!(!msg.contains("/switch"));
        assert!(msg.contains("was not processed"));
        assert!(msg.contains("/new <name>"));
        assert!(msg.contains("/sessions lists all sessions"));
    }
}

#[cfg(test)]
mod local_timestamp_tests {
    //! The /sessions last-active render — configured-timezone display
    //! with an explicit-UTC fallback (session-ux-fixes follow-up).
    use super::format_local_timestamp;
    use chrono::{DateTime, Utc};

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn iana_zone_renders_local_with_abbreviation() {
        // Winter instant — New York is UTC-5 (EST).
        let out = format_local_timestamp(utc("2026-01-15T15:00:00Z"), "America/New_York");
        assert_eq!(out, "2026-01-15 10:00 EST");
    }

    #[test]
    fn dst_shifts_the_offset_and_label() {
        // Summer instant — the same zone is UTC-4 (EDT).
        let out = format_local_timestamp(utc("2026-07-15T15:00:00Z"), "America/New_York");
        assert_eq!(out, "2026-07-15 11:00 EDT");
    }

    #[test]
    fn abbreviation_resolves_via_bug_007_map() {
        // "EST" isn't an IANA name; resolve_timezone maps it to
        // America/New_York, and the render then follows that zone's
        // rules (including DST — a summer instant shows EDT).
        let out = format_local_timestamp(utc("2026-07-15T15:00:00Z"), "EST");
        assert_eq!(out, "2026-07-15 11:00 EDT");
    }

    #[test]
    fn unparseable_timezone_falls_back_to_labeled_utc() {
        let out = format_local_timestamp(utc("2026-01-15T15:00:00Z"), "not-a-timezone");
        assert_eq!(out, "2026-01-15 15:00 UTC");
    }
}

#[cfg(test)]
mod session_delete_flow_tests {
    //! Pure-fn guards for the guided /sessions delete flow: the marker
    //! verdict (fail-closed), the two synthetic prompts, and the phase
    //! machine's arm/advance/consume shape.
    use super::*;

    #[test]
    fn verdict_is_fail_closed() {
        assert_eq!(delete_flow_verdict("all saved\n[SESSION_DELETE_READY]"), DeleteVerdict::Ready);
        assert_eq!(delete_flow_verdict("ok\n[SESSION_DELETE_ABORT]"), DeleteVerdict::Abort);
        // No marker — a failed/refused/capped turn must never delete.
        assert_eq!(delete_flow_verdict("I saved three memories."), DeleteVerdict::NoMarker);
        assert_eq!(delete_flow_verdict(""), DeleteVerdict::NoMarker);
        // Both markers: abort wins.
        assert_eq!(
            delete_flow_verdict("[SESSION_DELETE_READY]\nwait no\n[SESSION_DELETE_ABORT]"),
            DeleteVerdict::Abort
        );
    }

    #[test]
    fn kickoff_prompt_names_session_and_forbids_markers() {
        let p = build_delete_kickoff_prompt("old-proj");
        assert!(p.contains("session_summarize"));
        assert!(p.contains("\"old-proj\""));
        assert!(p.contains("do NOT emit any completion marker"));
        // The kickoff must never contain a live marker the verdict scan
        // could match if the model echoes the prompt.
        assert!(!p.contains(SESSION_DELETE_READY_MARKER));
        assert!(!p.contains(SESSION_DELETE_ABORT_MARKER));
    }

    /// The protocol saves with `remember`, which promotes; a
    /// `knowledge_promote` line in it would be the two-step workflow again
    /// (v2.2 carried five, two of them with categories that do not exist).
    /// The banner names the version the file declares.
    #[test]
    fn the_feedback_loop_spec_promotes_nothing_by_hand() {
        assert!(!FEEDBACK_LOOP_SPEC_V2.contains("knowledge_promote"));
        let version = FEEDBACK_LOOP_SPEC_V2
            .lines()
            .find_map(|l| l.strip_prefix("**Spec version:** "))
            .and_then(|l| l.split_whitespace().next())
            .expect("the spec declares its version");
        assert!(
            build_feedback_loop_prompt().contains(&format!("FEEDBACK LOOP SPEC {version} ")),
            "{version}"
        );
    }

    /// Since v2.2 the spec is steps only, and the map from an action to its
    /// tool lived in what the intelligence remembered of v2.1: "Rewrite =
    /// forget then remember". `forget` removes a memory's node with its
    /// links now, so the map is in the spec again, and a rewrite is done in
    /// place. An accepted tension gets no memory of its own.
    #[test]
    fn the_feedback_loop_action_map_rewrites_in_place() {
        let action = |name: &str| {
            FEEDBACK_LOOP_SPEC_V2
                .lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("the action map names {name}"))
        };
        assert!(action("Rewrite").contains("knowledge_update"));
        assert!(action("Supersede").contains("knowledge_link") && action("Supersede").contains("| supersedes |"));
        assert!(action("Reclassify").contains("knowledge_update"));
        assert!(action("Remove").contains("forget"));
        assert!(action("Add practice").contains("remember"));
        let accept = action("Accept");
        assert!(!accept.contains("remember") && !accept.contains("knowledge_"), "{accept}");
        // Duplicates live in nodes: the audit finds them, and step 5.3 only
        // links — nothing is saved a second time.
        assert!(FEEDBACK_LOOP_SPEC_V2.contains("knowledge_audit"));
        assert!(!FEEDBACK_LOOP_SPEC_V2.contains("memory_dedup"));
        let step_5_3 = FEEDBACK_LOOP_SPEC_V2.split("### 5.3").nth(1).expect("step 5.3");
        assert!(step_5_3.contains("knowledge_link") && !step_5_3.contains("remember <"), "{step_5_3}");
    }

    #[test]
    fn memorize_prompt_embeds_reason_and_both_markers() {
        let p = build_delete_memorize_prompt("old-proj", "superseded by the v2 rewrite");
        assert!(p.contains("superseded by the v2 rewrite"));
        assert!(p.contains("session_extract"));
        assert!(p.contains("remember"));
        // The reason is the approval: an approval round would end the turn
        // without the marker, and nothing would be deleted.
        assert!(p.contains("The operator's reply is the approval it asks for"), "{p}");
        assert!(p.contains("without presenting them first"), "{p}");
        assert!(p.contains("remember records each as coming from session 'old-proj'"), "{p}");
        // The brain writes the deletion record after the delete has run; a
        // memory of the deletion from the model would be a second one.
        assert!(!p.contains("Also save one memory"), "{p}");
        assert!(p.contains("records the deletion and the operator's reason as a memory"), "{p}");
        assert!(p.contains("do not save one yourself"), "{p}");
        // `remember` promotes what it saves; the prompt names no second step.
        assert!(!p.contains("knowledge_promote"));
        assert!(p.contains(SESSION_DELETE_READY_MARKER));
        assert!(p.contains(SESSION_DELETE_ABORT_MARKER));
        // Grace-period wording rides the shared const.
        assert!(p.contains(&format!(
            "{}-day",
            crate::sessions::SESSION_DELETE_RETENTION_DAYS
        )));
    }

    #[test]
    fn phase_machine_advances_kickoff_then_consumes_reason() {
        // The function the UserMessage arm calls: AwaitingKickoff advances
        // in place (the kickoff synthetic must run as a normal turn);
        // AwaitingReason is taken (the reason turn executes the delete).
        let mut slot = Some(PendingSessionDelete {
            name: "old-proj".into(),
            phase: DeleteFlowPhase::AwaitingKickoff,
        });
        let first = advance_delete_flow(&mut slot);
        assert!(first.is_none(), "kickoff message must not be consumed as the reason");
        assert_eq!(
            slot.as_ref().map(|s| s.phase),
            Some(DeleteFlowPhase::AwaitingReason)
        );
        let second = advance_delete_flow(&mut slot);
        assert_eq!(second.map(|s| s.name).as_deref(), Some("old-proj"));
        assert!(slot.is_none(), "reason consumption must clear the flow state");
        // No flow pending: a message is a message.
        assert!(advance_delete_flow(&mut slot).is_none());
    }
}

#[cfg(test)]
mod replay_tests {
    use super::{replay_history, SystemMessageType};
    use crate::brain::{AttachmentRef, Message};
    use embra_common::proto::brain::conversation_response::ResponseType;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn the_replay_sends_the_reset_then_each_row_with_its_attachments() {
        let (tx, mut rx) = mpsc::channel(16);
        let history = vec![
            Message {
                role: "user".into(),
                content: "look at this".into(),
                attachments: Some(vec![AttachmentRef {
                    id: "img1".into(),
                    name: "poster.png".into(),
                    media_type: "image/png".into(),
                    width: 10,
                    height: 10,
                    bytes: 100,
                    path: "/embra/workspace/MEDIA/poster.png".into(),
                }]),
            },
            Message {
                role: "assistant".into(),
                content: "a poster".into(),
                attachments: None,
            },
        ];
        replay_history(&tx, "main", &history).await;
        drop(tx);
        let mut frames = Vec::new();
        while let Some(Ok(f)) = rx.recv().await {
            frames.push(f.response_type.expect("typed frame"));
        }
        assert_eq!(frames.len(), 4, "{frames:?}");
        assert!(
            matches!(&frames[0], ResponseType::System(m)
                if m.msg_type == SystemMessageType::TranscriptReset as i32 && m.content == "main"),
            "{frames:?}"
        );
        assert!(
            matches!(&frames[1], ResponseType::System(m)
                if m.msg_type == SystemMessageType::Reconnection as i32 && m.content == "[user] look at this"),
            "{frames:?}"
        );
        assert!(matches!(&frames[2], ResponseType::Media(m) if m.replay && m.id == "img1"), "{frames:?}");
        assert!(
            matches!(&frames[3], ResponseType::System(m) if m.content == "[assistant] a poster"),
            "{frames:?}"
        );
    }
}

#[cfg(test)]
mod operator_stop_tests {
    //! /stop contract guards: the OperatorStop notice/persistence arms
    //! (refusal-precedent honesty) and the StopCheck snapshot semantics
    //! (a stop can only affect the turn in flight when it fired).
    use super::{
        begin_turn, final_assistant_text, learning_turn_outcome, stream_brain_to_grpc,
        terminal_outcome_notice, LearningAnswer, StopCheck,
    };
    use crate::provider::ir::{EarlyStopReason, TurnOutcome};
    use crate::provider::StreamEvent;
    use embra_common::proto::brain::SystemMessageType;
    use futures::stream::BoxStream;
    use tokio::sync::{mpsc, watch};

    const STOPPED: TurnOutcome = TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop);

    #[test]
    fn operator_stop_notice_is_warning_not_provider_error() {
        let (kind, msg) = terminal_outcome_notice(STOPPED, None).expect("notice");
        assert_eq!(kind, SystemMessageType::Warning);
        assert!(msg.contains("stopped by operator"));
        // Must not read as a provider action — the generic arm's wording.
        assert!(!msg.contains("Provider stopped"));
    }

    #[test]
    fn final_text_appends_operator_stop_suffix_to_partial() {
        let out = final_assistant_text("half an answer", &[], STOPPED);
        assert_eq!(out, "half an answer\n\n(response interrupted by operator stop)");
    }

    #[test]
    fn final_text_marks_empty_stopped_turn_over_tool_placeholder() {
        let tools = vec!["web_fetch".to_string()];
        let out = final_assistant_text("", &tools, STOPPED);
        assert_eq!(out, "(turn stopped by operator)");
    }

    #[tokio::test]
    async fn stop_check_trips_only_on_bumps_after_snapshot() {
        let (tx, rx) = watch::channel(0u64);

        // Bump BEFORE the snapshot: invisible to the new turn.
        tx.send_modify(|v| *v += 1);
        let check = StopCheck::new(rx.clone());
        assert!(!check.stopped(), "pre-turn stop must not arm the next turn");

        // Bump DURING the turn: trips both the poll and the select arm.
        tx.send_modify(|v| *v += 1);
        assert!(check.stopped());
        let mut awaited = StopCheck::new(rx.clone());
        assert!(!awaited.stopped());
        tx.send_modify(|v| *v += 1);
        tokio::time::timeout(std::time::Duration::from_secs(1), awaited.triggered())
            .await
            .expect("triggered() must resolve once the generation advances");
    }

    /// `StopTurn` raises the counter only for a turn that is marked as
    /// running. The learning loop took the observer and never set the mark,
    /// and could not be stopped.
    #[test]
    fn a_turn_is_marked_as_running_until_its_guard_is_dropped() {
        let in_turn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = watch::channel(0u64);

        let (guard, check) = begin_turn(&in_turn, &rx, "main");
        assert!(in_turn.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!check.stopped());
        tx.send_modify(|v| *v += 1);
        assert!(check.stopped(), "a stop raised after the turn began reaches it");

        drop(guard);
        assert!(!in_turn.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The activity feed hears a turn begin and end from the same mark.
    #[test]
    fn the_turn_guard_announces_the_turn_start_and_end() {
        let in_turn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (_tx, rx) = watch::channel(0u64);
        let mut events = crate::activity::subscribe();

        let (guard, _check) = begin_turn(&in_turn, &rx, "activity-guard-test");
        drop(guard);

        let mut saw_start = false;
        let mut saw_end_after_start = false;
        while let Ok(ev) = events.try_recv() {
            match ev {
                crate::activity::Event::TurnStarted { session }
                    if session == "activity-guard-test" =>
                {
                    saw_start = true;
                }
                crate::activity::Event::TurnEnded if saw_start => saw_end_after_start = true,
                _ => {}
            }
        }
        assert!(saw_start, "the start carries the session");
        assert!(saw_end_after_start, "the guard's drop ends the turn");
    }

    #[tokio::test]
    async fn a_learning_turn_that_is_stopped_says_so() {
        use embra_common::proto::brain::conversation_response::ResponseType;
        use futures::StreamExt;

        let (stop_tx, stop_rx) = watch::channel(0u64);
        let mut check = StopCheck::new(stop_rx);
        stop_tx.send_modify(|v| *v += 1);

        // A provider that has nothing to say yet, and never will.
        let mut silent: BoxStream<'static, StreamEvent> = futures::stream::pending().boxed();
        let (tx, mut rx) = mpsc::channel(8);
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stream_brain_to_grpc(&mut silent, &tx, "Embra", Some(&mut check)),
        )
        .await
        .expect("a stop ends the wait for the provider");
        assert!(answer.stopped);
        assert_eq!(answer.text, "");

        drop(tx);
        let mut frames = Vec::new();
        while let Some(Ok(frame)) = rx.recv().await {
            frames.push(frame.response_type.expect("typed frame"));
        }
        // The Done first, so the partial lands as a row; the Warning under
        // it, where "any partial response above" is true.
        assert!(matches!(&frames[0], ResponseType::Done(_)), "{frames:?}");
        assert!(
            matches!(&frames[1], ResponseType::System(m)
                if m.msg_type == SystemMessageType::Warning as i32 && m.content.contains("stopped by operator")),
            "{frames:?}"
        );
    }

    fn stop_notice() -> Option<(SystemMessageType, String)> {
        super::terminal_outcome_notice(
            TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop),
            None,
        )
    }

    fn done_text(frame: &embra_common::proto::brain::conversation_response::ResponseType) -> &str {
        match frame {
            embra_common::proto::brain::conversation_response::ResponseType::Done(d) => {
                &d.full_response
            }
            other => panic!("expected a Done, got {other:?}"),
        }
    }

    #[test]
    fn a_stop_during_a_tool_closes_with_an_empty_done_then_the_warning() {
        use embra_common::proto::brain::conversation_response::ResponseType;
        // The text of the response whose tool was running already went out
        // with its own Done: nothing is sent twice.
        let frames = super::closing_frames(true, false, None, stop_notice());
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(done_text(&frames[0]), "");
        assert!(
            matches!(&frames[1], ResponseType::System(m)
                if m.msg_type == SystemMessageType::Warning as i32 && m.content.contains("stopped by operator")),
            "{frames:?}"
        );
    }

    #[test]
    fn a_stop_during_streaming_closes_with_the_partial_then_the_warning() {
        use embra_common::proto::brain::conversation_response::ResponseType;
        let frames = super::closing_frames(
            true,
            false,
            Some("The file holds".to_string()),
            stop_notice(),
        );
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(done_text(&frames[0]), "The file holds");
        assert!(matches!(&frames[1], ResponseType::System(_)), "{frames:?}");
    }

    #[test]
    fn a_dangling_token_gets_its_done_and_no_notice() {
        // The silent-end diagnostic and the cap fallbacks stream as tokens
        // that no Complete closes; the closing Done turns the live line
        // into a row.
        let frames = super::closing_frames(
            false,
            false,
            Some("(model ended turn silently after 1 side-effectful tool call)".to_string()),
            None,
        );
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert!(done_text(&frames[0]).starts_with("(model ended turn silently"));
    }

    #[test]
    fn a_normal_end_sends_no_closing_frame() {
        assert!(super::closing_frames(false, false, None, None).is_empty());
    }

    #[test]
    fn a_failed_stream_closes_with_an_empty_done() {
        // The error frame went out from inside the stream; the Done ends
        // the clients' live state and adds no row.
        let frames = super::closing_frames(false, true, None, None);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(done_text(&frames[0]), "");
    }

    #[test]
    fn a_stopped_learning_turn_ends_no_phase_and_is_marked() {
        // Stopped after the marker had arrived: the phase is not over.
        let (entry, done) = learning_turn_outcome(&LearningAnswer {
            text: "Here is what I understood so far. [PHASE_COMPLETE]".into(),
            stopped: true,
        });
        assert!(!done);
        assert_eq!(
            entry,
            "Here is what I understood so far.\n\n(response interrupted by operator stop)"
        );

        // Stopped before anything arrived.
        let (entry, done) = learning_turn_outcome(&LearningAnswer { text: String::new(), stopped: true });
        assert!(!done);
        assert_eq!(entry, "(turn stopped by operator)");
    }

    #[test]
    fn a_learning_turn_that_ran_to_its_end_is_written_as_before() {
        let (entry, done) = learning_turn_outcome(&LearningAnswer {
            text: "Tell me more about that.".into(),
            stopped: false,
        });
        assert_eq!((entry.as_str(), done), ("Tell me more about that.", false));

        let (entry, done) = learning_turn_outcome(&LearningAnswer {
            text: "That completes it.\n[PHASE_COMPLETE]".into(),
            stopped: false,
        });
        assert_eq!((entry.as_str(), done), ("That completes it.", true));

        // The marker alone: never an empty assistant message.
        let (entry, done) = learning_turn_outcome(&LearningAnswer {
            text: "[PHASE_COMPLETE]".into(),
            stopped: false,
        });
        assert_eq!((entry.as_str(), done), ("(phase complete)", true));
    }
}

#[cfg(test)]
mod trigger_turn_tests {
    use super::*;
    use crate::proactive::{ActTrigger, TriggerKind};
    use std::time::{Duration, Instant};

    #[test]
    fn the_act_budget_allows_twelve_an_hour_and_the_oldest_stamp_expires() {
        assert_eq!(ACT_TURNS_PER_HOUR_MAX, 12);
        let mut budget = ActBudget::new();
        let start = Instant::now();
        for i in 0..ACT_TURNS_PER_HOUR_MAX {
            assert!(budget.allow(start + Duration::from_secs(i as u64 * 60)), "turn {i}");
        }
        // The thirteenth within the hour is refused, and refusing counts nothing.
        assert!(!budget.allow(start + Duration::from_secs(30 * 60)));
        assert!(!budget.allow(start + Duration::from_secs(59 * 60)));
        // An hour after the first stamp, one slot is free again.
        assert!(budget.allow(start + Duration::from_secs(60 * 60)));
        assert!(!budget.allow(start + Duration::from_secs(60 * 60)));
        // A second one frees a minute later.
        assert!(budget.allow(start + Duration::from_secs(61 * 60)));
    }

    #[test]
    fn trigger_turn_text_names_the_kind_and_caps_the_body() {
        let reminder = ActTrigger {
            kind: TriggerKind::Reminder,
            headline: "check the build".into(),
            body: "check the build".into(),
        };
        assert_eq!(trigger_turn_text(&reminder), "[Reminder fired] check the build");
        let long = "日".repeat(1000);
        let cron = ActTrigger {
            kind: TriggerKind::Cron,
            headline: "system_status".into(),
            body: long.clone(),
        };
        let text = trigger_turn_text(&cron);
        assert!(text.starts_with("[embraCRON acted] system_status: 日"), "{text}");
        let body = text.strip_prefix("[embraCRON acted] system_status: ").unwrap();
        assert!(body.len() <= TRIGGER_PERSIST_MAX && body.len() > TRIGGER_PERSIST_MAX - 3);
        assert!(long.starts_with(body));
    }
}

#[cfg(test)]
mod session_listing_tests {
    //! `/sessions` names the provider each session is pinned to, and says
    //! how to open a session pinned to another one.
    use super::{other_provider_footer, session_list_line};
    use crate::sessions::SessionMeta;
    use serde_json::json;

    fn meta(name: &str, provider: Option<&str>, turns: u32) -> SessionMeta {
        let mut doc = json!({
            "id": format!("id-{name}"), "name": name, "state": "Detached",
            "created_at": "2026-10-01T12:00:00Z", "last_active": "2026-10-08T18:40:00Z",
            "turn_count": turns,
        });
        if let Some(provider) = provider {
            doc["provider"] = json!(provider);
        }
        serde_json::from_value(doc).expect("a meta document")
    }

    #[test]
    fn a_session_line_names_its_provider() {
        assert_eq!(
            session_list_line(&meta("qwen-eval", Some("lm_studio"), 14), true, "UTC"),
            "  qwen-eval [Detached] LM Studio · 14 turns, last active: 2026-10-08 18:40 UTC"
        );
        assert_eq!(
            session_list_line(&meta("main", Some("anthropic"), 1), true, "UTC"),
            "  main [Detached] Anthropic · 1 turn, last active: 2026-10-08 18:40 UTC"
        );
    }

    /// Every session before Sprint 4 ran on Anthropic, and
    /// `read_session_provider` reads a missing provider so; the listing
    /// agrees with the check `/switch` makes. `learning` is never switched
    /// to and shows no provider.
    #[test]
    fn a_session_without_a_recorded_provider_reads_as_anthropic() {
        assert!(session_list_line(&meta("old", None, 3), true, "UTC").contains("[Detached] Anthropic · 3 turns"));
        assert_eq!(
            session_list_line(&meta("learning", None, 212), true, "UTC"),
            "  learning [sealed] [Detached] 212 turns, last active: 2026-10-08 18:40 UTC"
        );
    }

    #[test]
    fn the_footer_names_one_other_provider_in_the_steps() {
        let live = [meta("main", Some("anthropic"), 9), meta("qwen-eval", Some("lm_studio"), 14)];
        assert_eq!(
            other_provider_footer(&live, "anthropic", true).as_deref(),
            Some("  Under lm_studio: /close, then /provider lm_studio, then /switch <name>.")
        );
        assert_eq!(
            other_provider_footer(&live, "anthropic", false).as_deref(),
            Some("  Under lm_studio: /provider lm_studio, then /switch <name>.")
        );
    }

    #[test]
    fn the_footer_lists_several_other_providers() {
        let live = [
            meta("main", Some("anthropic"), 9),
            meta("qwen-eval", Some("lm_studio"), 14),
            meta("glm", Some("ollama"), 2),
            meta("gemma", Some("ollama"), 5),
        ];
        assert_eq!(
            other_provider_footer(&live, "gemini", true).as_deref(),
            Some("  Under another provider (anthropic, lm_studio, ollama): /close, then /provider <kind>, then /switch <name>.")
        );
    }

    #[test]
    fn no_footer_when_every_session_is_under_the_active_provider() {
        let live = [meta("main", Some("anthropic"), 9), meta("old", None, 3), meta("learning", None, 212)];
        assert_eq!(other_provider_footer(&live, "anthropic", true), None);
        assert_eq!(other_provider_footer(&[], "anthropic", true), None);
    }
}
