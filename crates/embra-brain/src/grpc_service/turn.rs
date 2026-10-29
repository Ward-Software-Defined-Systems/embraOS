//! The turn: `handle_request`, the operator's message through the tool loop to the persisted turn, the attach, the trigger and the delete flow it dispatches.

use super::*;

/// Handle a single incoming conversation request.
///
/// A caller that dispatches a message of its own making (the resume
/// briefing, the delete flow) sets `active_session` first or attaches a
/// session: a user message without one is answered with the no-session
/// notice, and the model is not called.
pub(super) async fn handle_request(
    req: ConversationRequest,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    ctx: &ServiceCtx,
    config_tz: &str,
    api_key: &str,
    stop_rx: &watch::Receiver<u64>,
) -> anyhow::Result<()> {
    let ServiceCtx {
        db,
        session_mgr,
        in_turn,
        pending_provider,
        pending_key_setup,
        pending_openai_compat_setup,
        pending_session_delete,
    } = ctx;
    let self_in_turn = in_turn.clone();
    match req.request_type {
        Some(conversation_request::RequestType::UserMessage(msg)) => {
            // D2: intercept the user message as a candidate API key
            // when /provider --setup queued a target. Validates,
            // persists per-provider key + STATE file, clears flag.
            // On invalid: keeps flag set so the operator can retry
            // with the next message.
            // Sprint 5 follow-up: intercept user messages while the
            // OpenAI-compat reconfigure state machine is active.
            // Each message advances one step; on completion or error
            // the state is cleared.
            {
                let mut state_opt = pending_openai_compat_setup.lock().await.take();
                if let Some(mut state) = state_opt.take() {
                    let outcome = handle_pending_openai_compat_step(
                        &mut state,
                        &msg.content,
                        tx,
                        db,
                        pending_openai_compat_setup,
                    )
                    .await;
                    if matches!(outcome, SetupStepOutcome::Advance) {
                        *pending_openai_compat_setup.lock().await = Some(state);
                    } else if matches!(outcome, SetupStepOutcome::Complete)
                        && state.model_only
                        && let Ok(cfg) = config::load_config(db).await
                    {
                        let brain = display_model_for(&cfg.api_provider, &cfg);
                        send_brain_status(tx, &cfg, session_mgr, &brain).await;
                    }
                    // Complete and Error already cleared the Mutex
                    // (or didn't reinstate it). Either way the flow ends
                    // here without invoking the regular conversation path.
                    return Ok(());
                }
            }

            let pending_step = *pending_key_setup.lock().await;
            match pending_step {
                Some(KeySetupStep::Key(target)) => {
                    let candidate = msg.content.trim().to_string();
                    handle_pending_key_setup(target, candidate, tx, db, pending_key_setup).await;
                    return Ok(());
                }
                Some(KeySetupStep::Model(target)) => {
                    handle_setup_model_pick(target, &msg.content, tx, db, session_mgr, pending_key_setup).await;
                    return Ok(());
                }
                None => {}
            }

            // Guided /sessions delete flow. AwaitingKickoff: this message
            // IS the kickoff synthetic the slash arm just dispatched —
            // advance in place and let it run as a normal turn (consuming
            // it here would eat the kickoff as the operator's reason).
            // AwaitingReason: this message is the operator's reason — take
            // the state; the enrichment step below swaps in the memorize
            // prompt and the post-persistence step executes the deletion
            // on the model's READY marker. Not an early-return intercept:
            // the reason must reach the model.
            let delete_exec: Option<PendingSessionDelete> = {
                let mut guard = pending_session_delete.lock().await;
                advance_delete_flow(&mut guard)
            };

            // No-session guard (session-ux-fixes). The historical fallback
            // ran the turn against a literal "default" session: normally
            // nonexistent, so the model answered with empty history and
            // both append_message calls silently no-opped — a ghost turn.
            // Blocking here (after the sessionless setup intercepts above,
            // which legitimately consume messages post-/close) also closes
            // the cross-provider hole: this path never runs
            // check_session_provider, so a real "default" session pinned
            // to another provider would have been silently contaminated.
            let session_name = {
                let mgr = session_mgr.read().await;
                match mgr.active_session.clone() {
                    Some(name) => name,
                    None => {
                        let most_recent = mgr
                            .get_most_recent_active()
                            .await
                            .ok()
                            .flatten()
                            .map(|m| m.name);
                        drop(mgr);
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: no_session_block_message(most_recent.as_deref()),
                                    msg_type: SystemMessageType::Error as i32,
                                }
                            )),
                        })).await;
                        return Ok(());
                    }
                }
            };

            // Load config — api_key comes from --api-key flag or env, not WardSONDB
            let loaded_config = config::load_config(db).await.unwrap_or_else(|_| config::SystemConfig::boot_fallback(api_key.to_string(), config_tz.to_string()));
            let config_name = loaded_config.name.clone();

            // Provider-aware credential precheck. For Anthropic/Gemini,
            // require a key (either boot-time --api-key or the resolved
            // per-provider field). For OpenAI-compat presets, no api_key
            // is expected — endpoint+model presence is the relevant
            // configuration check, handled per-call in
            // build_openai_compat_provider.
            let active_kind = ProviderKind::from_str(&loaded_config.api_provider)
                .unwrap_or(ProviderKind::Anthropic);
            let needs_api_key = matches!(
                active_kind,
                ProviderKind::Anthropic | ProviderKind::Gemini
            );
            if needs_api_key
                && api_key.is_empty()
                && loaded_config.key_for(active_kind).is_none()
            {
                let provider_label = active_kind.label();
                let env_var = match active_kind {
                    ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                    ProviderKind::Gemini => "GEMINI_API_KEY",
                    _ => unreachable!(),
                };
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!(
                                "No {provider_label} API key configured. Set {env_var}, run /provider --setup {}, or re-run the wizard.",
                                active_kind.as_str()
                            ),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
                return Ok(());
            }

            // Build system prompt
            let system_prompt = if learning::is_soul_sealed(db).await.unwrap_or(false) {
                // Operational mode — build full system prompt
                // Pass the soul as a Value — operational_mode renders it
                // into the embraOS constitution via brain::render_constitution.
                // The stored doc is read-only here; never re-serialized into
                // storage, so the SHA-256 seal and trustd verification are
                // untouched. The seal was just read, so the load probes the
                // collection no second time (#38).
                let soul = learning::load_sealed_soul(db)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(serde_json::Value::Null);
                // Pass the profile as a Value — operational_mode renders
                // it via brain::render_user_profile. Read-only; the profile
                // is not sealed, so no hash/trustd interaction.
                let user_profile = db
                    .read("memory.user", "user")
                    .await
                    .ok()
                    .unwrap_or(serde_json::Value::Null);
                // Part of the system prompt, which is the same on every
                // turn of a session: what goes in here is fixed for the
                // session. A clock, a turn count or anything else that
                // moves per turn belongs in the user message
                // (`knowledge::enrichment::build_turn_context`).
                let session_context = format!("Session: {}, Timezone: {}", session_name, config_tz);
                if crate::identity_graph::is_graph_soul(&soul) {
                    // Graph mode (kg-native-identity): identity lives IN
                    // the sealed graph — no memory.identity read (imported
                    // instances never write one). Renders from the sealed
                    // doc → byte-stable per seal, cache-warm.
                    crate::brain::operational_mode_graph(
                        &config_name, &soul, &user_profile, &session_context,
                    )
                } else {
                    // Legacy flat-soul path — byte-for-byte unchanged
                    // (legacy_prompt_golden_tests). Pass identity as a
                    // Value — operational_mode renders it into the embraOS
                    // character portrait via brain::render_identity.
                    // Read-only; identity is not sealed, so there is no
                    // hash/trustd interaction.
                    let identity = db
                        .read("memory.identity", "identity")
                        .await
                        .ok()
                        .unwrap_or(serde_json::Value::Null);
                    crate::brain::operational_mode(
                        &config_name, &soul, &identity, &user_profile, &session_context,
                    )
                }
            } else {
                // Learning mode — use phase-specific prompt
                "You are in Learning Mode. The system prompt will be set once the soul is defined.".to_string()
            };

            // Construct the right provider per-turn based on the
            // persisted config. Stage 8's /provider switch updates
            // config.system.api_provider AND mirrors the target's
            // per-provider key into loaded_config.api_key — so we
            // resolve the active key from the persisted config
            // rather than the brain-startup parameter (which holds
            // whatever key was on STATE at boot, NOT the post-swap
            // key). key_for falls back to legacy api_key when the
            // active provider matches, which is the post-swap case.
            // Provider construction is shared with the guardian replicant
            // check via `build_provider_from_config`. The per-turn loop
            // wires Gemini's context cache (Some(db)) and keeps the
            // per-turn "gemini turn starting" diagnostic; an unconfigured
            // OpenAI-compat preset surfaces as a SystemMessage as before.
            let provider_kind = ProviderKind::from_str(&loaded_config.api_provider)
                .unwrap_or(ProviderKind::Anthropic);
            if provider_kind == ProviderKind::Gemini {
                let model_id = resolve_gemini_model_id(&loaded_config);
                info!(
                    target: "gemini::diag",
                    model = %model_id,
                    explicit_cache = crate::provider::gemini::explicit_cache_supported(&model_id),
                    session = %session_name,
                    "gemini turn starting"
                );
            }
            let provider: Arc<dyn LlmProvider> =
                match build_provider_from_config(&loaded_config, api_key, Some(db.clone())) {
                    Ok(p) => p,
                    Err(msg) => {
                        let _ = tx
                            .send(Ok(ConversationResponse {
                                response_type: Some(
                                    conversation_response::ResponseType::System(SystemMessage {
                                        content: msg,
                                        msg_type: SystemMessageType::Error as i32,
                                    }),
                                ),
                            }))
                            .await;
                        return Ok(());
                    }
                };
            // The static registry and nothing else. A dynamic Guardian tool
            // is reached through the `guardian_*` meta-tools and is never
            // added to this list: what the model is shown stays the same
            // from turn to turn and from instance to instance.
            let descriptors: Vec<&'static tools::registry::ToolDescriptor> =
                tools::registry::all_descriptors().collect();
            let tool_manifest: ToolManifest = provider.build_tool_manifest(&descriptors);
            let system_bundle = SystemPromptBundle {
                text: system_prompt,
                session_name: session_name.clone(),
            };

            // Set the in-flight flag so /provider commands queue
            // instead of swapping mid-turn. Cleared in a drop guard so
            // the flag never sticks if this future is cancelled.
            // Per-turn operator-stop observer: snapshots the generation
            // now, so only a StopTurn fired DURING this turn trips it.
            let (in_turn_guard, mut stop_check) = begin_turn(&self_in_turn, stop_rx, &session_name);

            // Load session history. A history that cannot be read is not an
            // empty one: the turn would run without its context and its
            // appends would graft onto the real document (#59).
            let history = {
                let mgr = session_mgr.read().await;
                match mgr.load_history(&session_name).await {
                    Ok(h) => h,
                    Err(e) => {
                        drop(mgr);
                        send_system(
                            tx,
                            format!(
                                "Could not load the history of session '{}': {}. Nothing was sent to the model; try again.",
                                session_name, e
                            ),
                            SystemMessageType::Error,
                        )
                        .await;
                        return Ok(());
                    }
                }
            };

            // Resume-briefing branch: SessionAttach sets this flag for
            // the next user turn (existing session with prior history,
            // Operational mode). Read-and-clear, then substitute the
            // `<session_resumption>` wrapper for the brain-facing call.
            // Raw `msg.content` (`[Session resumed]`) is what the
            // persistence step at the end of this handler writes to
            // history, mirroring the auto-enrichment pattern below.
            let pending_briefing = session_mgr.write().await.pending_resume_briefing.take();
            // A trigger turn (a reminder or job with act=true fired) carries
            // the same kind of marker: synthetic for the attachments, an
            // operator turn for the events block.
            let pending_trigger = session_mgr.write().await.pending_trigger.take();
            // Where the next events block starts: this turn's start, stamped
            // on the session once the turn is saved (`sessions::events`).
            let turn_started = chrono::Utc::now();

            // Auto-KG-enrichment: wrap the user message in a <retrieved_context>
            // block when the knowledge graph has relevant prior knowledge. The
            // system prompt is left untouched so Anthropic prompt caching stays
            // warm. History persistence below saves `msg.content` (raw), so the
            // wrapper never leaks into subsequent turns.
            // Media on this turn: the client's explicit `attachment_ids` ∪
            // whatever `/attach` staged for the session. Synthetic turns
            // (resume briefing, delete-flow memorize) never consume the
            // staging. An unknown id aborts the turn with an Error frame
            // and persists nothing; the staging is drained only on success.
            let media_store = media::MediaStore::default_store();
            let synthetic_turn = pending_briefing.is_some() || delete_exec.is_some() || pending_trigger.is_some();
            // The briefing carries <while_away> and the delete flow nothing;
            // every other turn, a trigger turn included, carries the events.
            let carries_events = pending_briefing.is_none() && delete_exec.is_none();
            let mut turn_media: Vec<media::MediaMeta> = Vec::new();
            // Text files on this turn, each with its text: the paths the
            // client named (jailed to the workspace) ∪ those `/attach
            // <path>` staged, read now and gated as text. One that cannot
            // be read aborts the turn like an unknown image id.
            let mut turn_files: Vec<(media::text::TextUpload, String)> = Vec::new();
            if !synthetic_turn {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for id in &msg.attachment_ids {
                    match media_store.meta(id).await {
                        Ok(meta) => {
                            if seen.insert(meta.id.clone()) {
                                turn_media.push(meta);
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: format!("Attachment '{}' is not available: {}. Nothing was sent.", id, e),
                                        msg_type: SystemMessageType::Error as i32,
                                    }
                                )),
                            })).await;
                            return Ok(());
                        }
                    }
                }
                let staged = session_mgr.read().await.staged_media(&session_name).to_vec();
                for meta in staged {
                    if seen.insert(meta.id.clone()) {
                        turn_media.push(meta);
                    }
                }
                let mut seen_paths: std::collections::HashSet<std::path::PathBuf> =
                    std::collections::HashSet::new();
                let staged_files = session_mgr.read().await.staged_files(&session_name).to_vec();
                let named: Vec<(String, Result<std::path::PathBuf, String>)> = msg
                    .file_paths
                    .iter()
                    .map(|p| (p.clone(), media::text::wire_upload_path(p)))
                    .chain(staged_files.into_iter().map(|u| (u.path.display().to_string(), Ok(u.path))))
                    .collect();
                for (shown, resolved) in named {
                    let outcome = match resolved {
                        Ok(path) => {
                            if !seen_paths.insert(path.clone()) {
                                continue;
                            }
                            media::text::read_text_file(&path).await.map_err(|e| e.to_string())
                        }
                        Err(e) => Err(e),
                    };
                    match outcome {
                        Ok(read) => turn_files.push(read),
                        Err(e) => {
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: format!("Attached file '{}' is not available: {}. Nothing was sent.", shown, e),
                                        msg_type: SystemMessageType::Error as i32,
                                    }
                                )),
                            })).await;
                            return Ok(());
                        }
                    }
                }
                // One limit for everything the message carries: images and
                // text files, named on the message or staged with `/attach`.
                if turn_media.len() + turn_files.len() > media::MEDIA_MAX_PER_MESSAGE {
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "{} attachments on one message; the limit is {}. Use `/attach clear` and send fewer. Nothing was sent.",
                                    turn_media.len() + turn_files.len(),
                                    media::MEDIA_MAX_PER_MESSAGE
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            }
                        )),
                    })).await;
                    return Ok(());
                }
                if !turn_media.is_empty() {
                    let _ = session_mgr.write().await.take_staged_media(&session_name);
                }
                if !turn_files.is_empty() {
                    let _ = session_mgr.write().await.take_staged_files(&session_name);
                }
            }
            let mut turn_images: Vec<crate::provider::ir::ImageData> = Vec::with_capacity(turn_media.len());
            let mut turn_refs: Vec<crate::brain::AttachmentRef> = Vec::with_capacity(turn_media.len());
            for meta in &turn_media {
                match media_store.get(&meta.id).await {
                    Ok((m, data)) => {
                        turn_images.push(media::store::to_image_data(&m, &data));
                        turn_refs.push(media::store::to_attachment_ref(&m, media_store.dir()));
                    }
                    Err(e) => {
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!("Attachment '{}' could not be read: {}. Nothing was sent.", meta.id, e),
                                    msg_type: SystemMessageType::Error as i32,
                                }
                            )),
                        })).await;
                        return Ok(());
                    }
                }
            }
            if !turn_images.is_empty() {
                let dims: Vec<String> = turn_images
                    .iter()
                    .map(|i| format!("{}×{}", i.width, i.height))
                    .collect();
                info!(
                    target: "media",
                    session = %session_name,
                    images = turn_images.len(),
                    bytes = turn_refs.iter().map(|r| r.bytes).sum::<u64>(),
                    dims = %dims.join(","),
                    "attachments on user turn"
                );
            }
            for (u, _) in &turn_files {
                turn_refs.push(media::text::to_attachment_ref(u));
            }
            if !turn_files.is_empty() {
                info!(
                    target: "media",
                    session = %session_name,
                    files = turn_files.len(),
                    bytes = turn_files.iter().map(|(u, _)| u.bytes).sum::<u64>(),
                    "text attachments on user turn"
                );
            }

            let enriched = if let Some(ref del) = delete_exec {
                // Delete-flow reason turn: the brain-facing message is the
                // memorize prompt; the raw persisted content stays the
                // operator's reason (same swap contract as the briefing).
                build_delete_memorize_prompt(&del.name, &msg.content)
            } else if let Some(briefing) = pending_briefing {
                // What happened while the operator was away, when the
                // absence is known and anything happened in it.
                let away = match briefing.away_since {
                    Some(since) => {
                        let metas = session_mgr.read().await.list().await.unwrap_or_default();
                        let digest = crate::sessions::away::gather(
                            db.as_ref(),
                            &metas,
                            since,
                            chrono::Utc::now(),
                            &session_name,
                        )
                        .await;
                        crate::sessions::away::render(&digest, since, &loaded_config.timezone)
                    }
                    None => None,
                };
                crate::knowledge::enrichment::build_resumption_context(away.as_deref())
            } else if let Some(trigger) = pending_trigger {
                // A trigger turn: the fire, wrapped with what to do with it.
                crate::knowledge::enrichment::build_trigger_context(trigger.kind, &trigger.headline, &trigger.body)
            } else if msg.content.trim().is_empty() && !turn_images.is_empty() {
                // Image-only message: every provider path assumes a text
                // block exists (cache breakpoints, parts arrays), and the
                // enrichment gate would skip an empty message anyway.
                media::replay::IMAGE_ONLY_PLACEHOLDER.to_string()
            } else if msg.content.trim().is_empty() && !turn_files.is_empty() {
                // File-only message, for the same reasons.
                media::text::FILE_ONLY_PLACEHOLDER.to_string()
            } else {
                crate::knowledge::enrichment::build_turn_context(
                    db.as_ref(),
                    &msg.content,
                    &session_name,
                    &history,
                    &loaded_config,
                )
                .await
            };

            // Build neutral-IR message history. Persistence is role+String
            // (+ optional attachment refs): text maps to `Block::Text`,
            // user-turn attachments reload from the MEDIA store under the
            // inline-replay ceiling (`media::replay`). Thinking signatures
            // were never persisted so cross-turn replay is out of scope.
            // Within a single turn, the loop pushes the assistant turn's
            // full content (including any ProviderOpaque thinking blocks)
            // verbatim between iterations — the API requires this.
            // The image budget is the turn's provider's: Gemini caps a
            // request at 20 MB, Anthropic at 32 MB.
            let mut api_messages: Vec<ApiMessage> = media::replay::history_to_api(
                &media_store,
                &history,
                media::history_image_bytes_for(provider_kind),
            )
            .await;
            // Attached files ride the model-facing text ahead of the
            // operator's words, inside the turn's one text block. What is
            // persisted stays `msg.content` plus the refs.
            let files_block = media::text::render_attached_files(&turn_files);
            // What happened since this session's last turn — fired
            // reminders, cron runs — ahead of the files and the words,
            // rendered once and never persisted. A briefing carries
            // <while_away> instead, and the delete flow carries nothing.
            let events_block = if !carries_events {
                None
            } else {
                let cutoff = session_mgr
                    .read()
                    .await
                    .get_meta(&session_name)
                    .await
                    .ok()
                    .flatten()
                    .map(|m| crate::sessions::events::cutoff(&m));
                match cutoff {
                    Some(since) => {
                        let digest = crate::sessions::events::gather(db.as_ref(), since).await;
                        crate::sessions::events::render(&digest, since, &loaded_config.timezone)
                    }
                    None => None,
                }
            };
            let mut parts: Vec<String> = Vec::new();
            if let Some(events) = events_block {
                parts.push(events);
            }
            if !files_block.is_empty() {
                parts.push(files_block);
            }
            parts.push(enriched);
            let model_text = parts.join("\n");
            api_messages.push(if turn_images.is_empty() {
                ApiMessage::user_text(&model_text)
            } else {
                // Images first, then the files and the (possibly enriched) text.
                ApiMessage::user_with_images(turn_images, &model_text)
            });
            // A turn the operator stopped before any text is persisted as
            // a marker. The model reads it as a note inside the operator's
            // next message, never as a reply of its own
            // (`sessions::model_view`).
            let mut api_messages = crate::sessions::model_view::fold_stopped_turns(api_messages);

            // Send thinking indicator.
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Thinking(
                    ThinkingState { is_thinking: true, name: config_name.clone(), current_tool: String::new() }
                )),
            })).await;

            // Native tool-use loop driven by TurnOutcome. Cap default sized
            // for multi-file code-review traversals (Embra_Debug #80) — at
            // 10 the model regularly truncated mid-loop on real review
            // sessions. Operators can override per-install via `/iter-cap`
            // (persisted in SystemConfig). Clamp(1, 1000) is defense-in-
            // depth against hand-edited bogus values.
            let max_tool_iterations: usize = loaded_config
                .max_tool_iterations
                .unwrap_or(DEFAULT_MAX_TOOL_ITERATIONS)
                .clamp(1, 1000);
            let mut tool_iter: usize = 0;
            // Assigned from the first response below, before any read.
            // The text of the LAST iteration only, and kept apart from the
            // accumulator below on purpose: the operator-stop `Done` frame,
            // the Gemini telemetry and `delete_flow_verdict` read it, and a
            // `[SESSION_DELETE_READY]` the model wrote in an earlier
            // iteration must never delete a session.
            let mut last_response_text: String;
            // Transcript accumulator — every iteration's operator-visible
            // text, not just the last (multi-tool turns previously
            // persisted only the final segment; narration the operator
            // watched live never reached history).
            let mut persisted_segments = TranscriptSegments::default();
            // Set when a continuation's stream ended without a result (the
            // server answered with an error, or the connection went): the
            // turn ends like a stop, with a Done frame, and persists what
            // it has plus the marker.
            let mut stream_failed = false;
            // Text the operator saw stream in as tokens that no `Done` closed:
            // the partial of a stop that cut the stream, or a synthetic token
            // (the silent-end diagnostic, the cap fallbacks). The turn's
            // closing `Done` carries it, so the clients turn their live line
            // into a row. `None` after every `Complete`, whose `Done` already
            // went out — that text is never sent twice. Assigned from the
            // first response below, like `last_response_text`.
            let mut undelivered: Option<String>;
            // Embra_Debug #70: collect tool names dispatched across the
            // full user-turn loop (deduped, first-seen order). Used to
            // synthesize a placeholder when the assistant's last
            // iteration has no Text blocks — `(no response)` is the
            // historical fallback and is misleading when real work fired.
            let mut tool_names_in_turn: Vec<String> = Vec::new();
            // Images the turn's tools produced/viewed — persisted on the
            // assistant message for transcript + history-replay display
            // (never replayed to the model: assistant turns can't carry
            // image blocks).
            let mut turn_assistant_refs: Vec<crate::brain::AttachmentRef> = Vec::new();

            // Per-request turn trace. Interior mutability via Arc<Mutex>
            // avoids &mut propagation through the `fn` ToolDescriptor
            // handler signature. `turn_index` is the *logical* turn number
            // (history is per-role-message; one user + one assistant = one
            // turn, so `len() / 2` names the turn the model is now starting).
            // The `turn_trace` tool's read path computes `target = ctx.turn_index
            // - back` and matches `back=1` to the immediately prior turn's
            // persisted entries — that arithmetic only holds under logical
            // (not message-count) units.
            let trace_handle: embra_tools_core::TurnTraceHandle =
                embra_tools_core::new_turn_trace_handle();
            let turn_index: usize = history.len() / 2;

            let request_options = LlmRequestOptions {
                include_reasoning: loaded_config.show_reasoning(),
            };
            let first_stream = provider
                .stream_turn(
                    &api_messages,
                    &system_bundle,
                    &tool_manifest,
                    request_options,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;
            let Some(mut current_turn) =
                collect_response(first_stream, tx, &config_name, Some(&mut stop_check)).await?
            else {
                // Stream closed without Complete — treat as error and save
                // nothing. collect_response showed the error; the Done frame
                // ends the turn for the clients (the typing indicator, the
                // live text), as it does after an operator stop.
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Done(
                            StreamDone {
                                full_response: String::new(),
                            },
                        )),
                    }))
                    .await;
                drop(in_turn_guard);
                return Ok(());
            };
            // Track text from the most recent turn for persistence fallback.
            last_response_text = turn_text(&current_turn);
            persisted_segments.push(&last_response_text);
            undelivered = undelivered_after(&current_turn);
            api_messages.push(ApiMessage::assistant_blocks(current_turn.content.clone()));

            loop {
                // Operator stop observed between round-trips (after a
                // collect, or re-entering via `continue` after tool
                // dispatches): override the outcome so the terminal arm
                // runs and no further provider request is made.
                if stop_check.stopped()
                    && !matches!(
                        current_turn.outcome,
                        TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop)
                    )
                {
                    current_turn.outcome = TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop);
                }
                match current_turn.outcome {
                    TurnOutcome::EndTurn
                    | TurnOutcome::MaxTokens
                    | TurnOutcome::EarlyStop(_) => {
                        // #32 defense: detect empty-text terminal turns
                        // after side-effectful work and emit a diagnostic
                        // token so the user/model don't desync. The model
                        // legitimately ends turn silently on pure-read
                        // flows, so the guard requires at least one
                        // side-effectful success.
                        if !current_turn.has_text() && !current_turn.has_tool_call() {
                            let side_effectful_count = trace_handle
                                .lock()
                                .map(|g| {
                                    g.iter()
                                        .filter(|e| !e.is_error)
                                        .filter(|e| {
                                            tools::registry::REGISTRY
                                                .get(e.tool_name.as_str())
                                                .map(|d| d.is_side_effectful)
                                                .unwrap_or(false)
                                        })
                                        .count()
                                })
                                .unwrap_or(0);
                            if side_effectful_count > 0 {
                                let diagnostic = format!(
                                    "(model ended turn silently after {} side-effectful tool call{}; see `turn_trace` for details)",
                                    side_effectful_count,
                                    if side_effectful_count == 1 { "" } else { "s" },
                                );
                                let _ = tx
                                    .send(Ok(ConversationResponse {
                                        response_type: Some(
                                            conversation_response::ResponseType::Token(
                                                StreamToken {
                                                    text: diagnostic.clone(),
                                                },
                                            ),
                                        ),
                                    }))
                                    .await;
                                undelivered = Some(diagnostic.clone());
                                last_response_text = diagnostic;
                                persisted_segments.push(&last_response_text);
                                warn!(
                                    target: "dispatch",
                                    session = %session_name,
                                    side_effectful_count,
                                    "empty-text terminal turn after side-effectful tools"
                                );
                            }
                        }
                        break;
                    }

                    TurnOutcome::Pause => {
                        warn!(
                            target: "dispatch",
                            session = %session_name,
                            "stop_reason=pause_turn; resuming conversation"
                        );
                        let stream = provider
                            .stream_turn(
                                &api_messages,
                                &system_bundle,
                                &tool_manifest,
                                request_options,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("Brain pause-resume failed: {}", e))?;
                        let Some(resp) = collect_response(stream, tx, &config_name, Some(&mut stop_check)).await? else {
                            error!(
                                target: "dispatch",
                                session = %session_name,
                                "Brain pause-resume failed: the stream ended without a result"
                            );
                            persisted_segments.push(STREAM_FAILED_MARKER);
                            stream_failed = true;
                            break;
                        };
                        current_turn = resp;
                        last_response_text = turn_text(&current_turn);
                        persisted_segments.push(&last_response_text);
                        undelivered = undelivered_after(&current_turn);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_turn.content.clone(),
                        ));
                        continue;
                    }

                    TurnOutcome::ToolUse => {
                        if tool_iter >= max_tool_iterations {
                            // Embra_Debug #80: graceful cap termination.
                            // Silent break here used to leave undispatched
                            // ToolCall blocks dangling in `api_messages` and
                            // gave the model no chance to wrap up — the UI
                            // surfaced `(no response)` or the tool-name
                            // placeholder. Now we (1) surface a Warning
                            // frame, (2) synthesize is_error=true ToolResults
                            // for every undispatched call so the next request
                            // is wire-valid, (3) make one final stream_turn
                            // letting the model summarize, then break
                            // unconditionally regardless of outcome.
                            warn!(
                                target: "dispatch",
                                session = %session_name,
                                "tool iteration cap hit ({max_tool_iterations})"
                            );
                            let _ = tx
                                .send(Ok(ConversationResponse {
                                    response_type: Some(
                                        conversation_response::ResponseType::System(
                                            SystemMessage {
                                                content: format!(
                                                    "Tool iteration cap of {max_tool_iterations} reached — asking the model to summarize and stop."
                                                ),
                                                msg_type: SystemMessageType::Warning as i32,
                                            },
                                        ),
                                    ),
                                }))
                                .await;
                            let cap_msg = format!(
                                "System: tool iteration cap of {max_tool_iterations} reached. \
                                 No further tool calls will be dispatched in this turn. \
                                 Summarize your findings to the user now, or ask the operator for guidance."
                            );
                            let synthetic =
                                synthesize_cap_results(&current_turn.content, &cap_msg);
                            if !synthetic.is_empty() {
                                api_messages
                                    .push(ApiMessage::user_tool_results(synthetic));
                            }
                            match provider
                                .stream_turn(
                                    &api_messages,
                                    &system_bundle,
                                    &tool_manifest,
                                    request_options,
                                )
                                .await
                            {
                                Ok(stream) => {
                                    if let Some(final_turn) =
                                        collect_response(stream, tx, &config_name, Some(&mut stop_check)).await?
                                    {
                                        let final_text = turn_text(&final_turn);
                                        let stopped_in_stream = final_turn.outcome
                                            == TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop);
                                        api_messages.push(ApiMessage::assistant_blocks(
                                            final_turn.content,
                                        ));
                                        persisted_segments.push(&final_text);
                                        if !final_text.is_empty() {
                                            undelivered = stopped_in_stream.then(|| final_text.clone());
                                            last_response_text = final_text;
                                        } else {
                                            // Model produced no text on the final
                                            // summary turn (another ToolUse, or
                                            // pure thinking). Stream a synthetic
                                            // token so the UI doesn't go silent
                                            // after the Warning frame.
                                            let fallback = format!(
                                                "(tool iteration cap of {max_tool_iterations} reached; model did not produce a final summary)"
                                            );
                                            let _ = tx
                                                .send(Ok(ConversationResponse {
                                                    response_type: Some(
                                                        conversation_response::ResponseType::Token(
                                                            StreamToken {
                                                                text: fallback.clone(),
                                                            },
                                                        ),
                                                    ),
                                                }))
                                                .await;
                                            persisted_segments.push(&fallback);
                                            undelivered = Some(fallback.clone());
                                            last_response_text = fallback;
                                        }
                                    } else {
                                        let fallback = format!(
                                            "(tool iteration cap of {max_tool_iterations} reached; no final response received)"
                                        );
                                        let _ = tx
                                            .send(Ok(ConversationResponse {
                                                response_type: Some(
                                                    conversation_response::ResponseType::Token(
                                                        StreamToken {
                                                            text: fallback.clone(),
                                                        },
                                                    ),
                                                ),
                                            }))
                                            .await;
                                        // The fallback token was streamed
                                        // unconditionally above — record it
                                        // in the transcript unconditionally
                                        // too (the guarded single-slot
                                        // assignment predates the
                                        // accumulator).
                                        persisted_segments.push(&fallback);
                                        undelivered = Some(fallback.clone());
                                        if last_response_text.is_empty() {
                                            last_response_text = fallback;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        target: "dispatch",
                                        session = %session_name,
                                        error = %e,
                                        "final summary call failed after iteration cap"
                                    );
                                    let _ = tx
                                        .send(Ok(ConversationResponse {
                                            response_type: Some(
                                                conversation_response::ResponseType::System(
                                                    SystemMessage {
                                                        content: format!(
                                                            "Final summary call failed after iteration cap: {e}"
                                                        ),
                                                        msg_type: SystemMessageType::Error as i32,
                                                    },
                                                ),
                                            ),
                                        }))
                                        .await;
                                    let marker = format!(
                                        "(tool iteration cap of {max_tool_iterations} reached; final summary call errored)"
                                    );
                                    // Recorded next to the Error frame the
                                    // operator saw; unconditional — the
                                    // guarded assignment predates the
                                    // accumulator.
                                    persisted_segments.push(&marker);
                                    if last_response_text.is_empty() {
                                        last_response_text = marker;
                                    }
                                }
                            }
                            break;
                        }
                        tool_iter += 1;

                        let mut result_blocks: Vec<Block> = Vec::new();
                        for block in &current_turn.content {
                            let Block::ToolCall { id, name, args, .. } = block else {
                                continue;
                            };
                            // Operator stop: skip the remaining dispatches.
                            // Safe to leave tool_use blocks unanswered —
                            // the stop path below never sends a
                            // continuation request, and persistence is
                            // text-only.
                            if stop_check.stopped() {
                                break;
                            }
                            // Embra_Debug #70: dedupe-first-seen tool
                            // name accumulator for the (no response)
                            // placeholder. Pushed before dispatch so a
                            // failing tool still appears in the
                            // transcript signal.
                            if !tool_names_in_turn.iter().any(|n| n == name) {
                                tool_names_in_turn.push(name.clone());
                            }
                            let seq = tool_calls::next_seq();
                            let started = std::time::Instant::now();
                            let started_at = chrono::Utc::now();
                            let started_at_rfc = started_at.to_rfc3339();
                            info!(
                                target: "dispatch",
                                session = %session_name,
                                tool = %name,
                                tool_use_id = %id,
                                "dispatch:start"
                            );
                            let ctx = tools::registry::DispatchContext {
                                db,
                                config: &loaded_config,
                                session_name: &session_name,
                                config_tz: &loaded_config.timezone,
                                trace: &trace_handle,
                                turn_index,
                                // The memorize turn of a guided delete: what
                                // it saves comes from the session it deletes.
                                deleting_session: delete_exec.as_ref().map(|d| d.name.as_str()),
                            };
                            // Operator-facing tool-execution indicator — rides
                            // on the existing Thinking signal via `current_tool`.
                            // Cleared after dispatch regardless of outcome so
                            // the indicator never lingers on a finished tool.
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Thinking(
                                    ThinkingState {
                                        is_thinking: true,
                                        name: loaded_config.name.clone(),
                                        current_tool: name.to_string(),
                                    }
                                )),
                            })).await;
                            // Mid-tool operator stop: abandon the dispatch
                            // future (drops it — same subprocess-leak
                            // caveat as the 600s timeout) and skip out.
                            let outcome = tokio::select! {
                                outcome = tools::registry::dispatch(
                                    name,
                                    args.clone(),
                                    ctx,
                                ) => outcome,
                                _ = stop_check.triggered() => {
                                    info!(
                                        target: "dispatch",
                                        session = %session_name,
                                        tool = %name,
                                        "dispatch abandoned by operator stop"
                                    );
                                    // The call's record, for the Tools tab
                                    // and `turn_trace`: a stopped call used
                                    // to leave no trace at all.
                                    let elapsed_ms = started.elapsed().as_millis() as u64;
                                    let input_json =
                                        serde_json::to_string(args).unwrap_or_default();
                                    if tool_calls::has_watchers() {
                                        tool_calls::emit(tool_calls::stopped_record(tool_calls::Call {
                                            seq,
                                            tool_use_id: id,
                                            name,
                                            session: &session_name,
                                            job_id: "",
                                            started_at,
                                            elapsed_ms,
                                            is_error: true,
                                            input_json: &input_json,
                                            result: tool_calls::STOPPED_RESULT,
                                        }));
                                    }
                                    let stopped_doc = serde_json::json!({
                                        "session": session_name.to_string(),
                                        "turn_index": turn_index,
                                        "seq": seq,
                                        "tool_use_id": id.clone(),
                                        "tool_name": name.clone(),
                                        "input_preview": preview_str(&input_json, TURN_TRACE_PREVIEW_MAX),
                                        "started_at": started_at_rfc.clone(),
                                        "elapsed_ms": elapsed_ms,
                                        "is_error": true,
                                        "stopped": true,
                                        "result_preview": tool_calls::STOPPED_RESULT,
                                    });
                                    let db_clone = db.clone();
                                    tokio::spawn(async move {
                                        let _ = db_clone.write("tools.turn_trace", &stopped_doc).await;
                                    });
                                    break;
                                }
                            };
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Thinking(
                                    ThinkingState {
                                        is_thinking: true,
                                        name: loaded_config.name.clone(),
                                        current_tool: String::new(),
                                    }
                                )),
                            })).await;
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            let (content, tool_images, tool_media_refs, tool_files, is_error) = match outcome {
                                Ok(out) => (out.text, out.images, out.media_refs, out.files, false),
                                Err(err) => {
                                    let msg = match err {
                                        embra_tools_core::DispatchError::Unknown(n) => format!(
                                            "Unknown tool: '{}'. Check the tool manifest for the correct name.",
                                            n
                                        ),
                                        embra_tools_core::DispatchError::BadInput { tool, source } => format!(
                                            "Input schema mismatch for '{}': {}",
                                            tool, source
                                        ),
                                        embra_tools_core::DispatchError::Handler(msg) => msg,
                                        embra_tools_core::DispatchError::Timeout { tool, limit_secs } => format!(
                                            "Tool '{}' exceeded the {}s global execution timeout. \
                                             The command may still be running on the remote/host but \
                                             its result will not be visible to you. Consider breaking \
                                             the work into smaller chunks or backgrounding it.",
                                            tool, limit_secs
                                        ),
                                    };
                                    // Every dispatch error is answered as a
                                    // tool result with `is_error`, a timeout
                                    // included: a tool call left without a
                                    // result is a 400 on the next request.
                                    (msg, Vec::new(), Vec::new(), Vec::new(), true)
                                }
                            };
                            // `bytes` is what the model receives: the text
                            // after the dispatcher's cap. A continuation
                            // that overflows the context names its
                            // results below; this line is the per-call
                            // record (Embra#13, finding 3).
                            info!(
                                target: "dispatch",
                                session = %session_name,
                                tool = %name,
                                tool_use_id = %id,
                                elapsed_ms,
                                is_error,
                                bytes = content.len(),
                                images = tool_images.len(),
                                "dispatch:end"
                            );

                            // Record in the in-memory turn trace and persist
                            // asynchronously to tools.turn_trace so cross-turn
                            // introspection is available to the model.
                            let input_json =
                                serde_json::to_string(args).unwrap_or_default();
                            let input_preview = preview_str(&input_json, TURN_TRACE_PREVIEW_MAX);
                            let result_preview = preview_str(&content, TURN_TRACE_PREVIEW_MAX);
                            let entry = embra_tools_core::TraceEntry {
                                tool_name: name.clone(),
                                tool_use_id: id.clone(),
                                input_preview: input_preview.clone(),
                                started_at: started_at_rfc.clone(),
                                elapsed_ms,
                                is_error,
                                result_preview: result_preview.clone(),
                            };
                            if let Ok(mut guard) = trace_handle.lock() {
                                guard.push_back(entry);
                            }
                            let persist_doc = serde_json::json!({
                                "session": session_name.to_string(),
                                "turn_index": turn_index,
                                "seq": seq,
                                "tool_use_id": id.clone(),
                                "tool_name": name.clone(),
                                "input_preview": input_preview,
                                "started_at": started_at_rfc,
                                "elapsed_ms": elapsed_ms,
                                "is_error": is_error,
                                "result_preview": result_preview,
                            });
                            let db_clone = db.clone();
                            tokio::spawn(async move {
                                let _ = db_clone.write("tools.turn_trace", &persist_doc).await;
                            });

                            // The live record for the Tools tab
                            // (`tool_calls`); the cuts and copies run only
                            // with a watcher.
                            if tool_calls::has_watchers() {
                                tool_calls::emit(tool_calls::record(tool_calls::Call {
                                    seq,
                                    tool_use_id: id,
                                    name,
                                    session: &session_name,
                                    job_id: "",
                                    started_at,
                                    elapsed_ms,
                                    is_error,
                                    input_json: &input_json,
                                    result: &content,
                                }));
                            }

                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Tool(
                                    ToolExecution {
                                        tool_use_id: id.clone(),
                                        tool_name: name.clone(),
                                        input_json,
                                        result: content.clone(),
                                        is_error,
                                        seq,
                                    }
                                )),
                            })).await;

                            // set_name persisted a new display name — re-emit
                            // an Operational ModeChange so the console prefix
                            // and status bar refresh mid-turn. Tools carry no
                            // tx; this is the loop-side companion, mirroring
                            // the /model handler's refresh recipe. Reload
                            // fresh: `loaded_config` still holds the old name.
                            if name == "set_name"
                                && !is_error
                                && let Ok(fresh) = config::load_config(db).await
                            {
                                let model_display =
                                    display_model_for(&fresh.api_provider, &fresh);
                                let _ = tx.send(Ok(ConversationResponse {
                                    response_type: Some(
                                        conversation_response::ResponseType::ModeChange(
                                            ModeTransition {
                                                from_mode: OperatingMode::Operational as i32,
                                                to_mode: OperatingMode::Operational as i32,
                                                message: operational_status_line(&fresh.name, &session_name, &fresh.timezone, &model_display),
                                            },
                                        ),
                                    ),
                                })).await;
                            }

                            // Operator-facing frames for images the tool put
                            // in the MEDIA store (tools carry no tx — this is
                            // the loop-side companion, like set_name's
                            // ModeChange). Emitted after the ToolExecution
                            // frame so clients render the card under it.
                            let announce = tool_images
                                .iter()
                                .filter_map(|img| img.media_ref.as_ref())
                                .chain(tool_media_refs.iter());
                            for m in announce {
                                let _ = tx.send(Ok(ConversationResponse {
                                    response_type: Some(conversation_response::ResponseType::Media(
                                        media::media_ref_from_tool(m, id),
                                    )),
                                })).await;
                                turn_assistant_refs.push(media::attachment_ref_from_tool(m));
                            }
                            // The same for files the tool offered for
                            // download (`file_offer`): a card for the
                            // operator, a ref on the assistant turn so the
                            // offer replays on attach.
                            for f in &tool_files {
                                let _ = tx.send(Ok(ConversationResponse {
                                    response_type: Some(conversation_response::ResponseType::File(
                                        media::offer::file_ref_from_tool(f, id),
                                    )),
                                })).await;
                                turn_assistant_refs.push(media::offer::attachment_ref_from_tool(f));
                            }
                            // Images ride the structured ToolResult (never the
                            // text — the byte cap would cut base64 silently);
                            // each provider's conv places them on its wire.
                            result_blocks.push(Block::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error,
                                images: media::tool_images_to_ir(tool_images),
                            });
                        }

                        // Operator stop during the dispatch pass (mid-tool
                        // select or the per-tool skip): re-enter the loop
                        // top, which overrides the outcome to OperatorStop
                        // and breaks through the terminal arm — the
                        // continuation request below must NOT be sent.
                        if stop_check.stopped() {
                            continue;
                        }

                        if result_blocks.is_empty() {
                            // outcome claimed ToolUse but no ToolCall blocks
                            // were present — treat as a terminal state and exit.
                            break;
                        }

                        api_messages.push(ApiMessage::user_tool_results(result_blocks));
                        let stream = provider
                            .stream_turn(
                                &api_messages,
                                &system_bundle,
                                &tool_manifest,
                                request_options,
                            )
                            .await
                            .map_err(|e| {
                                // A prompt too long for the model is the
                                // usual cause; name the results that were
                                // just appended, with their sizes, so the
                                // culprit is in the log (Embra#13).
                                let sizes = tool_result_sizes(
                                    &current_turn.content,
                                    api_messages.last().map(ApiMessage::content).unwrap_or(&[]),
                                );
                                anyhow::anyhow!(
                                    "Brain continuation failed (iter {tool_iter}; tool results this iteration: {sizes}): {e}"
                                )
                            })?;
                        let Some(resp) = collect_response(stream, tx, &config_name, Some(&mut stop_check)).await? else {
                            // The stream ended without a result: the server
                            // answered with an error inside it (LM Studio does
                            // so for a prompt over its context length; shown by
                            // collect_response) or the connection went. The
                            // same log line as the pre-stream failure above,
                            // with the results that went out (Embra#13); the
                            // turn ends like a stop and persists what it has.
                            let sizes = tool_result_sizes(
                                &current_turn.content,
                                api_messages.last().map(ApiMessage::content).unwrap_or(&[]),
                            );
                            error!(
                                target: "dispatch",
                                session = %session_name,
                                "Brain continuation failed (iter {tool_iter}; tool results this iteration: {sizes}): the stream ended without a result"
                            );
                            persisted_segments.push(STREAM_FAILED_MARKER);
                            stream_failed = true;
                            break;
                        };
                        current_turn = resp;
                        last_response_text = turn_text(&current_turn);
                        persisted_segments.push(&last_response_text);
                        undelivered = undelivered_after(&current_turn);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_turn.content.clone(),
                        ));
                    }
                }
            }

            // A stop that landed on a path that `break`s past the loop top
            // (the iteration cap's final call) is still a stop: the same
            // override the loop top applies, so the notice and the
            // persisted marker say so.
            if stop_check.stopped() {
                current_turn.outcome = TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop);
            }
            // Surface refusal / truncation terminal outcomes as operator-
            // facing frames — a refusal must never end a turn silently (and
            // is never silently retried on a fallback model; rationale in
            // `terminal_outcome_notice`). Single post-loop site so every
            // exit path from the tool loop is covered; normal outcomes and
            // the stale-turn abort paths (`ToolUse`/`Pause`) yield no frame.
            let notice =
                terminal_outcome_notice(current_turn.outcome, current_turn.stop_details.as_ref());
            if current_turn.outcome == TurnOutcome::EarlyStop(EarlyStopReason::Refusal) {
                warn!(
                    target: "dispatch",
                    session = %session_name,
                    category = current_turn
                        .stop_details
                        .as_ref()
                        .and_then(|d| d.category.as_deref())
                        .unwrap_or("unspecified"),
                    "turn ended with provider refusal"
                );
            }
            // The turn closes with one `Done` first — the text the operator
            // saw stream in without its `Done`, or an empty one after a stop
            // or a failed stream so the clients end their live state — and
            // the notice after it, under the text it talks about. Text that
            // a `Complete` already closed is never sent again: both clients
            // add a row per non-empty `Done`. `closing_frames` decides.
            let stopped =
                current_turn.outcome == TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop);
            for frame in closing_frames(stopped, stream_failed, undelivered.take(), notice) {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(frame),
                    }))
                    .await;
            }

            // Per-turn telemetry — surfaces the customtools-needed
            // pattern (spec D8). Operators can grep journalctl for
            // turns where Gemini emitted text but zero tool calls
            // and consider swapping to gemini-3.1-pro-preview-customtools.
            if provider_kind == ProviderKind::Gemini {
                let tool_call_count = trace_handle
                    .lock()
                    .map(|g| g.len())
                    .unwrap_or(0);
                info!(
                    target: "gemini::diag",
                    session = %session_name,
                    text_chars = last_response_text.len(),
                    tool_calls = tool_call_count,
                    iters = tool_iter,
                    "gemini turn complete"
                );
            }

            drop(in_turn_guard);

            // Drain any /provider switch the operator queued
            // mid-loop. Performed after in_turn clears and before
            // we save history so the next user-message handler
            // observes the new provider state.
            {
                let mut guard = pending_provider.lock().await;
                if let Some(target) = guard.take() {
                    perform_provider_swap(target, db, session_mgr, tx).await;
                }
            }

            // Save conversation to session history.
            //
            // Persistence still uses the legacy Message { role, content: String }
            // shape — Stage 8 introduces format_version-aware typed-block
            // storage. We store the raw user message (pre-enrichment) and the
            // concatenated text of the final assistant response. Thinking
            // signatures and tool_use/tool_result blocks are NOT persisted in
            // Stage 5; the model doesn't require cross-turn thinking
            // preservation, and tool results are dropped silently (they reconstruct
            // on re-runs if the user asks again).
            {
                let mgr = session_mgr.read().await;
                let final_text = final_assistant_text(
                    &persisted_segments.joined(),
                    &tool_names_in_turn,
                    current_turn.outcome,
                );
                // One write for the turn (#59): the operator's message and
                // the reply land together or not at all, and the events
                // watermark rides the meta write that follows the history
                // write, so what happened during the turn is shown next
                // time only once the turn is on record.
                let turn = [
                    Message::user_with_attachments(&msg.content, turn_refs.clone()),
                    Message::assistant_with_attachments(&final_text, turn_assistant_refs.clone()),
                ];
                match mgr.append_turn(&session_name, &turn, Some(turn_started)).await {
                    Ok(()) => {}
                    Err(crate::sessions::SessionError::LegacyReadOnly(sess)) => {
                        warn!(
                            target: "sessions",
                            session = %sess,
                            "legacy session rejected append; surfacing to client"
                        );
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Session '{}' is legacy (pre-native-tools) and is read-only. Create a new session to continue.",
                                        sess
                                    ),
                                    msg_type: SystemMessageType::Error as i32,
                                }
                            )),
                        })).await;
                        return Ok(());
                    }
                    Err(e) => {
                        // A failed write is said, never swallowed (#59): the
                        // reply went out, so the operator is told the history
                        // does not carry this turn.
                        warn!(target: "sessions", session = %session_name, "the turn was not saved: {e}");
                        send_system(
                            tx,
                            format!(
                                "This turn was not saved to session '{}': {e}. The reply above stands, but the history does not carry it; try again once the database answers.",
                                session_name
                            ),
                            SystemMessageType::Warning,
                        )
                        .await;
                    }
                }
            }

            // Guided-delete execution — AFTER persistence, deliberately:
            // soft_delete stamps `deleted_at` onto the history doc, and
            // append_turn above rewrites that doc from the typed
            // SessionHistory struct (which has no deleted_at field), so a
            // pre-persistence stamp would be silently dropped. Fail-closed:
            // only the READY marker deletes; ABORT / no marker / a failed
            // turn all leave the session untouched (the flow state was
            // already consumed, so a retry is a fresh /sessions delete).
            if let Some(del) = delete_exec {
                let was_active = session_name == del.name;
                let verdict = delete_flow_verdict(&last_response_text);
                let (kind, note) = match verdict {
                    DeleteVerdict::Ready => {
                        let store = session_mgr.read().await.store();
                        let result = store.soft_delete(&del.name, msg.content.trim()).await;
                        if result.is_ok() {
                            session_mgr.write().await.forget(&del.name);
                        }
                        match result {
                            Ok(()) => {
                                // The brain records the deletion, never the
                                // model: only a deletion that happened gets
                                // a record, with the operator's own words,
                                // linked to what the memorize turn kept.
                                let when = format_local_timestamp(chrono::Utc::now(), &loaded_config.timezone);
                                let record = crate::sessions::delete_record::record_deletion(
                                    db,
                                    &del.name,
                                    msg.content.trim(),
                                    turn_started,
                                    &when,
                                    &loaded_config,
                                )
                                .await;
                                let (record_sentence, record_warns) =
                                    crate::sessions::delete_record::record_sentence(&record);
                                // The summary is stamped and reaped with the
                                // history (`soft_delete`); the memories stay.
                                let mut note = format!(
                                    "Session '{}' deleted ({}-day grace period — `/sessions restore {}` to undo). The extracted memories stay in the knowledge graph; the summary goes with the session. {}",
                                    del.name,
                                    crate::sessions::SESSION_DELETE_RETENTION_DAYS,
                                    del.name,
                                    record_sentence,
                                );
                                if was_active {
                                    note.push_str(
                                        " No session is now attached — /switch <name> or /new <name>.",
                                    );
                                }
                                let kind = if record_warns { SystemMessageType::Warning } else { SystemMessageType::Info };
                                (kind, note)
                            }
                            Err(e) => (
                                SystemMessageType::Error,
                                format!("Session delete of '{}' failed: {}", del.name, e),
                            ),
                        }
                    }
                    DeleteVerdict::Abort => (
                        SystemMessageType::Info,
                        format!("Session deletion of '{}' not executed (aborted).", del.name),
                    ),
                    DeleteVerdict::NoMarker => (
                        SystemMessageType::Warning,
                        format!(
                            "Session deletion of '{}' not executed — the memory turn ended without a completion marker. Re-run `/sessions delete {}` to retry.",
                            del.name, del.name,
                        ),
                    ),
                };
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage { content: note, msg_type: kind as i32 },
                        )),
                    }))
                    .await;
            }

            Ok(())
        }

        Some(conversation_request::RequestType::SlashCommand(cmd)) => {
            // Sprint 5 follow-up: cancel any in-flight OpenAI-compat
            // reconfigure when a slash command arrives — except when
            // the new command is itself /provider --setup (re-entry,
            // handled by enter_openai_compat_setup which replaces the
            // state). All other slash commands surface a SystemMessage
            // before dispatching to the regular handler.
            {
                let cancelled = pending_openai_compat_setup.lock().await.take();
                if cancelled.is_some() {
                    let is_reentry = cmd.command == "/provider"
                        && cmd.args.contains("--setup");
                    if !is_reentry {
                        let _ = tx
                            .send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: "OpenAI-compat setup cancelled.".to_string(),
                                        msg_type: SystemMessageType::Info as i32,
                                    },
                                )),
                            }))
                            .await;
                    }
                }
            }
            // A slash command ends the model step of the key setup; the
            // key step stays armed, as before. A `/provider --setup`
            // re-entry starts a new flow and needs no word.
            {
                let mut guard = pending_key_setup.lock().await;
                let (kept, ended) = key_setup_after_slash(*guard);
                *guard = kept;
                drop(guard);
                let is_reentry = cmd.command == "/provider" && cmd.args.contains("--setup");
                if let Some(target) = ended
                    && !is_reentry
                {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Model choice cancelled; the {} model is unchanged.",
                                        cloud_provider_name(target)
                                    ),
                                    msg_type: SystemMessageType::Info as i32,
                                },
                            )),
                        }))
                        .await;
                }
            }
            // Any slash command also cancels an in-flight guided session
            // deletion (always announced — even a /sessions delete re-entry
            // replaces the old flow, and saying so is honest). Deliberately
            // unlike pending_key_setup, which a slash leaves armed.
            {
                let cancelled = pending_session_delete.lock().await.take();
                if let Some(state) = cancelled {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Session delete flow for '{}' cancelled.",
                                        state.name
                                    ),
                                    msg_type: SystemMessageType::Info as i32,
                                },
                            )),
                        }))
                        .await;
                }
            }
            if let Some(synthetic_prompt) = handle_slash_command(
                &cmd.command, &cmd.args, tx, ctx, config_tz, api_key
            ).await {
                // Slash command requested a synthetic user turn — feed it through the Brain.
                let synthetic = ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: synthetic_prompt, timestamp: None, attachment_ids: Vec::new(), file_paths: Vec::new() }
                    )),
                };
                Box::pin(handle_request(synthetic, tx, ctx, config_tz, api_key, stop_rx)).await?;
            }
            Ok(())
        }

        Some(conversation_request::RequestType::SessionAttach(attach)) => {
            // The round trips of an attach run on the store handle with no
            // lock on the manager held (#59): the manager's lock is
            // write-preferring, and a writer parked on a round trip held
            // every reader, the per-turn history load and the activity
            // feed among them. The manager is written for a blink, below.
            let (store, active) = {
                let mgr = session_mgr.read().await;
                (mgr.store(), mgr.active_session.clone())
            };

            // Determine session: explicit name > most recent active > "main"
            let session_name = if !attach.session_name.is_empty() {
                attach.session_name.clone()
            } else if let Some(active) = active {
                active
            } else if let Ok(Some(recent)) = store.get_most_recent_active().await {
                recent.name.clone()
            } else {
                "main".to_string()
            };

            // Ensure session exists. A probe that failed is not an absent
            // session: "new" would skip the deleted-session guard and the
            // cross-provider check and attach the wrong thing (#59).
            let new_session = match store.session_exists(&session_name).await {
                Ok(exists) => !exists,
                Err(e) => {
                    send_system(
                        tx,
                        format!("Could not check session '{}': {}. Nothing was attached; try again.", session_name, e),
                        SystemMessageType::Error,
                    )
                    .await;
                    return Ok(());
                }
            };
            if new_session && let Err(e) = store.create(&session_name).await {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                            content: format!("Could not create session '{}': {}", session_name, e),
                            msg_type: SystemMessageType::Error as i32,
                        })),
                    }))
                    .await;
                return Ok(());
            }

            // Soft-deleted sessions are unattachable during their grace
            // period (the sheet filters them out, but a stale client or an
            // explicit name can still ask). Restore is the sanctioned path.
            if !new_session {
                let is_deleted = store
                    .get_meta(&session_name)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|m| m.state == SessionState::Deleted);
                if is_deleted {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Session '{}' is deleted (grace period) — /sessions restore {} to recover it, or /new a different session.",
                                        session_name, session_name
                                    ),
                                    msg_type: SystemMessageType::Error as i32,
                                },
                            )),
                        }))
                        .await;
                    return Ok(());
                }
            }

            // Cross-provider session-resume hard-block (locked
            // decision #3). For an existing session whose meta
            // recorded a different provider than the active
            // process-level provider, refuse the attach with a
            // clear error rather than silently corrupting state on
            // the first turn.
            if !new_session
                && let Err((session_provider, active_provider)) =
                    check_session_provider(db, &session_name).await
            {
                let active_session =
                    session_mgr.read().await.active_session.clone();
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: cross_provider_block_message(
                                    &session_name,
                                    &session_provider,
                                    &active_provider,
                                    active_session.as_deref(),
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                return Ok(());
            }

            // Mark active only after the cross-provider check passes.
            session_mgr.write().await.active_session = Some(session_name.clone());
            activity::emit(activity::Event::SessionAttached { name: session_name.clone() });

            // Load and send session history so console displays prior
            // conversation. Capture `history.len()` here so the
            // "restored" message and the resume-briefing gate further
            // down don't need a second load_history round-trip. A history
            // that cannot be read is said as such (#59), never reported as
            // "0 messages restored".
            let history_len: usize = {
                match store.load_history(&session_name).await {
                    Ok(history) => {
                        replay_history(tx, &session_name, &history).await;
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!("Session '{}' attached ({} messages restored)", session_name, history.len()),
                                    msg_type: SystemMessageType::Reconnection as i32,
                                }
                            )),
                        })).await;
                        history.len()
                    }
                    Err(e) => {
                        send_system(
                            tx,
                            format!(
                                "Session '{}' attached, but its history could not be loaded: {}. Nothing was replayed; the next turn reads it again.",
                                session_name, e
                            ),
                            SystemMessageType::Error,
                        )
                        .await;
                        0
                    }
                }
            };

            // Send ModeTransition so console knows the correct mode, timezone, and name
            let is_sealed = learning::is_soul_sealed(db).await.unwrap_or(false);
            let mode = if is_sealed { OperatingMode::Operational } else { OperatingMode::Learning };
            let cfg = config::load_config(db).await.ok();
            let tz = cfg.as_ref().map(|c| c.timezone.clone()).unwrap_or_else(|| config_tz.to_string());
            let name = cfg.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| "Embra".to_string());
            let active_provider = cfg.as_ref().map(|c| c.api_provider.clone()).unwrap_or_else(|| "anthropic".to_string());
            let model = match cfg.as_ref() {
                Some(c) => display_model_for(&active_provider, c),
                None => crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string(),
            };
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::ModeChange(
                    ModeTransition {
                        from_mode: OperatingMode::Unspecified as i32,
                        to_mode: mode as i32,
                        message: if is_sealed {
                            operational_status_line(&name, &session_name, &tz, &model)
                        } else {
                            format!("Learning Mode — Name: {} — TZ: {} — Brain: {}", name, tz, model)
                        },
                    }
                )),
            })).await;

            // Resume-briefing trigger: existing session, prior history,
            // Operational mode only. Mirrors the slash-command synthetic-turn
            // precedent above (`handle_slash_command` → `Box::pin(handle_request(...))`).
            // The UserMessage handler reads the flag we set here, swaps in
            // the `<session_resumption>` wrapper for the brain-facing call,
            // and persists `[Session resumed]` (raw) to history. No wrapper
            // leak into subsequent turns.
            //
            // Gated twice (session-ux-fixes): the idle gate keeps transport
            // flaps from costing a recap turn each (mobile browsers drop and
            // reopen the chat WS on every background/lock, and each reopen
            // re-attaches), and the attempt cooldown keeps an aborted
            // mid-recap briefing from restarting on every re-attach. A
            // missing/unreadable meta fires — preserves pre-gate behavior
            // on broken-meta paths (history already loaded fine).
            if !new_session && is_sealed && history_len > 0 {
                let meta = store.get_meta(&session_name).await.ok().flatten();
                let (idle_ok, idle_secs) = match &meta {
                    Some(m) => resume_briefing_idle_ok(m.last_active, chrono::Utc::now()),
                    None => (true, i64::MAX),
                };
                let fire = idle_ok && {
                    let mut mgr = session_mgr.write().await;
                    if mgr.briefing_attempt_recent(
                        &session_name,
                        std::time::Duration::from_secs(RESUME_BRIEFING_ATTEMPT_COOLDOWN_SECS),
                    ) {
                        false
                    } else {
                        mgr.record_briefing_attempt(&session_name);
                        // Attach does not touch `last_active`, so the meta
                        // read for the idle gate still holds the start of
                        // the absence.
                        mgr.pending_resume_briefing = Some(crate::sessions::ResumeBriefing {
                            away_since: meta.as_ref().map(|m| m.last_active),
                        });
                        true
                    }
                };
                if fire {
                    let synthetic = ConversationRequest {
                        request_type: Some(conversation_request::RequestType::UserMessage(
                            UserMessage {
                                content: "[Session resumed]".to_string(),
                                timestamp: None,
                                attachment_ids: Vec::new(),
                                file_paths: Vec::new(),
                            }
                        )),
                    };
                    Box::pin(handle_request(synthetic, tx, ctx, config_tz, api_key, stop_rx)).await?;
                } else {
                    info!(
                        target: "sessions",
                        session = %session_name,
                        idle_secs,
                        min_idle_secs = RESUME_BRIEFING_MIN_IDLE_SECS,
                        suppressed_by = if idle_ok { "attempt-cooldown" } else { "idle-gate" },
                        "resume briefing suppressed on attach"
                    );
                }
            }

            Ok(())
        }

        None => Ok(()),
    }
}

/// Drop guard that clears the per-service `in_turn` flag when the
/// future scope ends — including cancellation paths. Without this, a
/// dropped client connection mid-loop would leave `/provider` queued
/// forever.
pub(super) struct InTurnGuard(Arc<AtomicBool>);

/// Mark a turn as running and take its stop observer.
///
/// `StopTurn` raises the stop counter only while a turn is marked as
/// running, and a turn sees only a stop that was raised after it took its
/// observer. Both belong to the start of a turn, in the operational loop
/// and in the learning loop alike: a loop that takes the observer and does
/// not set the mark cannot be stopped. The guard clears the mark when it is
/// dropped, also when the turn's future is cancelled. The activity feed
/// hears the turn start here and its end from the guard.
pub(super) fn begin_turn(
    in_turn: &Arc<AtomicBool>,
    stop_rx: &watch::Receiver<u64>,
    session: &str,
) -> (InTurnGuard, StopCheck) {
    in_turn.store(true, Ordering::SeqCst);
    activity::emit(activity::Event::TurnStarted { session: session.to_string() });
    (InTurnGuard(in_turn.clone()), StopCheck::new(stop_rx.clone()))
}

impl Drop for InTurnGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
        activity::emit(activity::Event::TurnEnded);
    }
}

/// The two halves of the operator stop, handed to a loop that starts turns
/// of its own: the mark that a turn is running and the stop counter.
#[derive(Clone, Copy)]
pub(super) struct TurnStop<'a> {
    pub(super) in_turn: &'a Arc<AtomicBool>,
    pub(super) stop_rx: &'a watch::Receiver<u64>,
}

impl TurnStop<'_> {
    /// [`begin_turn`] with both halves, for a turn of `session`.
    pub(super) fn begin(self, session: &str) -> (InTurnGuard, StopCheck) {
        begin_turn(self.in_turn, self.stop_rx, session)
    }
}

/// The snapshot frame of the activity feed: the service's part is read
/// here, the rest comes from `activity::snapshot`.
pub(super) async fn activity_snapshot_frame(
    session_mgr: &Arc<RwLock<SessionManager>>,
    in_turn: &AtomicBool,
    stage: &watch::Sender<i32>,
    start_time: std::time::Instant,
    sample_rx: &watch::Receiver<Option<activity::DbSample>>,
) -> ActivityFrame {
    let active_session = session_mgr.read().await.active_session.clone();
    let sample = sample_rx.borrow().clone();
    let snapshot = activity::snapshot(activity::SnapshotInput {
        stage: *stage.borrow(),
        in_turn: in_turn.load(Ordering::SeqCst),
        active_session,
        uptime_seconds: start_time.elapsed().as_secs(),
        sample: sample.as_ref(),
    });
    ActivityFrame { frame: Some(activity_frame::Frame::Snapshot(snapshot)) }
}

/// Convert a legacy on-disk `Message` (role + String content) to the
/// neutral `ApiMessage` IR. Thinking signatures were never persisted,
/// so every historical turn becomes a text-only block. Migration-era
/// shim; future schema bump introduces typed-block persistence.
pub(super) fn legacy_message_to_api(m: &Message) -> ApiMessage {
    // Text-only fast path; the attachment-aware conversion is
    // `media::replay::history_to_api`.
    media::replay::legacy_message_to_api(m)
}

/// Byte cap for `tools.turn_trace` input/result previews. Historically 200,
/// which clipped most tool args/results below usefulness for cross-turn
/// introspection (reviewing exact tool parameters after a turn). 2 KiB keeps
/// the worst-case trace doc ~4.3 KB (one doc per tool call) while
/// `turn_trace`'s max read (100 entries) stays ~430 KB, well under the 2 MiB
/// tool-result backstop.
pub(super) const TURN_TRACE_PREVIEW_MAX: usize = 2048;

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, appending an
/// ellipsis when truncation occurred. Used to bound trace-entry previews.
pub(super) fn preview_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Extract the plain-text portion of an assistant turn for session
/// persistence. Concatenates all `Text` blocks; thinking signatures and
/// tool calls are dropped.
/// The tool results of one loop iteration with their sizes, for the
/// continuation failure that carries them: `file_read 1048576 B, git_log
/// 2048 B (2 results, 1050624 B total)`. A result whose call is not in
/// `calls` is named by its call id.
pub(super) fn tool_result_sizes(calls: &[Block], results: &[Block]) -> String {
    let name_of = |call_id: &str| -> String {
        calls
            .iter()
            .find_map(|b| match b {
                Block::ToolCall { id, name, .. } if id == call_id => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| call_id.to_string())
    };
    let mut parts: Vec<String> = Vec::new();
    let mut total = 0usize;
    for block in results {
        if let Block::ToolResult { call_id, content, .. } = block {
            total += content.len();
            parts.push(format!("{} {} B", name_of(call_id), content.len()));
        }
    }
    format!("{} ({} results, {} B total)", parts.join(", "), parts.len(), total)
}

pub(super) fn turn_text(turn: &AssistantTurn) -> String {
    turn.content
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Operator-visible assistant text accumulated across the tool-loop
/// iterations of ONE user turn, for session-transcript persistence.
/// Before this existed only the final iteration's text survived to
/// history while every iteration streamed live (`collect_response`
/// emits a Done frame per iteration) — narration emitted before or
/// between tool calls was lost from the recorded transcript.
///
/// REASONING-STREAM-01: fed exclusively from `turn_text` output
/// (`Block::Text` only) and the synthetic operator-visible markers the
/// loop already streams — never from `ReasoningDelta`.
/// `last_response_text` keeps its final-segment semantics for the
/// operator-stop Done frame, Gemini telemetry, and
/// `delete_flow_verdict`.
/// The segment persisted after the text of a continuation whose stream
/// ended without a result, so the operator on reload and the model on
/// the next turn see that the reply was cut by the provider, not chosen.
/// The server's words are in the console and the log, never here.
pub(super) const STREAM_FAILED_MARKER: &str = "(turn ended by a provider error; see the error above)";

#[derive(Default)]
pub(super) struct TranscriptSegments(Vec<String>);

impl TranscriptSegments {
    /// Append a segment; empty/whitespace-only text is skipped so
    /// text-less tool iterations don't inject blank paragraphs.
    pub(super) fn push(&mut self, segment: &str) {
        if !segment.trim().is_empty() {
            self.0.push(segment.to_string());
        }
    }

    /// Join with a blank line — the per-iteration boundary the operator
    /// watched live. Single-segment turns join byte-identically to the
    /// segment itself.
    pub(super) fn joined(&self) -> String {
        self.0.join("\n\n")
    }
}

/// Render the placeholder persisted to the session transcript when an
/// assistant turn fired tool calls but produced no text in its terminal
/// iteration. Embra_Debug #70: previously persisted as `(no response)`,
/// which made tool-only turns look like silence on session reload.
///
/// First eight names are rendered verbatim (deduped first-seen order at
/// the call site); a trailing `, …` is appended when more names exist.
pub(super) fn render_tool_only_placeholder(names: &[String]) -> String {
    const MAX_NAMES: usize = 8;
    let head = names
        .iter()
        .take(MAX_NAMES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if names.len() > MAX_NAMES { ", …" } else { "" };
    format!("[tool calls: {}{}]", head, suffix)
}

/// Operator-facing notice for a terminal turn outcome, emitted post-loop by
/// the driver so a refusal or truncation can never end a turn silently.
/// `None` for the normal outcomes — `EndTurn` needs no frame, and
/// `ToolUse`/`Pause` are only terminal here when a stream aborted mid-loop,
/// which already surfaced its own error frame.
pub(super) fn terminal_outcome_notice(
    outcome: TurnOutcome,
    stop_details: Option<&StopDetails>,
) -> Option<(SystemMessageType, String)> {
    match outcome {
        TurnOutcome::EndTurn | TurnOutcome::ToolUse | TurnOutcome::Pause => None,
        TurnOutcome::MaxTokens => Some((
            SystemMessageType::Warning,
            "Response hit the output-token limit (max_tokens) — the reply above may be \
             truncated mid-thought."
                .to_string(),
        )),
        // The Anthropic API offers server-side fallback-to-another-model on
        // refusal (`betas: ["server-side-fallback-2026-06-01"]` + a
        // `fallbacks` param), and it is opt-in; embraOS deliberately does
        // NOT opt in — the OS gives a single model a sealed identity, and
        // silently swapping models mid-conversation would be a continuity
        // violation. Refusals surface to the operator instead.
        TurnOutcome::EarlyStop(EarlyStopReason::Refusal) => {
            let mut msg =
                String::from("Anthropic declined to complete this response (refusal");
            if let Some(cat) = stop_details.and_then(|d| d.category.as_deref()) {
                msg.push_str(", category: ");
                msg.push_str(cat);
            }
            msg.push(')');
            if let Some(expl) = stop_details.and_then(|d| d.explanation.as_deref()) {
                msg.push_str(" — ");
                msg.push_str(expl);
            }
            msg.push_str(". Any partial text above is incomplete. No fallback model was used.");
            Some((SystemMessageType::Error, msg))
        }
        // Truncation, like MaxTokens: the reply is incomplete, nothing
        // was declined. The remedy is a shorter history, which for the
        // operator means a new session.
        TurnOutcome::EarlyStop(EarlyStopReason::ContextWindow) => Some((
            SystemMessageType::Warning,
            "Response stopped because the conversation no longer fits the model's context \
             window — the reply above may be cut off. Continue in a new session (/new <name>)."
                .to_string(),
        )),
        // The server closed the stream mid-reply (OpenAI-compat: neither a
        // finish reason nor [DONE]). Like MaxTokens: incomplete, nothing
        // declined, the partial text persisted unmarked.
        TurnOutcome::EarlyStop(EarlyStopReason::StreamEnded) => Some((
            SystemMessageType::Warning,
            "The model server closed the stream before finishing the reply — the text above \
             may be incomplete."
                .to_string(),
        )),
        // Operator interrupt — not an error and not a provider action; the
        // generic "Provider stopped..." wording below would misattribute it.
        TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop) => Some((
            SystemMessageType::Warning,
            "Turn stopped by operator — any partial response above is incomplete.".to_string(),
        )),
        TurnOutcome::EarlyStop(reason) => Some((
            SystemMessageType::Error,
            format!("Provider stopped this response early ({reason:?})."),
        )),
    }
}

/// Tells one client that the frames that follow replay a session's history:
/// it clears its transcript first. `content` names the session. Sent at the
/// head of every replay and alone by `/new`, whose session has no history.
pub(super) async fn send_transcript_reset(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    session: &str,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content: session.to_string(),
                msg_type: SystemMessageType::TranscriptReset as i32,
            })),
        }))
        .await;
}

/// Replays a session's history to one client: the reset frame, then one
/// `[user] …` / `[assistant] …` row per message with its `replay:true` media
/// and file frames right under it, so the cards come back with their turn.
/// The attach and `/switch` both go through here, so they show the same
/// thing.
pub(super) async fn replay_history(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    session: &str,
    history: &[crate::brain::Message],
) {
    send_transcript_reset(tx, session).await;
    for msg in history {
        let role_display = if msg.role == "user" { "user" } else { "assistant" };
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!("[{}] {}", role_display, msg.content),
                        msg_type: SystemMessageType::Reconnection as i32,
                    },
                )),
            }))
            .await;
        for r in msg.attachment_refs() {
            let frame = if r.is_image() {
                conversation_response::ResponseType::Media(media::media_ref_from_attachment(r))
            } else {
                let origin = if msg.role == "user" { "attached" } else { "offered" };
                conversation_response::ResponseType::File(media::text::file_ref_from_attachment(
                    r, origin,
                ))
            };
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(frame),
                }))
                .await;
        }
    }
}

/// The text of a response whose `Done` never went out. A stop that cuts the
/// stream comes back from `collect_response` as an `OperatorStop` turn with
/// the partial and no `Done`; every `Complete` sends its own.
pub(super) fn undelivered_after(turn: &AssistantTurn) -> Option<String> {
    (turn.outcome == TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop))
        .then(|| turn_text(turn))
}

/// The frames that close a turn, in order: a `Done` first — carrying text the
/// operator saw stream in without its `Done`, or empty after a stop or a
/// failed stream so the clients end their live state — then the notice,
/// under the text it talks about. A normal end whose text was closed by its
/// own `Done` sends nothing. Text a `Complete` already closed is never sent
/// again: the console and the mobile chat add a row per non-empty `Done`.
pub(super) fn closing_frames(
    stopped: bool,
    stream_failed: bool,
    undelivered: Option<String>,
    notice: Option<(SystemMessageType, String)>,
) -> Vec<conversation_response::ResponseType> {
    let mut frames = Vec::new();
    match undelivered {
        Some(text) => frames.push(conversation_response::ResponseType::Done(StreamDone {
            full_response: text,
        })),
        None if stopped || stream_failed => {
            frames.push(conversation_response::ResponseType::Done(StreamDone {
                full_response: String::new(),
            }))
        }
        None => {}
    }
    if let Some((kind, content)) = notice {
        frames.push(conversation_response::ResponseType::System(SystemMessage {
            content,
            msg_type: kind as i32,
        }));
    }
    frames
}

/// Final assistant text persisted to session history for a terminal turn.
/// `transcript_text` is the joined `TranscriptSegments` accumulator (every
/// iteration's text, blank-line separated), so outcome suffixes land once
/// at the very end of the full transcript. Extracted from the inline
/// persistence chain so refusal honesty is unit-testable: a refused turn
/// must never persist as a bare normal message. Empty refusals persist an explicit marker (instead of
/// `(no response)`, and taking precedence over the tool placeholder —
/// the refusal frame and `turn_trace` carry the tool detail); refused
/// partials carry an interruption suffix so both the operator on session
/// reload and the model on the next turn see that the reply was cut.
pub(super) fn final_assistant_text(
    transcript_text: &str,
    tool_names: &[String],
    outcome: TurnOutcome,
) -> String {
    let refused = matches!(outcome, TurnOutcome::EarlyStop(EarlyStopReason::Refusal));
    let stopped = matches!(outcome, TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop));
    if !transcript_text.trim().is_empty() {
        if refused {
            format!("{transcript_text}\n\n(response interrupted by provider refusal)")
        } else if stopped {
            format!("{transcript_text}\n\n(response interrupted by operator stop)")
        } else {
            transcript_text.to_string()
        }
    } else if refused {
        "(request declined by provider: refusal)".to_string()
    } else if stopped {
        // Takes precedence over the tool placeholder for the same reason
        // the refusal marker does: the stop frame + turn_trace carry the
        // tool detail, and the next turn must see the interruption.
        crate::sessions::model_view::OPERATOR_STOP_MARKER.to_string()
    } else if !tool_names.is_empty() {
        render_tool_only_placeholder(tool_names)
    } else {
        "(no response)".to_string()
    }
}

/// gRPC status mapping for the media RPCs.
/// An offer's refusal, in the words the operator reads back.
pub(super) fn offer_status(e: media::offer::OfferError) -> Status {
    use media::offer::OfferError;
    match e {
        OfferError::Denied(_) | OfferError::NotAFile(_) => Status::invalid_argument(e.to_string()),
        OfferError::NotFound(_) => Status::not_found(e.to_string()),
        OfferError::TooLarge(..) => Status::resource_exhausted(e.to_string()),
        OfferError::Io(_) => Status::internal(e.to_string()),
    }
}

/// A text upload's refusal, in the words the operator reads back.
pub(super) fn text_status(e: media::text::TextError) -> Status {
    use media::text::TextError;
    match e {
        TextError::TooLarge(..) => Status::resource_exhausted(e.to_string()),
        TextError::NotText => Status::invalid_argument(
            "not an image (PNG, JPEG, GIF, WebP) and not UTF-8 text; binaries are refused",
        ),
        TextError::Io(_) => Status::internal(e.to_string()),
    }
}

pub(super) fn media_status(e: media::store::MediaError) -> Status {
    use media::ingest::IngestError;
    use media::store::MediaError;
    match e {
        MediaError::Ingest(IngestError::TooLarge(..)) => Status::resource_exhausted(e.to_string()),
        MediaError::Ingest(_) => Status::invalid_argument(e.to_string()),
        MediaError::BadId(_) => Status::invalid_argument(e.to_string()),
        MediaError::NotFound(_) => Status::not_found(e.to_string()),
        MediaError::Corrupt(..) | MediaError::Io(_) => Status::internal(e.to_string()),
    }
}

/// When the loop driver hits its iteration cap with `current_turn` carrying
/// undispatched `ToolCall` blocks, synthesize one `Block::ToolResult` per
/// call so the next API request satisfies Anthropic's "every tool_use must
/// be answered by a tool_result" invariant. Embra_Debug #80.
///
/// `cap_msg` is replayed verbatim into every result with `is_error: true`,
/// instructing the model to summarize and stop. Non-`ToolCall` blocks in
/// `blocks` are ignored. An empty input yields an empty output — callers
/// must skip pushing a degenerate `user_tool_results(vec![])`.
pub(super) fn synthesize_cap_results(blocks: &[Block], cap_msg: &str) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolCall { id, .. } => Some(Block::ToolResult {
                call_id: id.clone(),
                content: cap_msg.to_string(),
                is_error: true,
                images: Vec::new(),
            }),
            _ => None,
        })
        .collect()
}

/// Operator-stop observer for one turn: a subscribed watch receiver plus
/// the stop-generation value snapshotted at turn start. "Stopped" = the
/// counter advanced past the baseline — so a StopTurn RPC can only ever
/// affect the turn that was in flight when it fired (a pre-turn bump is
/// below the next turn's baseline; no reset is needed).
pub(crate) struct StopCheck {
    pub(super) rx: watch::Receiver<u64>,
    pub(super) baseline: u64,
}

impl StopCheck {
    pub(crate) fn new(rx: watch::Receiver<u64>) -> Self {
        let baseline = *rx.borrow();
        Self { rx, baseline }
    }

    /// Non-blocking poll — the between-iterations / per-tool checkpoints.
    pub(crate) fn stopped(&self) -> bool {
        *self.rx.borrow() > self.baseline
    }

    /// Await the stop signal (`select!` arms: mid-stream, mid-tool).
    /// Resolves only when the generation advances past the baseline;
    /// pends forever if the sender is gone (no stop can ever arrive).
    pub(crate) async fn triggered(&mut self) {
        loop {
            if self.stopped() {
                return;
            }
            if self.rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Drive a provider stream, forwarding `TextDelta` events to the gRPC
/// UX channel (`tx`) and returning the final neutral `AssistantTurn`
/// when the stream completes. Synthesizes a gRPC `Done` from the
/// terminal `Complete(turn)` event so the TUI's typing animation
/// behavior matches pre-refactor.
///
/// Returns `Ok(None)` when the stream ended without a `Complete` event
/// (e.g. connection dropped or fatal error mid-stream).
pub(super) async fn collect_response(
    mut brain_rx: BoxStream<'static, StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    config_name: &str,
    mut stop: Option<&mut StopCheck>,
) -> anyhow::Result<Option<AssistantTurn>> {
    activity::emit(activity::Event::ModelCallStarted);
    let mut first_token = true;
    let mut full_turn: Option<AssistantTurn> = None;
    let mut accum_text = String::new();

    loop {
        // Stop-aware receive: the select! wakes on the operator-stop watch
        // even while the provider is silent. On stop, returning drops
        // `brain_rx` — the provider pumps exit on their next send (the
        // receiver-gone contract) and the severed connection stops a local
        // model's generation. The partial becomes a synthetic
        // OperatorStop turn so the driver's terminal path (notice +
        // persistence marker + Done) runs normally.
        let event = match stop.as_deref_mut() {
            Some(check) => {
                tokio::select! {
                    ev = brain_rx.next() => ev,
                    _ = check.triggered() => {
                        activity::emit(activity::Event::ModelCallEnded { error: false });
                        let mut content = Vec::new();
                        if !accum_text.is_empty() {
                            content.push(Block::Text(std::mem::take(&mut accum_text)));
                        }
                        return Ok(Some(AssistantTurn {
                            content,
                            outcome: TurnOutcome::EarlyStop(EarlyStopReason::OperatorStop),
                            stop_details: None,
                        }));
                    }
                }
            }
            None => brain_rx.next().await,
        };
        let Some(event) = event else { break };
        match event {
            StreamEvent::TextDelta(text) => {
                activity::emit(activity::Event::TextDelta {
                    chars: u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
                });
                if first_token {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Thinking(
                                ThinkingState {
                                    is_thinking: false,
                                    name: String::new(),
                                    current_tool: String::new(),
                                },
                            )),
                        }))
                        .await;
                    first_token = false;
                }
                accum_text.push_str(&text);
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Token(
                            StreamToken { text },
                        )),
                    }))
                    .await;
            }
            StreamEvent::Error(err) => {
                error!(
                    target: "dispatch",
                    config = %config_name,
                    "Brain stream error: {err}"
                );
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("Brain error: {err}"),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
            }
            StreamEvent::BlockComplete => {}
            StreamEvent::ReasoningDelta(text) => {
                activity::emit(activity::Event::ReasoningDelta {
                    chars: u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
                });
                // Forward to the expression panel via the dedicated
                // ReasoningDelta proto frame. CRITICAL: must NOT append
                // to `accum_text` (would persist to session history via
                // turn_text fallback) and must NOT toggle `is_thinking:
                // false` (the typing indicator stays gated on real text
                // deltas, not reasoning).
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
            StreamEvent::Complete(turn) => {
                // Synthesize a gRPC Done from the assembled turn so the
                // TUI gets a terminal "full text" frame for its
                // streaming animation, matching pre-refactor behavior.
                let full_text = if accum_text.is_empty() {
                    turn_text(&turn)
                } else {
                    accum_text.clone()
                };
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Done(
                            StreamDone {
                                full_response: full_text,
                            },
                        )),
                    }))
                    .await;
                full_turn = Some(turn);
            }
        }
    }

    // A stream that ended without `Complete` is a failed call: a provider
    // error, or the connection gone.
    activity::emit(activity::Event::ModelCallEnded { error: full_turn.is_none() });
    Ok(full_turn)
}
