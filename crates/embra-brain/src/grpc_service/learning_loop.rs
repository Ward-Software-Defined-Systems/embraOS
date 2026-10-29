//! Learning Mode: the learning loop and the seal.

use super::*;

/// Build the Learning→Operational `ModeChange` announced when the soul is
/// sealed. Emitted both by the stream that drives learning to completion AND
/// by any other stream woken out of its own blocked learning loop by the
/// onboarding-stage signal — hence one shared constructor. Clients parse the
/// message for the status bar (`Name:`/`TZ:`/`Brain:`), so the shape is
/// load-bearing; see the `soul_sealed_mode_change_tests` regression guards.
pub(super) fn soul_sealed_mode_change(config: &config::SystemConfig) -> ConversationResponse {
    ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Learning as i32,
                to_mode: OperatingMode::Operational as i32,
                message: format!(
                    "Soul sealed — Name: {} — TZ: {} — Brain: {}",
                    config.name,
                    config.timezone,
                    display_model_for(&config.api_provider, config)
                ),
            },
        )),
    }
}

/// Drive the 6-phase Learning Mode over gRPC.
/// Creates LearningState, runs each phase with Brain calls, detects [PHASE_COMPLETE],
/// persists extracted documents, and transitions to Operational when complete.
pub(super) async fn run_learning_loop(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    incoming: &mut Streaming<ConversationRequest>,
    db: &Arc<WardsonDbClient>,
    config: &config::SystemConfig,
    api_key: &str,
    onboarding_stage: &Arc<watch::Sender<i32>>,
    stop: TurnStop<'_>,
) -> anyhow::Result<()> {
    let mut state = learning::LearningState::new();

    // Local working copy of the config: Phase 2 can rename the
    // intelligence (identity → config name sync in handle_phase_complete),
    // and phases 3–6 prompts, thinking frames, and the seal-time
    // ModeChange must all carry the new name without re-loading config
    // every iteration.
    let mut config = config.clone();

    // Construct the right provider once per learning session.
    // Learning never invokes tools; pass an empty tool manifest so
    // the request body omits `tools` / `tool_choice` (the provider
    // handles that gating). Provider selection comes from the
    // persisted config (or its anthropic default for first-run).
    let provider_kind = ProviderKind::from_str(&config.api_provider)
        .unwrap_or(ProviderKind::Anthropic);
    if provider_kind == ProviderKind::Gemini {
        let model_id = resolve_gemini_model_id(&config);
        info!(
            target: "gemini::diag",
            model = %model_id,
            explicit_cache = crate::provider::gemini::explicit_cache_supported(&model_id),
            "gemini learning turn starting"
        );
    }
    // The one builder the turn loop and the replicant check use: the key
    // fallback, the model and effort resolvers, the context cache and the
    // provider gateway all come with it. An unconfigured preset is the
    // error it names.
    let provider: Arc<dyn LlmProvider> =
        build_provider_from_config(&config, api_key, Some(db.clone()))
            .map_err(|msg| anyhow::anyhow!(msg))?;
    let empty_manifest: ToolManifest = provider.build_tool_manifest(&[]);

    // Resume support: load any previously persisted documents
    if let Ok(profile) = db.read("memory.user", "user").await {
        state.user_profile = Some(profile);
        state.phase = learning::LearningPhase::IdentityFormation;
    }
    if let Ok(identity) = db.read("memory.identity", "identity").await {
        state.identity = Some(identity);
        state.phase = learning::LearningPhase::SoulDefinition;
    }
    // If soul is sealed, we shouldn't be here — but check anyway
    if learning::is_soul_sealed(db).await.unwrap_or(false) {
        return Ok(());
    }

    // Send mode transition. Sprint 4: include Brain: <model> so the
    // console status bar refreshes during learning (would otherwise
    // sit on the default "opus-5" until next session-attach).
    let learning_model = display_model_for(&config.api_provider, &config);
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Setup as i32,
                to_mode: OperatingMode::Learning as i32,
                message: format!(
                    "Learning Mode — Name: {} — Phase: {} — TZ: {} — Brain: {}",
                    config.name,
                    learning::phase_label(&state.phase),
                    config.timezone,
                    learning_model
                ),
            }
        )),
    })).await;

    // Wake signal: fires when ANOTHER stream advances the global onboarding
    // stage (i.e. seals the soul) while we sit blocked on `incoming.next()`
    // below. Without it, a second client that entered its own learning loop
    // stays frozen on Learning after the driver completes onboarding.
    let mut stage_rx = onboarding_stage.subscribe();

    // The import offer fires once per learning loop, at the entry to
    // Phase 2 (identity formation). Deliberately loop-local and
    // unpersisted: a disconnect mid-offer wrote nothing, so the next
    // learning loop re-offers. Covers the fresh-boot path AND the
    // production re-seal ceremony (whose resume-seed lands at
    // IdentityFormation with memory.user intact).
    let mut import_offered = false;

    loop {
        if state.phase == learning::LearningPhase::Complete {
            break;
        }

        if state.phase == learning::LearningPhase::IdentityFormation && !import_offered {
            import_offered = true;
            use crate::identity_graph::import_flow::{offer_import, ImportOutcome};
            match offer_import(tx, incoming, &mut stage_rx, db, &config, &mut state).await? {
                ImportOutcome::NoCandidates | ImportOutcome::Conversational => {
                    // Fall through to the normal Phase-2 kickoff below.
                }
                ImportOutcome::SealedByOtherStream => {
                    // Mirror the wait-block handling: announce Operational
                    // on OUR stream and hand off.
                    let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                    return Ok(());
                }
                ImportOutcome::Disconnected => {
                    debug!("Client disconnected during import dialogue");
                    return Ok(());
                }
                ImportOutcome::Imported { renamed_to, summary_line } => {
                    if let Some(new_name) = renamed_to {
                        // Same operator-facing pair as the Phase-2 name
                        // sync: an Info line + a Learning→Learning
                        // ModeChange so the console status bar picks up
                        // the imported name mid-learning.
                        let old_name = std::mem::replace(&mut config.name, new_name);
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Name updated: {} → {} (from the imported \
                                         identity graph; persisted to config).",
                                        old_name, config.name
                                    ),
                                    msg_type: SystemMessageType::Info as i32,
                                }
                            )),
                        })).await;
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::ModeChange(
                                ModeTransition {
                                    from_mode: OperatingMode::Learning as i32,
                                    to_mode: OperatingMode::Learning as i32,
                                    message: format!(
                                        "Learning Mode — Name: {} — Phase: {} — TZ: {} — Brain: {}",
                                        config.name,
                                        learning::phase_label(&state.phase),
                                        config.timezone,
                                        learning_model
                                    ),
                                }
                            )),
                        })).await;
                    }
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: summary_line,
                                msg_type: SystemMessageType::Info as i32,
                            }
                        )),
                    })).await;
                    // state.phase is InitialToolset — the Phase-4
                    // special case runs on the next iteration.
                    continue;
                }
            }
        }

        // Phase 4 is non-interactive: render a deterministic tool summary,
        // persist an "all_enabled" registry doc, and auto-advance to Confirmation
        // without consulting the Brain.
        if state.phase == learning::LearningPhase::InitialToolset {
            let summary = learning::tool_summary_message(&config.name);
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: summary,
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;

            if let Err(e) = learning::handle_phase_complete(&mut state, db, &config).await {
                error!("Phase 4 auto-advance failed: {}", e);
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Error processing phase: {}", e),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
            }

            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
            continue;
        }

        // Build system prompt for current phase
        let system_prompt = learning::system_prompt_for_phase(&state, &config);
        let system_bundle = SystemPromptBundle {
            text: system_prompt,
            session_name: "learning".to_string(),
        };

        // Send phase kickoff as first message
        let kickoff = learning::phase_kickoff(&state.phase);
        if !kickoff.is_empty() {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: kickoff.clone(),
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
        }

        // Add kickoff as user message to conversation history and call Brain
        state.conversation_history.push(Message::user(&kickoff));

        // Send thinking indicator
        let _ = tx.send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Thinking(
                ThinkingState { is_thinking: true, name: config.name.clone(), current_tool: String::new() }
            )),
        })).await;

        // Call Brain with conversation history (text-only neutral IR).
        let messages: Vec<ApiMessage> = crate::sessions::model_view::fold_stopped_turns(
            state.conversation_history.iter().map(legacy_message_to_api).collect(),
        );
        // A turn of the learning loop is a turn: marked as running from
        // here to the end of its stream, so that an operator stop reaches it.
        let (in_turn_guard, mut stop_check) = stop.begin("learning");
        let mut brain_rx = provider
            .stream_turn(
                &messages,
                &system_bundle,
                &empty_manifest,
                LlmRequestOptions {
                    include_reasoning: config.show_reasoning(),
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("Brain call failed in learning: {}", e))?;

        let answer = stream_brain_to_grpc(&mut brain_rx, tx, &config.name, Some(&mut stop_check)).await;
        drop(in_turn_guard);

        // What goes into the conversation history, and whether the phase is
        // over (`[PHASE_COMPLETE]`, never on a stopped turn).
        let (history_entry, phase_complete) = learning_turn_outcome(&answer);
        state.conversation_history.push(Message::assistant(&history_entry));

        if phase_complete {
            // Persist extracted documents and advance phase
            match learning::handle_phase_complete(&mut state, db, &config).await {
                Err(e) => {
                    error!("Phase complete handling failed: {}", e);
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("Error processing phase: {}", e),
                                msg_type: SystemMessageType::Error as i32,
                            }
                        )),
                    })).await;
                }
                Ok(Some(new_name)) => {
                    // Phase 2 renamed the intelligence (identity → config
                    // sync, already persisted). Adopt it in the loop's
                    // working copy so phases 3–6 prompts and the seal-time
                    // ModeChange carry it, tell the operator, and refresh
                    // the console name mid-learning (the console parses the
                    // `Name:` token on any ModeTransition).
                    let old_name = std::mem::replace(&mut config.name, new_name);
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "Name updated: {} → {} (synced from the Phase-2 \
                                     identity; persisted to config).",
                                    old_name, config.name
                                ),
                                msg_type: SystemMessageType::Info as i32,
                            }
                        )),
                    })).await;
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::ModeChange(
                            ModeTransition {
                                from_mode: OperatingMode::Learning as i32,
                                to_mode: OperatingMode::Learning as i32,
                                message: format!(
                                    "Learning Mode — Name: {} — Phase: {} — TZ: {} — Brain: {}",
                                    config.name,
                                    learning::phase_label(&state.phase),
                                    config.timezone,
                                    learning_model
                                ),
                            }
                        )),
                    })).await;
                }
                Ok(None) => {}
            }

            if state.phase == learning::LearningPhase::Complete {
                // Save learning conversation history
                let _ = save_learning_history(db, &state.conversation_history).await;

                // Transition to Operational
                let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                // We're the driver — soul is sealed. Wake any other stream
                // blocked in its own learning loop so it advances too.
                let _ = onboarding_stage.send_replace(OperatingMode::Operational as i32);
                break;
            } else {
                // Notify phase change
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                            msg_type: SystemMessageType::Info as i32,
                        }
                    )),
                })).await;
            }
        } else {
            // No [PHASE_COMPLETE] — wait for user input and continue conversation
            loop {
                let next = tokio::select! {
                    msg = incoming.next() => msg,
                    _ = stage_rx.changed() => {
                        // Another stream sealed the soul while we sat blocked
                        // here waiting for input that may never come. Re-check
                        // the authoritative DB state; if sealed, announce
                        // Operational on OUR own stream and hand off to the
                        // operational loop (the converse handler runs its
                        // post-learning setup on `Ok`). This is the fix for a
                        // second client staying frozen on Learning after another
                        // completes onboarding. The task owns an `Arc<Sender>`
                        // clone for its lifetime, so `changed()` never errors.
                        if learning::is_soul_sealed(db).await.unwrap_or(false) {
                            let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                            return Ok(());
                        }
                        continue;
                    }
                };
                match next {
                    Some(Ok(req)) => {
                        if let Some(conversation_request::RequestType::UserMessage(um)) = req.request_type {
                            state.conversation_history.push(Message::user(&um.content));

                            // Rebuild system prompt (may include newly extracted docs)
                            let system_prompt = learning::system_prompt_for_phase(&state, &config);
                            let system_bundle = SystemPromptBundle {
                                text: system_prompt,
                                session_name: "learning".to_string(),
                            };

                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Thinking(
                                    ThinkingState { is_thinking: true, name: config.name.clone(), current_tool: String::new() }
                                )),
                            })).await;

                            let messages: Vec<ApiMessage> = crate::sessions::model_view::fold_stopped_turns(
                                state.conversation_history.iter().map(legacy_message_to_api).collect(),
                            );
                            let (in_turn_guard, mut stop_check) = stop.begin("learning");
                            let mut brain_rx = provider
                                .stream_turn(
                                    &messages,
                                    &system_bundle,
                                    &empty_manifest,
                                    LlmRequestOptions {
                                        include_reasoning: config.show_reasoning(),
                                    },
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;

                            let answer = stream_brain_to_grpc(&mut brain_rx, tx, &config.name, Some(&mut stop_check)).await;
                            drop(in_turn_guard);

                            let (history_entry, phase_complete) = learning_turn_outcome(&answer);
                            state.conversation_history.push(Message::assistant(&history_entry));

                            if phase_complete {
                                if let Err(e) = learning::handle_phase_complete(&mut state, db, &config).await {
                                    error!("Phase complete handling failed: {}", e);
                                }

                                if state.phase == learning::LearningPhase::Complete {
                                    let _ = save_learning_history(db, &state.conversation_history).await;
                                    let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                                    // Driver path — soul sealed. Wake any other
                                    // stream blocked in its own learning loop so
                                    // it advances to Operational too.
                                    let _ = onboarding_stage
                                        .send_replace(OperatingMode::Operational as i32);
                                    return Ok(());
                                } else {
                                    let _ = tx.send(Ok(ConversationResponse {
                                        response_type: Some(conversation_response::ResponseType::System(
                                            SystemMessage {
                                                content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                                                msg_type: SystemMessageType::Info as i32,
                                            }
                                        )),
                                    })).await;
                                }
                                break; // Back to outer loop for next phase kickoff
                            }
                            // No phase complete — continue waiting for user input
                        }
                        // Ignore non-UserMessage during learning
                    }
                    Some(Err(e)) => { warn!("Stream error during learning: {}", e); return Err(e.into()); }
                    None => { debug!("Client disconnected during learning"); return Ok(()); }
                }
            }
        }
    }

    Ok(())
}

/// What one model turn of the learning loop produced.
pub(super) struct LearningAnswer {
    /// The concatenation of all `TextDelta`s.
    pub(super) text: String,
    /// The operator stopped the turn; `text` is what had arrived by then.
    pub(super) stopped: bool,
}

/// What a learning turn leaves in the conversation history, and whether it
/// ends the phase.
///
/// The marker `[PHASE_COMPLETE]` never goes into the history. An answer
/// without text is written as `(phase complete)`: an empty assistant message
/// is rejected by the API, and a model may send the marker alone. A turn the
/// operator stopped ends no phase, whatever had arrived of it, and is marked
/// as the operational loop marks one.
pub(super) fn learning_turn_outcome(answer: &LearningAnswer) -> (String, bool) {
    let clean = answer.text.replace("[PHASE_COMPLETE]", "").trim().to_string();
    if answer.stopped {
        let entry = final_assistant_text(
            &clean,
            &[],
            TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop),
        );
        return (entry, false);
    }
    let phase_complete = answer.text.contains("[PHASE_COMPLETE]");
    let entry = if clean.is_empty() {
        "(phase complete)".to_string()
    } else {
        clean
    };
    (entry, phase_complete)
}

/// Stream a provider response to gRPC for the learning loop, return
/// the full response text (concatenation of all `TextDelta`s) and whether
/// the operator stopped it.
pub(super) async fn stream_brain_to_grpc(
    brain_rx: &mut BoxStream<'static, StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    _name: &str,
    mut stop: Option<&mut StopCheck>,
) -> LearningAnswer {
    let mut full_response = String::new();
    let mut first_token = true;

    loop {
        // Same stop-aware receive as collect_response: on operator stop,
        // abandon the stream (dropping it exits the provider pump and
        // severs the connection), surface the notice + Done so the client
        // animation terminates, and hand the partial back to the learning
        // loop — which simply waits for the operator's next message.
        let event = match stop.as_deref_mut() {
            Some(check) => {
                tokio::select! {
                    ev = brain_rx.next() => ev,
                    _ = check.triggered() => {
                        // The Done first: it turns the partial the operator
                        // watched into a row; the Warning lands under it,
                        // where "above" is true.
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Done(
                                StreamDone { full_response: full_response.clone() }
                            )),
                        })).await;
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: "Turn stopped by operator — any partial response above is incomplete.".to_string(),
                                    msg_type: SystemMessageType::Warning as i32,
                                }
                            )),
                        })).await;
                        return LearningAnswer { text: full_response, stopped: true };
                    }
                }
            }
            None => brain_rx.next().await,
        };
        let Some(event) = event else { break };
        match event {
            StreamEvent::TextDelta(text) => {
                if first_token {
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Thinking(
                            ThinkingState { is_thinking: false, name: String::new(), current_tool: String::new() }
                        )),
                    })).await;
                    first_token = false;
                }
                full_response.push_str(&text);
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Token(
                        StreamToken { text }
                    )),
                })).await;
            }
            StreamEvent::Error(err) => {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Brain error: {}", err),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
            }
            StreamEvent::Complete(turn) => {
                // Use the assembled turn for the Done payload — this is
                // the equivalent of the pre-refactor `Done(full)` event.
                let full = if full_response.is_empty() {
                    turn_text(&turn)
                } else {
                    full_response.clone()
                };
                let clean = full.replace("[PHASE_COMPLETE]", "").trim().to_string();
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Done(
                        StreamDone { full_response: clean }
                    )),
                })).await;
                if full_response.is_empty() {
                    full_response = full;
                }
            }
            StreamEvent::BlockComplete => {}
            StreamEvent::ReasoningDelta(text) => {
                // Forward to the expression panel via the dedicated
                // ReasoningDelta proto frame. CRITICAL: must NOT append
                // to `full_response` (would persist to session history)
                // and must NOT toggle `is_thinking: false` (the typing
                // indicator stays gated on real text deltas, not
                // reasoning).
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(
                            conversation_response::ResponseType::ReasoningDelta(
                                ReasoningDelta { text },
                            ),
                        ),
                    }))
                    .await;
            }
        }
    }

    LearningAnswer { text: full_response, stopped: false }
}

/// Save learning conversation history to WardSONDB
pub(super) async fn save_learning_history(db: &Arc<WardsonDbClient>, history: &[Message]) -> anyhow::Result<()> {
    let collection = "sessions.learning.history";
    if !db.collection_exists(collection).await? {
        db.create_collection(collection).await?;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let doc = serde_json::json!({
        "_id": "learning",
        "turns": history,
        "completed_at": &now,
    });
    match db.write(collection, &doc).await {
        Ok(_) => {}
        Err(_) => { let _ = db.update(collection, "learning", &doc).await; }
    }

    // Create meta entry so learning session appears in /sessions listing
    let meta_collection = "sessions.learning.meta";
    if !db.collection_exists(meta_collection).await? {
        db.create_collection(meta_collection).await?;
    }
    let meta = serde_json::json!({
        "_id": "learning",
        "name": "learning",
        "state": "Closed",
        "created_at": &now,
        "last_active": &now,
    });
    match db.write(meta_collection, &meta).await {
        Ok(_) => Ok(()),
        Err(_) => db.update(meta_collection, "learning", &meta).await,
    }
}

// ── NATIVE-TOOLS-01 helpers ──
