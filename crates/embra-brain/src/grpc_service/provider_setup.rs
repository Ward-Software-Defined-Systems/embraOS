//! Providers: the switch, the setup flows of `/provider --setup`, the model and effort resolvers, `build_provider_from_config`.

use super::*;

/// Sprint 5 follow-up: post-wizard reconfigure for OpenAI-compat
/// presets. Set by `/provider --setup <ollama|lm_studio>`; advanced
/// one transition per intercepted UserMessage; cleared on completion
/// or any non-reentry slash command.
#[derive(Debug, Clone)]
pub struct OpenAiCompatSetupState {
    pub preset: crate::provider::openai_compat::OpenAiCompatPreset,
    pub current_endpoint: Option<String>,
    pub current_bearer: Option<String>,
    pub current_model: Option<String>,
    pub step: OpenAiCompatSetupStep,
    /// Set by `/model` on the active Ollama or LM Studio preset: the flow
    /// starts at the model step with the endpoint and bearer as they are,
    /// and the pick only switches the model (`local_model_selection`).
    pub model_only: bool,
    /// The provider gateway while it is on, captured at entry: the models
    /// are listed through its prefix, and the endpoint typed is the direct
    /// path for when the gateway is off, `none` allowed.
    pub gateway: Option<config::ProviderGateway>,
}

#[derive(Debug, Clone)]
#[expect(
    clippy::enum_variant_names,
    reason = "a step is named after the answer the flow is waiting for"
)]
pub enum OpenAiCompatSetupStep {
    AwaitingEndpoint,
    AwaitingBearerChoice {
        endpoint: String,
    },
    AwaitingBearerToken {
        endpoint: String,
    },
    AwaitingModelSelection {
        endpoint: String,
        bearer: Option<String>,
        models: Vec<String>,
    },
}

/// Outcome of one state-machine step. `Advance` reinstates the state
/// in the Mutex; `Complete` and `Error` leave the Mutex empty so the
/// next user message goes to the regular conversation path.
pub(super) enum SetupStepOutcome {
    Advance,
    Complete,
    Error,
}

/// Where `/provider --setup <anthropic|gemini>` stands: the key, then the
/// model, the order the local presets' setup uses (endpoint, bearer,
/// model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeySetupStep {
    /// The next message is this provider's API key.
    Key(ProviderKind),
    /// The key is saved; the next message picks this provider's model.
    Model(ProviderKind),
}

/// `/provider` command surface: status, switch, and (as deferred via
/// spec D2) a deny-then-instruct path when no key is recorded for the
/// alternate provider. Idle switches apply immediately; in-turn
/// switches queue via `pending_provider` and drain after the loop.
pub(super) async fn handle_provider_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    ctx: &ServiceCtx,
) {
    let ServiceCtx {
        db,
        session_mgr,
        in_turn,
        pending_provider,
        pending_key_setup,
        pending_openai_compat_setup,
        ..
    } = ctx;
    let send_msg = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };

    let action = args.trim();

    // /provider --setup <kind> — multi-turn flow.
    // - <anthropic|gemini>: single-step API-key add via pending_key_setup;
    //   handle_pending_key_setup intercepts the next user message.
    // - <ollama|lm_studio>: 4-step reconfigure (Endpoint → Bearer →
    //   Model) via pending_openai_compat_setup; intercepted N user
    //   messages by handle_pending_openai_compat_step.
    // No-arg form auto-targets between Anthropic/Gemini only (see
    // auto-target match below). OpenAI-compat presets must be
    // specified explicitly.
    if let Some(rest) = action.strip_prefix("--setup") {
        let cfg = match config::load_config(db).await {
            Ok(c) => c,
            Err(_) => {
                send_msg(
                    "No system config found — run setup wizard first.".to_string(),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }
        };
        let explicit = rest.trim();
        let target = if explicit.is_empty() {
            // Auto-target the provider whose per-provider key is
            // missing. If both are set, require explicit form.
            let anth_present = cfg.key_for(ProviderKind::Anthropic).is_some();
            let gem_present = cfg.key_for(ProviderKind::Gemini).is_some();
            match (anth_present, gem_present) {
                (true, false) => ProviderKind::Gemini,
                (false, true) => ProviderKind::Anthropic,
                (true, true) => {
                    send_msg(
                        "Both Anthropic and Gemini already have API keys. \
                         Use /provider --setup <anthropic|gemini> to replace one, \
                         or /provider --setup <ollama|lm_studio> to (re)configure an OpenAI-compat preset."
                            .to_string(),
                        SystemMessageType::Error,
                    ).await;
                    return;
                }
                (false, false) => {
                    // Pre-wizard / cleared state.
                    send_msg(
                        "No keys recorded yet. Re-run setup wizard first.".to_string(),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            }
        } else {
            match ProviderKind::from_str(explicit) {
                Some(k) => k,
                None => {
                    send_msg(
                        format!(
                            "Unknown provider '{}'. Use 'anthropic', 'gemini', 'ollama', or 'lm_studio'.",
                            explicit
                        ),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            }
        };
        // OpenAI-compat presets dispatch to the multi-step
        // reconfigure flow (Sprint 5 follow-up). Anthropic/Gemini
        // continue with the single-step pending_key_setup pattern.
        match target {
            ProviderKind::Ollama | ProviderKind::LmStudio => {
                // Clear any other pending state — only one --setup
                // flow at a time.
                *pending_key_setup.lock().await = None;
                enter_openai_compat_setup(target, tx, db, pending_openai_compat_setup).await;
                return;
            }
            ProviderKind::Anthropic | ProviderKind::Gemini => {
                // Clear any in-flight OpenAI-compat reconfigure too.
                *pending_openai_compat_setup.lock().await = None;
                *pending_key_setup.lock().await = Some(KeySetupStep::Key(target));
            }
        }
        // Prompt for the key. Console renders as a system message;
        // the next user input will be intercepted.
        let prompt_text = match target {
            ProviderKind::Anthropic => "Enter your Anthropic API key (next message):",
            ProviderKind::Gemini => "Enter your Gemini API key (next message):",
            // Unreachable — handled by the dispatch match above.
            ProviderKind::Ollama | ProviderKind::LmStudio => unreachable!(),
        };
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(
                    SetupPrompt {
                        field_type: SetupFieldType::Secret as i32,
                        prompt: prompt_text.to_string(),
                        options: vec![],
                        default_value: String::new(),
                    },
                )),
            }))
            .await;
        return;
    }

    match action {
        "" | "status" => {
            let cfg = config::load_config(db).await.ok();
            let provider = cfg
                .as_ref()
                .map(|c| c.api_provider.clone())
                .unwrap_or_else(|| "anthropic".to_string());
            let model = match cfg.as_ref() {
                Some(c) => display_model_for(&provider, c),
                None => crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string(),
            };
            let session = session_mgr
                .read()
                .await
                .active_session
                .clone()
                .unwrap_or_else(|| "<none>".to_string());
            let gateway = cfg
                .as_ref()
                .map(crate::provider::gateway::provider_line_suffix)
                .unwrap_or_default();
            send_msg(
                format!(
                    "Active provider: {}. Model: {}. Session: {}.{}",
                    provider, model, session, gateway
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        "anthropic" | "gemini" | "ollama" | "lm_studio" => {
            let target = ProviderKind::from_str(action).unwrap();
            // Pre-check: is the target configured? For Anthropic/Gemini
            // that's a recorded API key; for OpenAI-compat presets it's
            // an endpoint + model in `config.system.openai_compat`.
            // Surface a missing-config error early so the operator gets
            // a useful hint instead of a per-call failure on the next
            // turn.
            let cfg = match config::load_config(db).await {
                Ok(c) => c,
                Err(_) => {
                    send_msg(
                        "No system config found — re-run setup wizard first.".to_string(),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            };
            if cfg.api_provider == target.as_str() {
                send_msg(
                    format!("Already using {}. No change.", target.as_str()),
                    SystemMessageType::Info,
                )
                .await;
                return;
            }
            // Per-provider configuration check.
            // - Anthropic/Gemini: per-provider API-key field via
            //   `cfg.key_for` (Sprint 4 D2), falling back to the legacy
            //   `api_key` when the active provider matches.
            // - Ollama/LmStudio: a model, and a base to reach it on
            //   (`for_preset_routed`: the stored endpoint, or the provider
            //   gateway's prefix while the gateway is on; a preset set up
            //   through the gateway may have no direct endpoint). Bearer is
            //   intentionally NOT required — no-auth Ollama is a valid
            //   configuration.
            let refusal: Option<String> = match target {
                ProviderKind::Anthropic | ProviderKind::Gemini => cfg
                    .key_for(target)
                    .is_none()
                    .then(|| {
                        format!(
                            "No API key recorded for {}. Run /provider --setup {} to add one.",
                            target.as_str(),
                            target.as_str()
                        )
                    }),
                ProviderKind::Ollama | ProviderKind::LmStudio => {
                    let preset = match target {
                        ProviderKind::Ollama => {
                            crate::provider::openai_compat::OpenAiCompatPreset::Ollama
                        }
                        ProviderKind::LmStudio => {
                            crate::provider::openai_compat::OpenAiCompatPreset::LmStudio
                        }
                        _ => unreachable!(),
                    };
                    cfg.openai_compat
                        .for_preset_routed(preset, cfg.provider_gateway.as_ref())
                        .err()
                        .map(|e| e.to_string())
                }
            };
            if let Some(body) = refusal {
                send_msg(body, SystemMessageType::Error).await;
                return;
            }

            // Cross-provider invariant: a session is bound to the
            // provider it was created under. Switching providers
            // mid-session would mutate the active session's meta and
            // corrupt the very state the SessionAttach load-guard
            // exists to protect.
            if let Some(ref active) = session_mgr.read().await.active_session {
                send_msg(
                    format!(
                        "Cannot switch providers while a session is active (current: '{}'). \
                         Run /close to close it first, then /provider {}.",
                        active,
                        target.as_str()
                    ),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }

            if in_turn.load(Ordering::SeqCst) {
                let mut guard = pending_provider.lock().await;
                let prev = guard.replace(target);
                let body = match prev {
                    Some(p) if p != target => format!(
                        "Switch queued — replacing previously queued switch to {}.",
                        p.as_str()
                    ),
                    _ => "Switch queued. Will apply after current turn completes.".to_string(),
                };
                send_msg(body, SystemMessageType::Info).await;
                return;
            }
            perform_provider_swap(target, db, session_mgr, tx).await;
        }
        _ => {
            send_msg(
                format!("Unknown provider '{}'. Use 'anthropic', 'gemini', 'ollama', or 'lm_studio'.", action),
                SystemMessageType::Error,
            )
            .await;
        }
    }
}

/// Apply a provider switch. Runs after `in_turn` has cleared (either
/// because the loop just ended or because the command arrived idle).
/// Updates config.system.api_provider, the active session's
/// meta.provider/meta.model, and /embra/state/api_provider, then
/// emits a `ModeTransition` carrying the new `Brain: <model>` token
/// so the console's status-bar `provider_model` refreshes immediately
/// (without waiting for the next session-attach).
pub(super) async fn perform_provider_swap(
    target: ProviderKind,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
) {
    // 1. Update config doc — set api_provider AND mirror the
    //    target's per-provider key into the legacy api_key field so
    //    existing read paths see the right value. Also mirror to
    //    /embra/state/api_key so the supervisor's existing read path
    //    keeps working until a future change teaches embrad about
    //    per-provider STATE.
    if let Ok(mut cfg) = config::load_config(db).await {
        // Sprint 5: OpenAI-compat presets are pre-checked for
        // endpoint + model configuration rather than api_key presence.
        // Bearer comes from runtime env; absence is OK (no-auth path).
        let target_key: String = match target {
            ProviderKind::Anthropic | ProviderKind::Gemini => match cfg.key_for(target) {
                Some(k) => k.to_string(),
                None => {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Cannot switch — no API key recorded for {}.",
                                        target.as_str()
                                    ),
                                    msg_type: SystemMessageType::Error as i32,
                                },
                            )),
                        }))
                        .await;
                    return;
                }
            },
            ProviderKind::Ollama | ProviderKind::LmStudio => {
                let preset = match target {
                    ProviderKind::Ollama => {
                        crate::provider::openai_compat::OpenAiCompatPreset::Ollama
                    }
                    ProviderKind::LmStudio => {
                        crate::provider::openai_compat::OpenAiCompatPreset::LmStudio
                    }
                    _ => unreachable!(),
                };
                if let Err(reason) = cfg
                    .openai_compat
                    .for_preset_routed(preset, cfg.provider_gateway.as_ref())
                {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!("Cannot switch — {reason}"),
                                    msg_type: SystemMessageType::Error as i32,
                                },
                            )),
                        }))
                        .await;
                    return;
                }
                String::new()
            }
        };
        cfg.api_provider = target.as_str().to_string();
        cfg.api_key = target_key.clone();
        if let Err(e) = config::save_config(db, &cfg).await {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Failed to persist provider switch: {}", e),
                            msg_type: SystemMessageType::Error as i32,
                        },
                    )),
                }))
                .await;
            return;
        }
        // Skip mirroring an empty target_key — that would clobber a
        // previously-recorded Anthropic/Gemini key in STATE when an
        // operator swaps to an OpenAI-compat preset.
        if !target_key.is_empty() {
            let _ = config::write_credential_state("/embra/state/api_key", &target_key);
        }
    }

    // 2. Update active session's meta.provider / meta.model so
    //    cross-session resume sees the right provider. With the
    //    cross-provider guard in `handle_provider_command` rejecting
    //    swaps while a session is active, this block is defensive —
    //    `active_session` should be `None` at this point. Kept in
    //    place so the meta stays correct if the guard is ever bypassed.
    //    Uses query (not `db.read(_, name)`) because session-meta docs
    //    have auto-generated `_id`s, not the session name.
    let active_session = session_mgr.read().await.active_session.clone();
    if let Some(ref name) = active_session {
        let collection = format!("sessions.{}.meta", name);
        if let Ok(results) = db.query(&collection, &crate::sessions::history_query_body()).await
            && let Some(mut doc) = results.into_iter().next()
        {
            let id = doc
                .get("_id")
                .or_else(|| doc.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(id) = id {
                let model_str = match config::load_config(db).await {
                    Ok(c) => display_model_for(target.as_str(), &c),
                    Err(_) => target.as_str().to_string(),
                };
                if let Some(obj) = doc.as_object_mut() {
                    obj.insert(
                        "provider".into(),
                        serde_json::Value::String(target.as_str().to_string()),
                    );
                    obj.insert(
                        "model".into(),
                        serde_json::Value::String(model_str),
                    );
                }
                let _ = db.update(&collection, &id, &doc).await;
            }
        }
    }

    // 3. Persist to STATE so embrad picks the right provider on the
    //    next boot.
    let _ = std::fs::write("/embra/state/api_provider", target.as_str());
    // The active endpoint changed — re-probe now instead of at the next
    // 5-minute tick.
    crate::provider::health::request_probe();

    let cfg_after = config::load_config(db).await.ok();
    let model_for_swap_msg = match cfg_after.as_ref() {
        Some(c) => display_model_for(target.as_str(), c),
        None => target.as_str().to_string(),
    };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(
                SystemMessage {
                    content: format!(
                        "Provider switched to {}. Next turn will use {}.",
                        target.as_str(),
                        model_for_swap_msg
                    ),
                    msg_type: SystemMessageType::Info as i32,
                },
            )),
        }))
        .await;

    // 4. Emit a ModeTransition with the new `Brain: <model>` token
    //    so the console's status bar updates immediately. Without
    //    this, AppState.provider_model only refreshes on the next
    //    session-attach / `/switch` / `/new`, leaving the bar
    //    showing the old model until then.
    //
    //    Mirrors the session-attach handler's shape: Operational
    //    when soul is sealed, Learning otherwise. The console parser
    //    extracts Name / Session / TZ / Brain tokens regardless of
    //    other prose; we use the same delimited format the rest of
    //    the codebase emits.
    let name = cfg_after
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Embra".to_string());
    let tz = cfg_after
        .as_ref()
        .map(|c| c.timezone.clone())
        .unwrap_or_else(|| "Etc/UTC".to_string());
    let model = model_for_swap_msg.clone();
    let is_sealed = learning::is_soul_sealed(db).await.unwrap_or(false);
    let mode = if is_sealed {
        OperatingMode::Operational
    } else {
        OperatingMode::Learning
    };
    let message = if is_sealed {
        let session = active_session
            .clone()
            .unwrap_or_else(|| "<none>".to_string());
        operational_status_line(&name, &session, &tz, &model)
    } else {
        format!(
            "Learning Mode — Name: {} — TZ: {} — Brain: {}",
            name, tz, model
        )
    };
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::ModeChange(
                ModeTransition {
                    from_mode: mode as i32,
                    to_mode: mode as i32,
                    message,
                },
            )),
        }))
        .await;
}

/// Construct an `OpenAICompatProvider` for the given `ProviderKind`
/// (Ollama or LmStudio) from the loaded config + runtime env vars.
/// Returns an error message suitable for surfacing to the operator
/// when the preset is unconfigured (empty endpoint or model).
///
/// Bearer is read per-call from `EMBRA_OLLAMA_BEARER` /
/// `EMBRA_LM_STUDIO_BEARER` rather than boot-time captured values,
/// matching the post-`9b4b64d` resolve-fresh pattern Anthropic and
/// Gemini use to avoid stale-after-swap bugs.
pub(super) fn build_openai_compat_provider(
    kind: ProviderKind,
    config: &config::SystemConfig,
) -> Result<Arc<dyn LlmProvider>, String> {
    use crate::provider::openai_compat::{OpenAICompatProvider, OpenAiCompatPreset};
    let preset = match kind {
        ProviderKind::Ollama => OpenAiCompatPreset::Ollama,
        ProviderKind::LmStudio => OpenAiCompatPreset::LmStudio,
        _ => unreachable!("build_openai_compat_provider called with non-OpenAI-compat kind"),
    };
    // The base is the stored endpoint, or the provider gateway's `{url}/ollama`
    // or `{url}/lmstudio` while the gateway is on, when the gateway's own
    // upstream answers; a preset set up through the gateway with no direct
    // endpoint is refused while the gateway is off (`for_preset_routed`).
    let (base, model) = config
        .openai_compat
        .for_preset_routed(preset, config.provider_gateway.as_ref())
        .map_err(|reason| format!("Cannot start a turn — {reason}"))?;
    let bearer = std::env::var(bearer_env_var(preset))
        .ok()
        .filter(|s| !s.is_empty());
    let provider = match preset {
        OpenAiCompatPreset::Ollama => OpenAICompatProvider::ollama(base, bearer, model.to_string()),
        OpenAiCompatPreset::LmStudio => {
            OpenAICompatProvider::lm_studio(base, bearer, model.to_string())
        }
    };
    // `/effort` override for this preset, sent verbatim when set; `None`
    // keeps the per-model auto-map (byte-identical default).
    Ok(Arc::new(
        provider.with_reasoning_effort(resolve_provider_effort(kind, config)),
    ))
}

/// What the endpoint-health probe should check, resolved from persisted
/// config with the SAME resolvers a turn uses (env model overrides, the
/// per-provider key with the boot-key fallback, the preset's bearer, the
/// base the provider gateway rule gives), so the probe never disagrees
/// with the next request. `None` = nothing to probe yet (no key / no
/// preset endpoint — a pre-wizard boot).
pub(crate) fn provider_probe_target(
    cfg: &config::SystemConfig,
    boot_key: &str,
) -> Option<crate::provider::health::ProbeTarget> {
    use crate::provider::health::ProbeTarget;
    use crate::provider::openai_compat::OpenAiCompatPreset;
    let kind = ProviderKind::from_str(&cfg.api_provider).unwrap_or(ProviderKind::Anthropic);
    match kind {
        ProviderKind::Anthropic | ProviderKind::Gemini => {
            let key = cfg
                .key_for(kind)
                .map(str::to_string)
                .unwrap_or_else(|| boot_key.to_string());
            if key.is_empty() {
                return None;
            }
            let model = match kind {
                ProviderKind::Anthropic => resolve_anthropic_model(cfg).0,
                _ => resolve_gemini_model_id(cfg),
            };
            Some(ProbeTarget {
                kind,
                model,
                key,
                endpoint: crate::provider::gateway::base_for(
                    cfg.provider_gateway.as_ref(),
                    kind,
                    match kind {
                        ProviderKind::Anthropic => crate::provider::anthropic::DIRECT_BASE,
                        _ => crate::provider::gemini::DIRECT_BASE,
                    },
                ),
            })
        }
        ProviderKind::Ollama | ProviderKind::LmStudio => {
            let preset = match kind {
                ProviderKind::Ollama => OpenAiCompatPreset::Ollama,
                _ => OpenAiCompatPreset::LmStudio,
            };
            // The routed base: the stored endpoint, or the gateway's prefix
            // while it is on; a preset without a direct endpoint reads as
            // unconfigured while the gateway is off.
            let (endpoint, model) = cfg
                .openai_compat
                .for_preset_routed(preset, cfg.provider_gateway.as_ref())
                .ok()?;
            let bearer = std::env::var(bearer_env_var(preset))
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            Some(ProbeTarget {
                kind,
                model: model.to_string(),
                key: bearer,
                endpoint,
            })
        }
    }
}

/// Build the active LLM provider from persisted config. Shared by the
/// per-turn loop and the guardian replicant check
/// (`crate::guardian::replicant`). `fallback_key` is used for
/// Anthropic/Gemini when no per-provider key is recorded in config (the
/// per-turn loop passes its boot key; the guardian path reads STATE).
/// `gemini_cache_db = Some(db)` wires Gemini's context cache (the per-turn
/// loop); `None` builds a cacheless provider for one-shot calls like the
/// replicant check. Returns `Err(msg)` for an unconfigured OpenAI-compat
/// preset, so the caller can surface it (loop → SystemMessage; replicant
/// check → fail closed).
pub(crate) fn build_provider_from_config(
    cfg: &config::SystemConfig,
    fallback_key: &str,
    gemini_cache_db: Option<Arc<WardsonDbClient>>,
) -> Result<Arc<dyn LlmProvider>, String> {
    let kind = ProviderKind::from_str(&cfg.api_provider).unwrap_or(ProviderKind::Anthropic);
    let active_key = cfg
        .key_for(kind)
        .map(str::to_string)
        .unwrap_or_else(|| fallback_key.to_string());
    match kind {
        ProviderKind::Gemini => {
            let model_id = resolve_gemini_model_id(cfg);
            // The base first: `with_cache` builds the cache manager on it.
            let p = GeminiProvider::with_model(active_key, model_id)
                .with_base_url(crate::provider::gateway::base_for(
                    cfg.provider_gateway.as_ref(),
                    ProviderKind::Gemini,
                    crate::provider::gemini::DIRECT_BASE,
                ))
                .with_effort(resolve_provider_effort(ProviderKind::Gemini, cfg));
            let p = match gemini_cache_db {
                Some(db) => p.with_cache(db),
                None => p,
            };
            Ok(Arc::new(p))
        }
        ProviderKind::Anthropic => {
            let (model_id, display) = resolve_anthropic_model(cfg);
            Ok(Arc::new(
                AnthropicProvider::with_model(active_key, model_id, display)
                    .with_base_url(crate::provider::gateway::base_for(
                        cfg.provider_gateway.as_ref(),
                        ProviderKind::Anthropic,
                        crate::provider::anthropic::DIRECT_BASE,
                    ))
                    .with_effort(resolve_anthropic_effort(cfg)),
            ))
        }
        ProviderKind::Ollama | ProviderKind::LmStudio => build_openai_compat_provider(kind, cfg),
    }
}

/// Display name shown in `Brain: <model>` status-bar tokens. For
/// fixed-model providers (Anthropic, Gemini) the model id is hardcoded.
/// For OpenAI-compat presets the model id is operator-selected via the
/// wizard and lives in `config.openai_compat`; falls back to the preset
/// label when unconfigured (e.g., on first boot before the wizard
/// finishes). Sprint 5: signature changed from `-> &'static str` to
/// `-> String` because OpenAI-compat models can't be hardcoded literals.
pub(super) fn display_model_for(provider: &str, config: &config::SystemConfig) -> String {
    match provider {
        "gemini" => crate::provider::gemini::display_name_for(&resolve_gemini_model_id(config)),
        "anthropic" => resolve_anthropic_model(config).1,
        "ollama" => {
            if config.openai_compat.ollama_model.is_empty() {
                "ollama".to_string()
            } else {
                config.openai_compat.ollama_model.clone()
            }
        }
        "lm_studio" => {
            if config.openai_compat.lm_studio_model.is_empty() {
                "lm_studio".to_string()
            } else {
                config.openai_compat.lm_studio_model.clone()
            }
        }
        _ => crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string(),
    }
}

/// Bearer choice selector option labels. 3-way when a current bearer
/// exists; 2-way otherwise.
pub(super) const BEARER_KEEP: &str = "Keep";

pub(super) const BEARER_CHANGE: &str = "Change";

pub(super) const BEARER_REMOVE: &str = "Remove";

pub(super) const BEARER_SET: &str = "Set";

pub(super) const BEARER_SKIP: &str = "Skip";

/// Initialize the OpenAI-compat reconfigure state machine. Loads
/// current config + env-var bearer, captures snapshot defaults,
/// emits the initial Endpoint Text prompt.
pub(super) async fn enter_openai_compat_setup(
    target: ProviderKind,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) {
    use crate::provider::openai_compat::OpenAiCompatPreset;
    let preset = match target {
        ProviderKind::Ollama => OpenAiCompatPreset::Ollama,
        ProviderKind::LmStudio => OpenAiCompatPreset::LmStudio,
        _ => unreachable!("enter_openai_compat_setup called with non-OpenAI-compat kind"),
    };

    let cfg = config::load_config(db).await.ok();
    let (current_endpoint, current_model) = cfg
        .as_ref()
        .map(|c| {
            let (endpoint, model) = c.openai_compat.fields(preset);
            (non_empty(endpoint), non_empty(model))
        })
        .unwrap_or((None, None));
    let current_bearer = std::env::var(bearer_env_var(preset))
        .ok()
        .filter(|s| !s.is_empty());

    // The gateway while it is on: the models are listed through it, and
    // the endpoint typed is the direct path for when it is off.
    let gateway = cfg
        .as_ref()
        .and_then(|c| c.provider_gateway.clone())
        .filter(|g| g.enabled);
    let state = OpenAiCompatSetupState {
        preset,
        current_endpoint,
        current_bearer,
        current_model,
        step: OpenAiCompatSetupStep::AwaitingEndpoint,
        model_only: false,
        gateway,
    };
    let prompt = endpoint_prompt(&state, false);
    *pending.lock().await = Some(state);

    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(prompt)),
        }))
        .await;
}

/// The endpoint step's prompt. With the provider gateway on, the endpoint
/// typed is the direct path for when the gateway is off, and `none` is
/// allowed, since the gateway's own upstream answers while it is on; the
/// default is the current endpoint, else `none`. Without a gateway on, the
/// typed endpoint is where the preset is reached, and the default is the
/// current endpoint, else the preset's.
pub(super) fn endpoint_prompt(state: &OpenAiCompatSetupState, re_enter: bool) -> SetupPrompt {
    let label = state.preset.label();
    let current = state.current_endpoint.as_deref();
    let (prompt, default_value) = if state.gateway.is_some() {
        (
            if re_enter {
                format!("Re-enter the {label} endpoint URL (none is allowed):")
            } else {
                format!(
                    "Enter the {label} endpoint URL for when the gateway is off (current: {}; none is allowed):",
                    current.unwrap_or("none")
                )
            },
            current.unwrap_or("none").to_string(),
        )
    } else {
        (
            if re_enter {
                format!("Re-enter the {label} endpoint URL:")
            } else {
                format!(
                    "Enter the {label} endpoint URL (current: {}):",
                    current.unwrap_or("<unconfigured>")
                )
            },
            current.unwrap_or(state.preset.default_base_url()).to_string(),
        )
    };
    SetupPrompt {
        field_type: SetupFieldType::Text as i32,
        prompt,
        options: vec![],
        default_value,
    }
}

pub(super) fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Advance the OpenAI-compat reconfigure state machine by one step
/// based on the captured user input. Caller is responsible for
/// reinstating the state in the Mutex on `Advance`; `Complete` and
/// `Error` indicate the state was already cleared.
pub(super) async fn handle_pending_openai_compat_step(
    state: &mut OpenAiCompatSetupState,
    input: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) -> SetupStepOutcome {
    let trimmed = input.trim();

    let next_step = std::mem::replace(&mut state.step, OpenAiCompatSetupStep::AwaitingEndpoint);

    match next_step {
        OpenAiCompatSetupStep::AwaitingEndpoint => {
            // `none`, or nothing with nothing current, is a blank direct
            // endpoint while the gateway is on; without the gateway on, a
            // direct endpoint is the one way to reach the preset.
            let none = trimmed.eq_ignore_ascii_case("none");
            let endpoint_input = if state.gateway.is_some()
                && (none || (trimmed.is_empty() && state.current_endpoint.is_none()))
            {
                String::new()
            } else if none {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: "none needs the gateway: a direct endpoint is required \
                                          while /local-ai-gateway is off."
                                    .to_string(),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                return reset_to_endpoint_step(state, tx).await;
            } else if trimmed.is_empty() {
                state
                    .current_endpoint
                    .clone()
                    .unwrap_or_else(|| state.preset.default_base_url().to_string())
            } else {
                crate::setup::wizard::normalize_endpoint(trimmed, state.preset)
            };
            // Build bearer-choice prompt — 3-way if current bearer exists,
            // 2-way otherwise.
            let (options, default_value) = if state.current_bearer.is_some() {
                (
                    vec![
                        BEARER_KEEP.to_string(),
                        BEARER_CHANGE.to_string(),
                        BEARER_REMOVE.to_string(),
                    ],
                    BEARER_KEEP.to_string(),
                )
            } else {
                (
                    vec![BEARER_SET.to_string(), BEARER_SKIP.to_string()],
                    BEARER_SKIP.to_string(),
                )
            };
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                        field_type: SetupFieldType::Selector as i32,
                        prompt: format!(
                            "Bearer token for {}? (current: {})",
                            state.preset.label(),
                            if state.current_bearer.is_some() {
                                "set"
                            } else {
                                "none"
                            }
                        ),
                        options,
                        default_value,
                    })),
                }))
                .await;
            state.step = OpenAiCompatSetupStep::AwaitingBearerChoice {
                endpoint: endpoint_input,
            };
            SetupStepOutcome::Advance
        }

        OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint } => {
            match trimmed {
                BEARER_KEEP => {
                    let bearer = state.current_bearer.clone();
                    advance_to_probe(state, endpoint, bearer, tx, pending).await
                }
                BEARER_CHANGE | BEARER_SET => {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Setup(
                                SetupPrompt {
                                    field_type: SetupFieldType::Secret as i32,
                                    prompt: format!(
                                        "Enter the {} bearer token:",
                                        state.preset.label()
                                    ),
                                    options: vec![],
                                    default_value: String::new(),
                                },
                            )),
                        }))
                        .await;
                    state.step = OpenAiCompatSetupStep::AwaitingBearerToken { endpoint };
                    SetupStepOutcome::Advance
                }
                BEARER_REMOVE | BEARER_SKIP => {
                    advance_to_probe(state, endpoint, None, tx, pending).await
                }
                other => {
                    // Unrecognized choice — re-emit the Selector at the
                    // same step. Conservatively treat as Keep when one
                    // exists, Skip otherwise.
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Unknown bearer choice '{other}'. Pick from the options."
                                    ),
                                    msg_type: SystemMessageType::Error as i32,
                                },
                            )),
                        }))
                        .await;
                    state.step = OpenAiCompatSetupStep::AwaitingBearerChoice { endpoint };
                    SetupStepOutcome::Advance
                }
            }
        }

        OpenAiCompatSetupStep::AwaitingBearerToken { endpoint } => {
            // Console enforces non-empty Text; defensive empty=None.
            let bearer = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            advance_to_probe(state, endpoint, bearer, tx, pending).await
        }

        OpenAiCompatSetupStep::AwaitingModelSelection {
            endpoint,
            bearer,
            models,
        } => {
            // Operator picked a model. Validate it's in the offered list.
            let chosen = if trimmed.is_empty() {
                models.first().cloned().unwrap_or_default()
            } else if models.iter().any(|m| m == trimmed) {
                trimmed.to_string()
            } else if state.model_only {
                // `/model` never asked for an endpoint: end the flow and
                // name the way back.
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "'{trimmed}' is not among the {} models {} lists. Run /model again to pick from the list.",
                                    models.len(),
                                    state.preset.label(),
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                return SetupStepOutcome::Error;
            } else {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "Selection '{trimmed}' is not in the offered list of {} models. Restart from endpoint.",
                                    models.len()
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                return reset_to_endpoint_step(state, tx).await;
            };
            complete_openai_compat_setup(state, endpoint, bearer, chosen, tx, db, pending).await
        }
    }
}

/// Run probe_models against the gathered endpoint+bearer; on success,
/// emit the Model Selector and advance state. On failure or 0 models,
/// emit an error message and reset to AwaitingEndpoint.
pub(super) async fn advance_to_probe(
    state: &mut OpenAiCompatSetupState,
    endpoint: String,
    bearer: Option<String>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    _pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) -> SetupStepOutcome {
    // The list comes from where the next turn goes: the typed endpoint, or
    // the gateway's prefix while the gateway is on.
    let probe_base = crate::provider::gateway::base_for(
        state.gateway.as_ref(),
        state.preset.as_kind(),
        &endpoint,
    );
    let routed = state.gateway.is_some();
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: if routed {
                    format!("Probing {probe_base} for the models the gateway's upstream lists…")
                } else {
                    format!("Probing {probe_base} for available models…")
                },
                msg_type: SystemMessageType::Info as i32,
            })),
        }))
        .await;

    let probe = crate::provider::openai_compat::OpenAICompatProvider::probe_models(
        state.preset,
        &probe_base,
        bearer.as_deref(),
    )
    .await;
    let models = match probe {
        Ok(m) if m.is_empty() => {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!(
                                "No models found at {probe_base}. Pull or load a model on the server, then try again."
                            ),
                            msg_type: SystemMessageType::Error as i32,
                        },
                    )),
                }))
                .await;
            return reset_to_endpoint_step(state, tx).await;
        }
        Ok(m) => m,
        Err(e) => {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: if routed {
                                format!(
                                    "Probe through the gateway at {probe_base} failed: {e}. Its \
                                     [backends.{}] upstream must answer, or /local-ai-gateway off sets \
                                     the preset up directly. Re-entering endpoint step.",
                                    crate::provider::gateway::prefix(state.preset.as_kind())
                                )
                            } else {
                                format!("Probe failed: {e}. Re-entering endpoint step.")
                            },
                            msg_type: SystemMessageType::Error as i32,
                        },
                    )),
                }))
                .await;
            return reset_to_endpoint_step(state, tx).await;
        }
    };

    // The current model, when the server still lists it, leads the list
    // and is the default: the console's selector starts on the first
    // option whatever the default, so Enter keeps the model.
    let mut models = models;
    if let Some(at) = state.current_model.as_ref().and_then(|c| models.iter().position(|m| m == c)) {
        let listed = models.remove(at);
        models.insert(0, listed);
    }
    let default_model = models[0].clone();

    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(SetupPrompt {
                field_type: SetupFieldType::Selector as i32,
                prompt: "Select a model:".to_string(),
                options: models.clone(),
                default_value: default_model,
            })),
        }))
        .await;

    state.step = OpenAiCompatSetupStep::AwaitingModelSelection {
        endpoint,
        bearer,
        models,
    };
    SetupStepOutcome::Advance
}

/// Reset state to AwaitingEndpoint and re-emit the Endpoint Text prompt
/// (used as a recovery path for probe-fail / 0-models / invalid model).
pub(super) async fn reset_to_endpoint_step(
    state: &mut OpenAiCompatSetupState,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
) -> SetupStepOutcome {
    let prompt = endpoint_prompt(state, true);
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Setup(prompt)),
        }))
        .await;
    state.step = OpenAiCompatSetupStep::AwaitingEndpoint;
    SetupStepOutcome::Advance
}

/// Persist endpoint+model to config, bearer to STATE file (with env
/// hot-reload), emit a summary, clear the pending state.
pub(super) async fn complete_openai_compat_setup(
    state: &OpenAiCompatSetupState,
    endpoint: String,
    bearer: Option<String>,
    model: String,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) -> SetupStepOutcome {
    use crate::provider::openai_compat::OpenAiCompatPreset;

    // 1. Persist endpoint + model on config; api_provider untouched.
    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(_) => {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: "No system config — re-run setup wizard first.".to_string(),
                            msg_type: SystemMessageType::Error as i32,
                        },
                    )),
                }))
                .await;
            *pending.lock().await = None;
            return SetupStepOutcome::Error;
        }
    };
    if state.model_only {
        // `/model` on the active preset: the model moves, and the endpoint
        // and the bearer stay as they are.
        set_local_model(&mut cfg, state.preset, &model);
        let outcome = match config::save_config(db, &cfg).await {
            Ok(()) => {
                crate::provider::health::request_probe();
                (local_model_set_message(&cfg, state.preset, &model), SystemMessageType::Info, SetupStepOutcome::Complete)
            }
            Err(e) => (format!("Failed to persist config: {e}"), SystemMessageType::Error, SetupStepOutcome::Error),
        };
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                    content: outcome.0,
                    msg_type: outcome.1 as i32,
                })),
            }))
            .await;
        *pending.lock().await = None;
        return outcome.2;
    }
    match state.preset {
        OpenAiCompatPreset::Ollama => {
            cfg.openai_compat.ollama_endpoint = endpoint.clone();
        }
        OpenAiCompatPreset::LmStudio => {
            cfg.openai_compat.lm_studio_endpoint = endpoint.clone();
        }
    }
    set_local_model(&mut cfg, state.preset, &model);
    if let Err(e) = config::save_config(db, &cfg).await {
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                    content: format!("Failed to persist config: {e}"),
                    msg_type: SystemMessageType::Error as i32,
                })),
            }))
            .await;
        *pending.lock().await = None;
        return SetupStepOutcome::Error;
    }

    // 2. Bearer STATE + env hot-reload.
    let bearer_path = bearer_state_path(state.preset);
    let env_var = bearer_env_var(state.preset);
    let bearer_change_label = match (&state.current_bearer, &bearer) {
        (Some(prev), Some(new)) if prev == new => "kept",
        (_, Some(_)) => "set",
        (Some(_), None) => "removed",
        (None, None) => "none",
    };
    match &bearer {
        Some(token) if !token.is_empty() => {
            if let Err(e) = config::write_credential_state(bearer_path, token) {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("Could not write bearer to STATE: {e}"),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
            }
            // SAFETY: slash-command handler context, single-threaded
            // env access at this point. Hot-reload so the next
            // build_openai_compat_provider call picks up the new
            // bearer without a brain restart.
            unsafe {
                std::env::set_var(env_var, token);
            }
        }
        _ => {
            let _ = std::fs::remove_file(bearer_path);
            unsafe {
                std::env::remove_var(env_var);
            }
        }
    }

    // The preset's endpoint / model / bearer changed: re-probe the active
    // provider now rather than at the next 5-minute tick (a no-op change
    // if this preset is not the active provider).
    crate::provider::health::request_probe();

    // 3. Acknowledge and clear state.
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: format!(
                    "{} reconfigured: endpoint={}, bearer={bearer_change_label}, model={model}. \
                     Switch to it with /provider {} when ready.",
                    state.preset.label(),
                    if endpoint.is_empty() {
                        "none (through the gateway only)"
                    } else {
                        endpoint.as_str()
                    },
                    state.preset.as_kind().as_str(),
                ),
                msg_type: SystemMessageType::Info as i32,
            })),
        }))
        .await;

    *pending.lock().await = None;
    SetupStepOutcome::Complete
}

/// Validate + persist a candidate API key submitted via the
/// `/provider --setup` multi-turn flow. On valid, writes the per-
/// provider config field and STATE file, clears the pending flag,
/// and acknowledges. On invalid, leaves the flag set so the next
/// message is re-intercepted (the operator can retype without
/// re-running --setup).
pub(super) async fn handle_pending_key_setup(
    target: ProviderKind,
    candidate: String,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending_key_setup: &Arc<Mutex<Option<KeySetupStep>>>,
) {
    let send_msg = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };

    if candidate.is_empty() {
        send_msg(
            format!(
                "Empty key — type the {} API key on the next message, or use a non-/provider command to abort setup.",
                target.as_str()
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    // The config first: the key is persisted into it, and the check goes
    // where the next turn would (through the provider gateway when on).
    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(_) => {
            send_msg(
                "No system config found — re-run setup wizard first.".to_string(),
                SystemMessageType::Error,
            )
            .await;
            // Clear flag — config is broken, bailing entirely.
            *pending_key_setup.lock().await = None;
            return;
        }
    };

    // Validate via the same probe path the wizard uses.
    if let Err(msg) =
        config::check_api_key(target, &candidate, cfg.provider_gateway.as_ref()).await
    {
        send_msg(
            format!(
                "{} Try again on the next message, or use a non-/provider command to abort.",
                msg
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    }
    match target {
        ProviderKind::Anthropic => cfg.anthropic_api_key = Some(candidate.clone()),
        ProviderKind::Gemini => cfg.gemini_api_key = Some(candidate.clone()),
        // OpenAI-compat presets reach this site only if /provider --setup
        // accepted them above. Today's gating refuses them outright; this
        // arm exists for exhaustive-match completeness only.
        ProviderKind::Ollama | ProviderKind::LmStudio => {
            // No-op: Stage 5 will route the bearer to STATE files instead
            // of SystemConfig fields.
        }
    }
    if let Err(e) = config::save_config(db, &cfg).await {
        send_msg(
            format!("Failed to persist key: {}", e),
            SystemMessageType::Error,
        )
        .await;
        return;
    }
    // A new key for this provider: re-probe now (the probe reads the key
    // from the config just saved, the same way a turn does).
    crate::provider::health::request_probe();

    // Persist to per-provider STATE file with mode 0600 per Locked
    // Decision #8 (Sprint 5 retroactive fix to existing api_key_*
    // writes that previously used default umask).
    let state_path = match target {
        ProviderKind::Anthropic => "/embra/state/api_key_anthropic",
        ProviderKind::Gemini => "/embra/state/api_key_gemini",
        // Stage 4/5: bearer file routing for OpenAI-compat presets.
        ProviderKind::Ollama => "/embra/state/bearer_ollama",
        ProviderKind::LmStudio => "/embra/state/bearer_lm_studio",
    };
    if let Err(e) = config::write_credential_state(state_path, &candidate) {
        warn!(
            target: "config",
            "Could not write per-provider key to {}: {}",
            state_path,
            e
        );
    }

    // The key is in. The model comes next, the way the local presets'
    // setup ends with theirs.
    match step_after_saved_key(target) {
        Some(step) => {
            *pending_key_setup.lock().await = Some(step);
            send_msg(
                format!("Key for {} recorded.", target.as_str()),
                SystemMessageType::Info,
            )
            .await;
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Setup(
                        cloud_model_selection(target, &cfg),
                    )),
                }))
                .await;
        }
        None => {
            *pending_key_setup.lock().await = None;
            send_msg(
                format!(
                    "Key for {} recorded. Use /provider {} to switch, or stay on the current provider.",
                    target.as_str(),
                    target.as_str()
                ),
                SystemMessageType::Info,
            )
            .await;
        }
    }
}

/// What follows a saved key: the model step, for the two providers whose
/// setup takes a key.
pub(super) fn step_after_saved_key(target: ProviderKind) -> Option<KeySetupStep> {
    match target {
        ProviderKind::Anthropic | ProviderKind::Gemini => Some(KeySetupStep::Model(target)),
        ProviderKind::Ollama | ProviderKind::LmStudio => None,
    }
}

/// "Anthropic" or "Gemini", for the model step's messages.
pub(super) fn cloud_provider_name(target: ProviderKind) -> &'static str {
    match target {
        ProviderKind::Gemini => "Gemini",
        _ => "Anthropic",
    }
}

/// The model step of `/provider --setup <anthropic|gemini>`: the
/// provider's wizard labels, with the current model first and as the
/// default (the console's selector starts on the first option, so Enter
/// keeps the model). A current model that no label names (the legacy
/// `opus-4.7`, an env override, the customtools id) leaves the wizard
/// order; the prompt names it either way.
pub(super) fn cloud_model_selection(target: ProviderKind, cfg: &config::SystemConfig) -> SetupPrompt {
    let labels: &[&str] = match target {
        ProviderKind::Gemini => &config::GEMINI_WIZARD_LABELS,
        _ => &config::ANTHROPIC_WIZARD_LABELS,
    };
    let current = display_model_for(target.as_str(), cfg);
    let mut options: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    if let Some(at) = options
        .iter()
        .position(|l| model_alias_for_label(target, l).as_deref() == Some(current.as_str()))
    {
        let label = options.remove(at);
        options.insert(0, label);
    }
    SetupPrompt {
        field_type: SetupFieldType::Selector as i32,
        prompt: format!(
            "Which {} model? Current: {current}.",
            cloud_provider_name(target)
        ),
        default_value: options.first().cloned().unwrap_or_default(),
        options,
    }
}

/// The alias one of the provider's wizard labels seeds.
pub(super) fn model_alias_for_label(target: ProviderKind, label: &str) -> Option<String> {
    match target {
        ProviderKind::Gemini => config::gemini_model_from_label(label),
        _ => config::anthropic_model_from_label(label),
    }
}

/// The alias a model-step answer names: one of the provider's wizard
/// labels, or an alias typed the way `/model` takes it.
pub(super) fn model_alias_for_pick(target: ProviderKind, answer: &str) -> Option<String> {
    let answer = answer.trim();
    model_alias_for_label(target, answer).or_else(|| match target {
        ProviderKind::Gemini => {
            parse_gemini_model_choice(answer).map(|(_, alias)| alias.to_string())
        }
        _ => parse_anthropic_model_choice(answer).map(|(_, display)| display.to_string()),
    })
}

/// The answer to the model step: save the alias the way `/model` does, or
/// say the model stays. Either way the step ends here, and the answer
/// never reaches the model as a message.
pub(super) async fn handle_setup_model_pick(
    target: ProviderKind,
    answer: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    pending_key_setup: &Arc<Mutex<Option<KeySetupStep>>>,
) {
    *pending_key_setup.lock().await = None;
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };
    let name = cloud_provider_name(target);
    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send(
                format!("{name} model: failed to load config: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };
    let current = display_model_for(target.as_str(), &cfg);
    if answer.trim().is_empty() {
        send(
            format!("The {name} model stays {current}."),
            SystemMessageType::Info,
        )
        .await;
        return;
    }
    let Some(alias) = model_alias_for_pick(target, answer) else {
        send(
            format!(
                "'{}' is not a {name} model; the model stays {current}. /model changes it later.",
                answer.trim()
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    };
    match target {
        ProviderKind::Gemini => cfg.gemini_model = Some(alias.clone()),
        _ => cfg.anthropic_model = Some(alias.clone()),
    }
    if let Err(e) = config::save_config(db, &cfg).await {
        send(
            format!("{name} model: failed to save: {e}"),
            SystemMessageType::Error,
        )
        .await;
        return;
    }
    crate::provider::health::request_probe();
    if cfg.api_provider == target.as_str() {
        send(
            format!("{name} model set to {alias}. Takes effect on your next message."),
            SystemMessageType::Info,
        )
        .await;
        send_brain_status(tx, &cfg, session_mgr, &alias).await;
    } else {
        send(
            format!(
                "{name} model set to {alias}. Use /provider {} to switch, or stay on the current provider.",
                target.as_str()
            ),
            SystemMessageType::Info,
        )
        .await;
    }
}

/// What a slash command leaves of the key setup: the key step stays
/// armed, as it always has; the model step ends, and the provider whose
/// step ended is returned so the cancel can be said.
pub(super) fn key_setup_after_slash(
    step: Option<KeySetupStep>,
) -> (Option<KeySetupStep>, Option<ProviderKind>) {
    match step {
        Some(KeySetupStep::Model(target)) => (None, Some(target)),
        other => (other, None),
    }
}

/// Pick the Gemini model id, honoring (in priority order):
/// 1. `EMBRA_GEMINI_MODEL` env var (set by --api-provider startup or
///    by the operator before brain restart).
/// 2. `config.system.gemini_model` (persistent: the alias `/model`
///    stores).
/// 3. The provider crate's default constant (`gemini-3.1-pro-preview`).
///
/// A selectable alias ([`parse_gemini_model_choice`]) becomes its API id;
/// any other value passes through, such as
/// `gemini-3.1-pro-preview-customtools` when the default ignores custom
/// tools (spec D8). The one rule for turns, learning and the health
/// probe. Reads env once via the wrapper; the pure inner is unit-tested
/// directly to avoid env-mutation races across the test suite.
pub(super) fn resolve_gemini_model_id(cfg: &config::SystemConfig) -> String {
    let env_override = std::env::var("EMBRA_GEMINI_MODEL").ok();
    resolve_gemini_model_id_inner(env_override.as_deref(), cfg.gemini_model.as_deref())
}

pub(super) fn resolve_gemini_model_id_inner(env: Option<&str>, cfg_field: Option<&str>) -> String {
    let chosen = env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| cfg_field.filter(|s| !s.is_empty()));
    match chosen {
        Some(s) => parse_gemini_model_choice(s)
            .map_or_else(|| s.to_string(), |(id, _)| id.to_string()),
        None => crate::provider::gemini::DEFAULT_MODEL.to_string(),
    }
}

/// Resolve the active Anthropic model, returning `(api_model_id, display)`.
/// Precedence mirrors [`resolve_gemini_model_id`]:
/// 1. `EMBRA_ANTHROPIC_MODEL` env var (boot/dev override).
/// 2. `config.anthropic_model` (persistent, set via `/model`).
/// 3. The provider's [`DEFAULT_MODEL`](crate::provider::anthropic::DEFAULT_MODEL)
///    (`claude-opus-5`).
///
/// The request shape is identical across supported Anthropic models
/// (Opus 5.5, Opus 5, Opus 4.8, Fable 5.1, Fable 5 — and the legacy 4.7),
/// so this only swaps the `model` id + display name. Inner fn is pure for
/// env-race-free tests.
pub(super) fn resolve_anthropic_model(cfg: &config::SystemConfig) -> (String, String) {
    let env_override = std::env::var("EMBRA_ANTHROPIC_MODEL").ok();
    resolve_anthropic_model_inner(env_override.as_deref(), cfg.anthropic_model.as_deref())
}

pub(super) fn resolve_anthropic_model_inner(
    env: Option<&str>,
    cfg_field: Option<&str>,
) -> (String, String) {
    let raw = [env, cfg_field]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty());
    match raw {
        Some(s) => normalize_anthropic_model(s),
        None => (
            crate::provider::anthropic::DEFAULT_MODEL.to_string(),
            crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string(),
        ),
    }
}

/// Map a SELECTABLE model alias to `(api_id, display)`. The `/model` command
/// only accepts these (so a typo can't persist a bogus id that 400s every
/// turn). Line-up as of 2026-09-27: Opus 5 (default), Opus 5.5, Opus 4.8,
/// Fable 5.1, Fable 5; only 4.7 is retired from selection. The persisted
/// 4.7 alias keeps resolving via [`canonicalize_legacy_anthropic_alias`],
/// and the resolver additionally allows env/config passthrough for a model
/// this build predates — see [`normalize_anthropic_model`].
///
/// Bare aliases: `5` is a version literal and stays Opus 5. `opus` and
/// `fable` are family pointers; `opus` follows the default, and both keep
/// their current targets until the default moves.
pub(crate) fn parse_anthropic_model_choice(s: &str) -> Option<(&'static str, &'static str)> {
    match s.trim().to_ascii_lowercase().as_str() {
        "opus-5.5" | "opus5.5" | "5.5" | "claude-opus-5-5" => {
            Some(("claude-opus-5-5", "opus-5.5"))
        }
        "opus-5" | "opus5" | "opus" | "5" | "claude-opus-5" => {
            Some(("claude-opus-5", "opus-5"))
        }
        "opus-4.8" | "4.8" | "opus4.8" | "claude-opus-4-8" => {
            Some(("claude-opus-4-8", "opus-4.8"))
        }
        "fable-5.1" | "fable5.1" | "claude-fable-5-1" => {
            Some(("claude-fable-5-1", "fable-5.1"))
        }
        "fable-5" | "fable5" | "fable" | "claude-fable-5" => {
            Some(("claude-fable-5", "fable-5"))
        }
        _ => None,
    }
}

/// The selectable line-up in display order: the canonical display alias of
/// every arm in [`parse_anthropic_model_choice`]. `/model` prints it; a
/// test keeps the two in step.
pub(crate) const ANTHROPIC_MODEL_CHOICES: [&str; 5] =
    ["opus-5.5", "opus-5", "opus-4.8", "fable-5.1", "fable-5"];

pub(super) fn anthropic_model_options() -> String {
    ANTHROPIC_MODEL_CHOICES.join(", ")
}

/// Parse a Gemini model choice to `(api_id, alias)`. The alias is what
/// `/model` and the wizard store in `SystemConfig.gemini_model` and what
/// is shown; [`resolve_gemini_model_id`] maps it back to the API id, so
/// a later build can move an alias to a new id without stale ids left in
/// configs. Anything else is not selectable, but still resolves as typed
/// when it comes from env or config.
pub(crate) fn parse_gemini_model_choice(s: &str) -> Option<(&'static str, &'static str)> {
    use crate::provider::gemini::{ARGON_MODEL, DEFAULT_DISPLAY_NAME, DEFAULT_MODEL};
    match s.trim().to_ascii_lowercase().as_str() {
        "gemini-3.1-pro" | "3.1-pro" | "gemini-3.1-pro-preview" => {
            Some((DEFAULT_MODEL, DEFAULT_DISPLAY_NAME))
        }
        "gemini-4-argon" | "4-argon" | "argon" => Some((ARGON_MODEL, ARGON_MODEL)),
        _ => None,
    }
}

/// The selectable Gemini models in display order: the alias of every arm
/// in [`parse_gemini_model_choice`]. `/model` prints it; a test keeps the
/// two in step.
pub(crate) const GEMINI_MODEL_CHOICES: [&str; 2] = ["gemini-3.1-pro", "gemini-4-argon"];

pub(super) fn gemini_model_options() -> String {
    GEMINI_MODEL_CHOICES.join(", ")
}

/// LEGACY alias canonicalization — NOT selectable via `/model`, but a
/// persisted `anthropic_model` of `"opus-4.7"` (wizard-seeded before
/// 2026-07-06, or the frozen v9 backfill) must keep resolving to its
/// real API id forever: dropping this arm would send the bare alias as
/// the model id and 400 every turn on an existing instance. The model stays
/// served by the API; the operator moves off it explicitly via `/model`.
pub(super) fn canonicalize_legacy_anthropic_alias(s: &str) -> Option<(&'static str, &'static str)> {
    match s.trim().to_ascii_lowercase().as_str() {
        "opus-4.7" | "4.7" | "opus4.7" | "claude-opus-4-7" => {
            Some(("claude-opus-4-7", "opus-4.7"))
        }
        _ => None,
    }
}

/// Resolve a raw value to `(api_id, display)`. Selectable aliases and legacy
/// persisted aliases canonicalize; anything else passes through unchanged
/// (forward-compat — the operator owns the exact id via env/config when the
/// build predates the model).
pub(super) fn normalize_anthropic_model(s: &str) -> (String, String) {
    match parse_anthropic_model_choice(s).or_else(|| canonicalize_legacy_anthropic_alias(s)) {
        Some((id, disp)) => (id.to_string(), disp.to_string()),
        None => (s.to_string(), s.to_string()),
    }
}

/// Validate an `output_config.effort` choice against the API allowlist.
/// Unlike the model resolver there is NO unknown-value passthrough — an
/// unrecognized effort would 400 every turn, and unlike model ids the
/// value set is stable across models, so strictness costs nothing.
pub(super) fn parse_effort_choice(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}

/// Resolve the active Anthropic effort level. Precedence mirrors
/// [`resolve_anthropic_model`]: env `EMBRA_ANTHROPIC_EFFORT` >
/// `config.anthropic_effort` (set via `/effort`) > `"max"`. Invalid
/// values fall through to the next source (a hand-edited bogus config
/// value can't 400 every turn). Inner fn is pure for env-race-free tests.
/// The variable is for development: embrad passes none of the `_EFFORT`
/// variables to the brain, so on an instance the level comes from `/effort`.
pub(super) fn resolve_anthropic_effort(cfg: &config::SystemConfig) -> String {
    let env_override = std::env::var("EMBRA_ANTHROPIC_EFFORT").ok();
    resolve_anthropic_effort_inner(env_override.as_deref(), cfg.anthropic_effort.as_deref())
}

pub(super) fn resolve_anthropic_effort_inner(env: Option<&str>, cfg_field: Option<&str>) -> String {
    [env, cfg_field]
        .into_iter()
        .flatten()
        .find_map(parse_effort_choice)
        .unwrap_or("max")
        .to_string()
}

/// The operator's effort posture for `kind`, on the canonical ladder.
/// Anthropic always resolves (default `max`). For the other providers
/// `None` means "provider default" — Gemini `high`, OpenAI-compat the
/// per-model auto-map — which keeps every request byte-identical to
/// before the knob existed. Precedence env (`EMBRA_GEMINI_EFFORT` /
/// `EMBRA_OLLAMA_EFFORT` / `EMBRA_LM_STUDIO_EFFORT`, dev-only: embrad
/// plumbs none of them) > config; invalid values fall through.
pub(super) fn resolve_provider_effort(kind: ProviderKind, cfg: &config::SystemConfig) -> Option<String> {
    use crate::provider::openai_compat::OpenAiCompatPreset;
    let (env_name, cfg_field) = match kind {
        ProviderKind::Anthropic => return Some(resolve_anthropic_effort(cfg)),
        ProviderKind::Gemini => ("EMBRA_GEMINI_EFFORT", cfg.gemini_effort.as_deref()),
        ProviderKind::Ollama => (
            "EMBRA_OLLAMA_EFFORT",
            cfg.openai_compat.effort_for_preset(OpenAiCompatPreset::Ollama),
        ),
        ProviderKind::LmStudio => (
            "EMBRA_LM_STUDIO_EFFORT",
            cfg.openai_compat.effort_for_preset(OpenAiCompatPreset::LmStudio),
        ),
    };
    let env_override = std::env::var(env_name).ok();
    resolve_optional_effort_inner(env_override.as_deref(), cfg_field)
}

pub(super) fn resolve_optional_effort_inner(env: Option<&str>, cfg_field: Option<&str>) -> Option<String> {
    [env, cfg_field]
        .into_iter()
        .flatten()
        .find_map(parse_effort_choice)
        .map(str::to_string)
}

/// Persist an effort level (or clear it) into the active provider's field;
/// on Ollama and LM Studio, the active model's (`OpenAiCompatConfig::set_effort`).
pub(super) fn set_effort_field(cfg: &mut config::SystemConfig, kind: ProviderKind, level: Option<String>) {
    use crate::provider::openai_compat::OpenAiCompatPreset;
    match kind {
        ProviderKind::Anthropic => cfg.anthropic_effort = level,
        ProviderKind::Gemini => cfg.gemini_effort = level,
        ProviderKind::Ollama => cfg.openai_compat.set_effort(OpenAiCompatPreset::Ollama, level),
        ProviderKind::LmStudio => cfg.openai_compat.set_effort(OpenAiCompatPreset::LmStudio, level),
    }
}

/// What the brain sends `model` while no `/effort` level is set.
pub(super) fn automatic_effort_phrase(model: &str) -> String {
    match crate::provider::openai_compat::reasoning_effort_for_model(model) {
        Some(auto) => format!("embraOS sends reasoning_effort={auto} automatically"),
        None => "nothing is sent; the server's per-model default applies".to_string(),
    }
}

/// `/model [<name>]` — show or switch the active provider's model; the
/// loop driver rebuilds the provider per turn from config, so a switch
/// takes effect on the next user message.
/// - Anthropic: one of `ANTHROPIC_MODEL_CHOICES`, persisted as
///   `SystemConfig.anthropic_model`.
/// - Ollama and LM Studio: a model the server lists. Without a name the
///   server's list comes back as a selector (`local_model_selection`), and
///   the pick goes through the reconfigure flow's model step alone.
/// - Gemini: one of `GEMINI_MODEL_CHOICES`, persisted as the alias in
///   `SystemConfig.gemini_model` (`handle_gemini_model_command`).
///
/// A switch emits an Operational ModeChange so the status bar shows the
/// new Brain at once.
pub(super) async fn handle_model_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    pending_openai_compat_setup: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };

    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send(
                format!("/model: failed to load config: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    let active = cfg.api_provider.clone();
    let trimmed = args.trim();

    match ProviderKind::from_str(&active) {
        Some(kind @ (ProviderKind::Ollama | ProviderKind::LmStudio)) => {
            let preset = match kind {
                ProviderKind::Ollama => crate::provider::openai_compat::OpenAiCompatPreset::Ollama,
                _ => crate::provider::openai_compat::OpenAiCompatPreset::LmStudio,
            };
            handle_local_model_command(preset, trimmed, cfg, tx, db, session_mgr, pending_openai_compat_setup).await;
            return;
        }
        Some(ProviderKind::Gemini) => {
            handle_gemini_model_command(trimmed, cfg, tx, db, session_mgr).await;
            return;
        }
        _ => {}
    }

    let options = anthropic_model_options();

    // No args → show current model + options.
    if trimmed.is_empty() {
        let current = display_model_for(&active, &cfg);
        send(
            format!(
                "Anthropic model: {current}. Options: {options}. \
                 Use `/model fable-5` to switch (takes effect on your next message)."
            ),
            SystemMessageType::Info,
        )
        .await;
        return;
    }

    let Some((_id, display)) = parse_anthropic_model_choice(trimmed) else {
        // The legacy 4.7 alias gets a tailored message: it still RESOLVES
        // (persisted configs keep working) but is no longer selectable.
        let msg = if canonicalize_legacy_anthropic_alias(trimmed).is_some() {
            format!(
                "'{trimmed}' was retired from the selectable line-up \
                 (an instance already persisted on it keeps working). Options: \
                 {options}."
            )
        } else {
            format!("'{trimmed}' is not a recognized Anthropic model. Options: {options}.")
        };
        send(msg, SystemMessageType::Error).await;
        return;
    };

    cfg.anthropic_model = Some(display.to_string());
    if let Err(e) = config::save_config(db, &cfg).await {
        send(
            format!("/model: failed to save: {e}"),
            SystemMessageType::Error,
        )
        .await;
        return;
    }
    // The model is part of what the health probe checks: ask for a probe
    // now, as after every other change of the provider, so the status does
    // not describe the previous model until the next tick.
    crate::provider::health::request_probe();

    send(
        format!("Anthropic model set to {display}. Takes effect on your next message."),
        SystemMessageType::Info,
    )
    .await;
    send_brain_status(tx, &cfg, session_mgr, display).await;
}

/// `/model` on Gemini: the current model and the options, or a switch to
/// one of `GEMINI_MODEL_CHOICES`. The alias is saved; the resolver gives
/// the turn its API id. Like the Anthropic arm, a switch asks for a probe
/// and sends the new Brain to the status bar.
pub(super) async fn handle_gemini_model_command(
    trimmed: &str,
    mut cfg: config::SystemConfig,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };
    let options = gemini_model_options();
    if trimmed.is_empty() {
        let current = display_model_for("gemini", &cfg);
        send(
            format!(
                "Gemini model: {current}. Options: {options}. \
                 Use `/model gemini-4-argon` to switch (takes effect on your next message)."
            ),
            SystemMessageType::Info,
        )
        .await;
        return;
    }
    let Some((_id, alias)) = parse_gemini_model_choice(trimmed) else {
        send(
            format!("'{trimmed}' is not a recognized Gemini model. Options: {options}."),
            SystemMessageType::Error,
        )
        .await;
        return;
    };
    cfg.gemini_model = Some(alias.to_string());
    if let Err(e) = config::save_config(db, &cfg).await {
        send(
            format!("/model: failed to save: {e}"),
            SystemMessageType::Error,
        )
        .await;
        return;
    }
    crate::provider::health::request_probe();
    send(
        format!("Gemini model set to {alias}. Takes effect on your next message."),
        SystemMessageType::Info,
    )
    .await;
    send_brain_status(tx, &cfg, session_mgr, alias).await;
}

/// `/model` on the active Ollama or LM Studio preset. The server's list is
/// the set of choices: a name must be on it, and no name brings the list
/// back as a selector. The endpoint and the bearer stay as configured;
/// `/provider --setup <kind>` changes those.
pub(super) async fn handle_local_model_command(
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
    wanted: &str,
    mut cfg: config::SystemConfig,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage { content, msg_type: kind as i32 },
                    )),
                }))
                .await;
        }
    };
    let (endpoint, current) = {
        let (endpoint, model) = cfg.openai_compat.fields(preset);
        (endpoint.to_string(), non_empty(model))
    };
    let bearer = std::env::var(bearer_env_var(preset)).ok().filter(|s| !s.is_empty());
    // The list comes from where the next turn goes: the stored endpoint, or
    // the provider gateway's prefixed base while it is on. The stored
    // endpoint itself, blank for a preset set up through the gateway, is
    // what the selector keeps; with no base there is nothing to list.
    let Some(probe_base) = local_route(&cfg, preset) else {
        send(
            config::PresetNotReady::NoDirectEndpoint {
                preset,
                gateway_stored: cfg.provider_gateway.is_some(),
            }
            .to_string(),
            SystemMessageType::Error,
        )
        .await;
        return;
    };

    let models = match crate::provider::openai_compat::OpenAICompatProvider::probe_models(
        preset,
        &probe_base,
        bearer.as_deref(),
    )
    .await
    {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            send(
                format!("No models found at {probe_base}. Pull or load a model on the server, then run /model again."),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
        Err(e) => {
            send(
                format!(
                    "Could not list the models at {probe_base}: {e}. /provider --setup {} reconfigures the endpoint.",
                    preset.as_kind().as_str()
                ),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    if wanted.is_empty() {
        let gateway = cfg.provider_gateway.clone().filter(|g| g.enabled);
        let (prompt, state) =
            local_model_selection(preset, endpoint, bearer, current, models, gateway);
        *pending.lock().await = Some(state);
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(prompt)),
            }))
            .await;
        return;
    }

    let model = match local_model_choice(preset, &models, wanted) {
        Ok(model) => model,
        Err(msg) => {
            send(msg, SystemMessageType::Error).await;
            return;
        }
    };
    set_local_model(&mut cfg, preset, &model);
    if let Err(e) = config::save_config(db, &cfg).await {
        send(format!("/model: failed to save: {e}"), SystemMessageType::Error).await;
        return;
    }
    crate::provider::health::request_probe();
    send(local_model_set_message(&cfg, preset, &model), SystemMessageType::Info).await;
    send_brain_status(tx, &cfg, session_mgr, &model).await;
}

/// Where a preset's requests go now: the stored endpoint, or the provider
/// gateway's prefixed base while the gateway is on; `None` for a preset set
/// up through the gateway with no direct endpoint while the gateway is off
/// or gone. Never the preset's default: inside the OS `localhost` is the
/// machine itself.
pub(super) fn local_route(
    cfg: &config::SystemConfig,
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
) -> Option<String> {
    let (endpoint, _) = cfg.openai_compat.fields(preset);
    non_empty(&crate::provider::gateway::base_for(
        cfg.provider_gateway.as_ref(),
        preset.as_kind(),
        endpoint,
    ))
}

/// Point the preset at `model`. Each model keeps its own `/effort` level;
/// one stored the old way stays with the model in use until now
/// (`OpenAiCompatConfig::set_model`).
pub(super) fn set_local_model(
    cfg: &mut config::SystemConfig,
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
    model: &str,
) {
    cfg.openai_compat.set_model(preset, model);
}

/// The switch's answer, with the level the new model runs at.
pub(super) fn local_model_set_message(
    cfg: &config::SystemConfig,
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
    model: &str,
) -> String {
    use crate::provider::openai_compat::OpenAiCompatPreset;
    let kind = match preset {
        OpenAiCompatPreset::Ollama => ProviderKind::Ollama,
        OpenAiCompatPreset::LmStudio => ProviderKind::LmStudio,
    };
    let effort = match resolve_provider_effort(kind, cfg) {
        Some(level) => format!("{level}, set with /effort for this model"),
        None => format!("not set — {}", automatic_effort_phrase(model)),
    };
    format!(
        "{} model set to {model}. Effort: {effort}. Takes effect on your next message.",
        preset.label()
    )
}

/// How many of the server's models a refusal names.
pub(super) const LOCAL_MODELS_NAMED: usize = 12;

/// The model a `/model <name>` means: the exact name the server lists, or
/// the one name that matches it ignoring case. Anything else is refused
/// with the list.
pub(super) fn local_model_choice(
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
    models: &[String],
    wanted: &str,
) -> Result<String, String> {
    if let Some(exact) = models.iter().find(|m| m.as_str() == wanted) {
        return Ok(exact.clone());
    }
    let folded: Vec<&String> = models.iter().filter(|m| m.eq_ignore_ascii_case(wanted)).collect();
    if let [only] = folded.as_slice() {
        return Ok((*only).clone());
    }
    let named: Vec<&str> = models.iter().take(LOCAL_MODELS_NAMED).map(String::as_str).collect();
    let more = models.len().saturating_sub(named.len());
    Err(format!(
        "'{wanted}' is not a model {} lists. It lists: {}{}. /model alone picks from the list.",
        preset.label(),
        named.join(", "),
        if more > 0 { format!(" and {more} more") } else { String::new() },
    ))
}

/// The selector `/model` shows for a local server, and the reconfigure
/// state that receives the pick at its model step: the endpoint and
/// bearer kept. The current model, when the server still lists it, is the
/// first option as well as the default: the console's selector starts on
/// the first option whatever the default, so Enter keeps the model.
pub(super) fn local_model_selection(
    preset: crate::provider::openai_compat::OpenAiCompatPreset,
    endpoint: String,
    bearer: Option<String>,
    current: Option<String>,
    mut models: Vec<String>,
    gateway: Option<config::ProviderGateway>,
) -> (SetupPrompt, OpenAiCompatSetupState) {
    if let Some(at) = current.as_ref().and_then(|c| models.iter().position(|m| m == c)) {
        let listed = models.remove(at);
        models.insert(0, listed);
    }
    let default_value = models.first().cloned().unwrap_or_default();
    let prompt = SetupPrompt {
        field_type: SetupFieldType::Selector as i32,
        prompt: format!("Select an {} model:", preset.label()),
        options: models.clone(),
        default_value,
    };
    let state = OpenAiCompatSetupState {
        preset,
        current_endpoint: non_empty(&endpoint),
        current_bearer: bearer.clone(),
        current_model: current,
        step: OpenAiCompatSetupStep::AwaitingModelSelection { endpoint, bearer, models },
        model_only: true,
        gateway,
    };
    (prompt, state)
}
