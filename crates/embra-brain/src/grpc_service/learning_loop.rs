//! Learning Mode: the learning loop and the seal.

use super::*;
use crate::identity_graph::import_flow::{wait_for_user_line, Wait};

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
pub(super) async fn run_learning_loop<S>(
    incoming: &mut S,
    db: &Arc<WardsonDbClient>,
    config: &config::SystemConfig,
    api_key: &str,
    onboarding_stage: &Arc<watch::Sender<i32>>,
    stop: TurnStop<'_>,
    commands: &SetupCommands,
) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<ConversationRequest, Status>> + Unpin,
{
    // The client's stream, which the door for commands answers on too.
    let tx = commands.stream();
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
    // stage (i.e. seals the soul) while we sit blocked on the operator's
    // next message below (`wait_for_user_line`). Without it, a second
    // client that entered its own learning loop stays frozen on Learning
    // after the driver completes onboarding.
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

        // Another console may have sealed the soul while this one was in a
        // turn or at the import offer. A phase before the seal has nothing
        // left to do then: its model turn is not run and nothing is written.
        if before_the_seal(&state.phase) && learning::is_soul_sealed(db).await.unwrap_or(false) {
            let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
            return Ok(());
        }

        if state.phase == learning::LearningPhase::IdentityFormation && !import_offered {
            import_offered = true;
            use crate::identity_graph::import_flow::{offer_import, ImportOutcome};
            match offer_import(tx, incoming, &mut stage_rx, db, &config, &mut state, commands).await? {
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

            match complete_phase(&mut state, db, &mut config, tx, &learning_model).await {
                PhaseEnd::Advanced => {}
                PhaseEnd::SealedElsewhere => {
                    let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                    return Ok(());
                }
                PhaseEnd::Open | PhaseEnd::Failed => {
                    // The registry was not written, and this phase has no
                    // conversation to wait in. It is tried again when the
                    // operator says so: a database that is down would
                    // otherwise be written to as fast as this loop turns.
                    match wait_for_user_line(incoming, &mut stage_rx, db, commands).await? {
                        Wait::Line(_) => continue,
                        Wait::Sealed => {
                            let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                            return Ok(());
                        }
                        Wait::Disconnected => {
                            debug!("Client disconnected during learning");
                            return Ok(());
                        }
                    }
                }
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
        let mut ended = if phase_complete {
            complete_phase(&mut state, db, &mut config, tx, &learning_model).await
        } else {
            PhaseEnd::Open
        };

        // The phase stays open until it advances, a turn a message of the
        // operator's. A completion that failed is tried again by the model's
        // answer to the next one, and never by a turn this loop starts on
        // its own: the kickoff above is sent once a phase.
        while matches!(ended, PhaseEnd::Open | PhaseEnd::Failed) {
            // The operator's message is the conversation's; a command is the
            // door's (`SetupCommands`), answered or told to wait and never
            // sent to the model. A wake with the soul sealed is another
            // console that completed onboarding: this one announces
            // Operational on its own stream and hands off (the converse
            // handler runs its post-learning setup on `Ok`).
            let content = match wait_for_user_line(incoming, &mut stage_rx, db, commands).await? {
                Wait::Line(content) => content,
                Wait::Sealed => {
                    let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
                    return Ok(());
                }
                Wait::Disconnected => {
                    debug!("Client disconnected during learning");
                    return Ok(());
                }
            };
            state.conversation_history.push(Message::user(&content));

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
            ended = if phase_complete {
                complete_phase(&mut state, db, &mut config, tx, &learning_model).await
            } else {
                PhaseEnd::Open
            };
        }

        if matches!(ended, PhaseEnd::SealedElsewhere) {
            let _ = tx.send(Ok(soul_sealed_mode_change(&config))).await;
            return Ok(());
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
        }
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

    Ok(())
}

/// The phases a console runs before the soul is sealed. A console that
/// finds the soul sealed while it is in one of them is behind another
/// console's seal; in the two after it (the toolset, the confirmation) the
/// seal is its own.
fn before_the_seal(phase: &learning::LearningPhase) -> bool {
    matches!(
        phase,
        learning::LearningPhase::UserConfiguration
            | learning::LearningPhase::IdentityFormation
            | learning::LearningPhase::SoulDefinition
    )
}

/// What became of a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseEnd {
    /// The model has not called it complete: the conversation goes on.
    Open,
    /// Its documents are stored and the state is on the next phase.
    Advanced,
    /// Another console sealed the soul while this one was in the phase.
    /// Nothing was written here.
    SealedElsewhere,
    /// Completing it failed, and the state is on the same phase.
    Failed,
}

/// Complete the phase the model called complete, and tell the client what
/// the completion did: a name the phase changed, or the failure.
///
/// The one place a completion is handled. A failure used to fall back into
/// the loop, which sent the phase's kickoff and ran a model turn again at
/// once: with two consoles finishing the first phase together the second
/// write of the profile was refused, and a provider that kept calling the
/// phase complete was asked again without end. It is said to the operator
/// now, and the phase stays open for the next message.
async fn complete_phase(
    state: &mut learning::LearningState,
    db: &Arc<WardsonDbClient>,
    config: &mut config::SystemConfig,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    learning_model: &str,
) -> PhaseEnd {
    let behind_a_seal = before_the_seal(&state.phase);
    if behind_a_seal && learning::is_soul_sealed(db).await.unwrap_or(false) {
        return PhaseEnd::SealedElsewhere;
    }
    match learning::handle_phase_complete(state, db, config).await {
        Ok(renamed) => {
            if let Some(new_name) = renamed {
                // Phase 2 renamed the intelligence (identity → config sync,
                // already persisted). Adopt it in the loop's working copy so
                // the later phases' prompts, the name the seal stamps and the
                // seal-time ModeChange carry it, tell the operator, and
                // refresh the console name mid-learning (the console parses
                // the `Name:` token on any ModeTransition).
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
            PhaseEnd::Advanced
        }
        Err(e) => {
            // A seal that another console wrote first is refused here as a
            // conflict: that is a seal, not a failure.
            if behind_a_seal && learning::is_soul_sealed(db).await.unwrap_or(false) {
                return PhaseEnd::SealedElsewhere;
            }
            error!("Phase complete handling failed: {}", e);
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!(
                            "Error processing phase: {e}\nThe phase stays open and nothing was advanced. \
                             Send a message to try again."
                        ),
                        msg_type: SystemMessageType::Error as i32,
                    }
                )),
            })).await;
            PhaseEnd::Failed
        }
    }
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
                // A provider's last event: the answer is whole. Reading on
                // waited for the stream to close with the stop still armed,
                // and a stop in that window sent a second `Done`.
                break;
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

#[cfg(test)]
mod loop_tests {
    //! The learning loop itself, between a provider and a database that are
    //! mocks: what it asks the model, what it writes, what it tells the
    //! client. The provider is the LM Studio preset pointed at a mock that
    //! answers a stream; the database answers 404 to what a test did not
    //! mount, which is "no such document" and "no seal".
    use super::*;
    use crate::config::tests_support::minimal_cfg;
    use embra_common::proto::brain::conversation_response::ResponseType;
    use serde_json::json;
    use std::time::Duration;
    use tokio_stream::wrappers::ReceiverStream;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A model's answer as the OpenAI-compatible stream carries it.
    fn answer(text: &str) -> ResponseTemplate {
        let chunk = |delta: serde_json::Value, finish: Option<&str>| {
            let frame = json!({
                "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "m",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            });
            format!("data: {frame}\n\n")
        };
        let body = format!("{}{}data: [DONE]\n\n", chunk(json!({"content": text}), None), chunk(json!({}), Some("stop")));
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    /// An answer that calls the phase complete with `document`.
    fn completes_with(document: &str) -> ResponseTemplate {
        answer(&format!("Noted.\n```json\n{document}\n```\n[PHASE_COMPLETE]"))
    }

    fn stored(doc: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": doc, "meta": {}}))
    }

    /// One console in Learning Mode: what it sends, what it is sent, and
    /// the loop's task.
    struct Console {
        say: mpsc::Sender<Result<ConversationRequest, Status>>,
        frames: mpsc::Receiver<Result<ConversationResponse, Status>>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Console {
        fn attach(provider: &MockServer, db: &MockServer) -> Self {
            let mut config = minimal_cfg();
            config.api_provider = "lm_studio".into();
            config.openai_compat.lm_studio_endpoint = provider.uri();
            config.openai_compat.lm_studio_model = "scripted".into();
            let db = Arc::new(WardsonDbClient::from_url(&db.uri()));
            let (say, said) = mpsc::channel(8);
            let (tx, frames) = mpsc::channel(256);
            let task = tokio::spawn(async move {
                let mut incoming = ReceiverStream::new(said);
                let in_turn = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (stop_tx, stop_rx) = watch::channel(0u64);
                let pending = Arc::new(Mutex::new(None));
                let commands = SetupCommands::new(&tx, &pending);
                let stage = Arc::new(watch::channel(OperatingMode::Learning as i32).0);
                let ended = run_learning_loop(
                    &mut incoming,
                    &db,
                    &config,
                    "k",
                    &stage,
                    TurnStop { in_turn: &in_turn, stop_rx: &stop_rx },
                    &commands,
                )
                .await;
                drop(stop_tx);
                ended
            });
            Console { say, frames, task }
        }

        async fn message(&self, text: &str) {
            let request = ConversationRequest {
                request_type: Some(conversation_request::RequestType::UserMessage(UserMessage {
                    content: text.to_string(),
                    ..Default::default()
                })),
            };
            self.say.send(Ok(request)).await.expect("the loop reads its console");
        }

        /// The frames up to and including the first one `wanted` accepts.
        async fn until(&mut self, what: &str, wanted: impl Fn(&ResponseType) -> bool) -> Vec<ResponseType> {
            let mut seen = Vec::new();
            loop {
                let frame = tokio::time::timeout(Duration::from_secs(10), self.frames.recv())
                    .await
                    .unwrap_or_else(|_| panic!("no {what} within 10 s; the frames so far: {seen:?}"))
                    .unwrap_or_else(|| panic!("the stream ended before {what}; the frames so far: {seen:?}"))
                    .expect("a frame")
                    .response_type
                    .expect("a typed frame");
                let found = wanted(&frame);
                seen.push(frame);
                if found {
                    return seen;
                }
            }
        }

        /// The console goes away; how the loop ended.
        async fn detach(self) -> anyhow::Result<()> {
            drop(self.say);
            tokio::time::timeout(Duration::from_secs(10), self.task).await.expect("the loop ends").expect("no panic")
        }
    }

    fn system(frame: &ResponseType, kind: SystemMessageType, part: &str) -> bool {
        matches!(frame, ResponseType::System(m) if m.msg_type == kind as i32 && m.content.contains(part))
    }

    async fn asked(provider: &MockServer) -> Vec<serde_json::Value> {
        provider
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON request"))
            .collect()
    }

    /// The loop of 2026-11-14. A completion that failed fell back into the
    /// loop, which sent the kickoff and asked the model again at once; a
    /// provider that calls the phase complete each time was asked 3,742
    /// times in forty seconds. The failure is said to the operator, nothing
    /// is asked until the operator writes, and the message that follows is
    /// one turn in the same phase.
    #[tokio::test]
    async fn a_phase_that_cannot_be_completed_waits_for_the_operator_and_asks_the_model_nothing() {
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completes_with(r#"{"name": "Operator"}"#))
            .mount(&provider)
            .await;
        let db = MockServer::start().await;
        Mock::given(method("GET")).and(path("/memory.user")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        Mock::given(method("POST"))
            .and(path("/memory.user/docs"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .mount(&db)
            .await;
        let mut console = Console::attach(&provider, &db);

        let frames = console.until("the failure", |f| system(f, SystemMessageType::Error, "Error processing phase")).await;
        let ResponseType::System(said) = frames.last().unwrap() else { unreachable!() };
        assert!(said.content.contains("503") && said.content.contains("Send a message to try again"), "{}", said.content);
        assert!(
            !frames.iter().any(|f| system(f, SystemMessageType::Info, "advancing to")),
            "a phase that did not advance is not announced as one that did: {frames:?}"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(asked(&provider).await.len(), 1, "the kickoff's turn and no other");

        console.message("try again").await;
        console.until("the second failure", |f| system(f, SystemMessageType::Error, "Error processing phase")).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let requests = asked(&provider).await;
        assert_eq!(requests.len(), 2, "one more turn, for the operator's message");
        // The same phase's conversation: the kickoff once, then the answer
        // and the operator's message.
        let texts: Vec<&str> = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert_eq!(texts, [learning::phase_kickoff(&learning::LearningPhase::UserConfiguration).as_str(), "try again"]);

        console.detach().await.expect("a console that goes away ends the loop without an error");
    }

    /// Two consoles finish the first phase together: the web console keeps
    /// a console of its own attached beside a chat client. The second one's
    /// profile is refused by the store as a conflict; it adopts the stored
    /// one and goes on to the next phase, with no error and nothing
    /// written over.
    #[tokio::test]
    async fn a_console_that_finishes_a_phase_second_adopts_the_first_ones_document() {
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completes_with(r#"{"name": "Second Console"}"#))
            .up_to_n_times(1)
            .mount(&provider)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(answer("What would you like me to be called?"))
            .mount(&provider)
            .await;
        let db = MockServer::start().await;
        Mock::given(method("GET")).and(path("/memory.user")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        Mock::given(method("POST"))
            .and(path("/memory.user/docs"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": {"code": "DOCUMENT_CONFLICT", "message": "Document conflict: Document already exists: user"},
                "meta": {}, "ok": false,
            })))
            .expect(1)
            .mount(&db)
            .await;
        // Absent when the loop starts (the first console has not written it
        // yet), there when this console's own write is refused.
        Mock::given(method("GET"))
            .and(path("/memory.user/docs/user"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .mount(&db)
            .await;
        Mock::given(method("GET"))
            .and(path("/memory.user/docs/user"))
            .respond_with(stored(json!({"_id": "user", "name": "First Console"})))
            .mount(&db)
            .await;
        Mock::given(method("PUT")).and(path("/memory.user/docs/user")).respond_with(stored(json!({}))).expect(0).mount(&db).await;
        let mut console = Console::attach(&provider, &db);

        let frames = console
            .until("the next phase's first answer", |f| matches!(f, ResponseType::Done(d) if d.full_response.contains("called")))
            .await;
        assert!(frames.iter().any(|f| system(f, SystemMessageType::Info, "advancing to: Identity Formation")), "{frames:?}");
        assert!(
            !frames.iter().any(|f| matches!(f, ResponseType::System(m) if m.msg_type == SystemMessageType::Error as i32)),
            "a document another console stored is no failure: {frames:?}"
        );
        // The next phase is built on the profile that is stored.
        let requests = asked(&provider).await;
        assert_eq!(requests.len(), 2);
        let prompt = requests[1]["messages"][0]["content"].as_str().unwrap();
        assert!(prompt.contains("First Console") && !prompt.contains("Second Console"), "{prompt}");

        console.detach().await.unwrap();
        db.verify().await;
    }

    /// Another console seals the soul while this one is in a turn of a
    /// phase before the seal. The answer that calls the phase complete
    /// writes nothing; this console announces the sealed soul and hands
    /// off, as it does when it is woken while it waits.
    #[tokio::test]
    async fn a_console_behind_another_ones_seal_writes_nothing_and_hands_off() {
        let provider = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completes_with(r#"{"name": "Operator"}"#))
            .mount(&provider)
            .await;
        let db = MockServer::start().await;
        // Unsealed at the loop's two reads before the turn, sealed after it.
        Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(404)).up_to_n_times(2).mount(&db).await;
        Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        // The profile's write would go through, were it made.
        Mock::given(method("GET")).and(path("/memory.user")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        Mock::given(method("POST")).and(path("/memory.user/docs")).respond_with(stored(json!({"_id": "user"}))).expect(0).mount(&db).await;
        let mut console = Console::attach(&provider, &db);

        let frames = console
            .until("the hand-off", |f| matches!(f, ResponseType::ModeChange(m) if m.to_mode == OperatingMode::Operational as i32))
            .await;
        assert!(
            !frames.iter().any(|f| matches!(f, ResponseType::System(m) if m.msg_type == SystemMessageType::Error as i32)),
            "{frames:?}"
        );
        tokio::time::timeout(Duration::from_secs(10), console.task).await.expect("the loop ends").unwrap().unwrap();
        assert_eq!(asked(&provider).await.len(), 1);
        db.verify().await;
    }

    /// A phase ends in the conversation far more often than on its kickoff,
    /// and the name the identity phase gives was taken up only on the
    /// kickoff's path: the phases after it kept the wizard's name, and the
    /// seal stamped it on the sealed graph beside a configuration that
    /// carried the new one. The name is taken up wherever the phase ends.
    #[tokio::test]
    async fn a_name_the_identity_phase_gives_in_conversation_is_the_later_phases_name() {
        let provider = MockServer::start().await;
        let turn = |template: ResponseTemplate| {
            Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(template).up_to_n_times(1)
        };
        turn(answer("What would you like me to be called?")).mount(&provider).await;
        turn(completes_with(r#"{"name": "Aster"}"#)).mount(&provider).await;
        turn(answer("Now the values.")).mount(&provider).await;
        let db = MockServer::start().await;
        // The first phase is done: the loop starts at the identity.
        Mock::given(method("GET")).and(path("/memory.user/docs/user")).respond_with(stored(json!({"_id": "user", "name": "Operator"}))).mount(&db).await;
        Mock::given(method("GET")).and(path("/memory.identity")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        Mock::given(method("POST")).and(path("/memory.identity/docs")).respond_with(stored(json!({"_id": "identity"}))).expect(1).mount(&db).await;
        let mut config_doc = serde_json::to_value(minimal_cfg()).unwrap();
        config_doc["_id"] = json!("config");
        Mock::given(method("GET")).and(path("/config.system")).respond_with(ResponseTemplate::new(200)).mount(&db).await;
        Mock::given(method("GET")).and(path("/config.system/docs/config")).respond_with(stored(config_doc)).mount(&db).await;
        Mock::given(method("POST")).and(path("/config.system/docs")).respond_with(stored(json!({"_id": "config"}))).mount(&db).await;
        let mut console = Console::attach(&provider, &db);

        console.until("the identity phase's question", |f| matches!(f, ResponseType::Done(d) if d.full_response.contains("called"))).await;
        console.message("Aster").await;
        let frames = console
            .until("the next phase's first answer", |f| matches!(f, ResponseType::Done(d) if d.full_response.contains("values")))
            .await;
        assert!(
            frames.iter().any(|f| system(f, SystemMessageType::Info, "Name updated: Embra → Aster (synced from the Phase-2 identity")),
            "{frames:?}"
        );
        assert!(
            frames.iter().any(|f| matches!(f, ResponseType::ModeChange(m)
                if m.to_mode == OperatingMode::Learning as i32 && m.message.contains("Name: Aster") && m.message.contains("Phase: Soul Definition"))),
            "{frames:?}"
        );
        let requests = asked(&provider).await;
        assert_eq!(requests.len(), 3);
        let prompt = requests[2]["messages"][0]["content"].as_str().unwrap();
        assert!(prompt.contains("Aster"), "the soul phase's prompt carries the new name: {prompt}");

        console.detach().await.unwrap();
        db.verify().await;
    }
}
