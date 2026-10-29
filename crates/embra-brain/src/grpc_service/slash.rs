//! The slash commands: `handle_slash_command` and the handlers it dispatches to.

use super::*;

pub(super) async fn handle_slash_command(
    command: &str,
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    ctx: &ServiceCtx,
    config_tz: &str,
    _api_key: &str,
) -> Option<String> {
    let ServiceCtx { db, session_mgr, pending_openai_compat_setup, pending_session_delete, .. } = ctx;
    // Helper to send a system message
    let send_msg = |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, content: String| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage { content, msg_type: SystemMessageType::Info as i32 }
                )),
            })).await;
        }
    };

    // Helper to send a ModeTransition with updated session name.
    // Sprint 4: includes the active model in the message so the
    // console status bar can render it without a separate event.
    let tz = config_tz.to_string();
    let send_session_update = {
        move |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, session_name: &str| {
            let tx = tx.clone();
            let session = session_name.to_string();
            let tz = tz.clone();
            async move {
                // The configuration is read here, on `/new` and `/switch`
                // alone, the two commands that send this frame; every other
                // command used to pay the read for nothing (#38).
                let cfg_loaded = config::load_config(db).await.ok();
                let config_name = cfg_loaded
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "Embra".to_string());
                let active_provider = cfg_loaded
                    .as_ref()
                    .map(|c| c.api_provider.clone())
                    .unwrap_or_else(|| "anthropic".to_string());
                let model = match cfg_loaded.as_ref() {
                    Some(c) => display_model_for(&active_provider, c),
                    None => crate::provider::anthropic::DEFAULT_DISPLAY_NAME.to_string(),
                };
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::ModeChange(
                        ModeTransition {
                            from_mode: OperatingMode::Operational as i32,
                            to_mode: OperatingMode::Operational as i32,
                            message: operational_status_line(&config_name, &session, &tz, &model),
                        }
                    )),
                })).await;
            }
        }
    };

    match command {
        "/help" => {
            send_msg(tx, "Available commands:\n  /sessions, /switch <name>, /new <name>, /close\n  /sessions delete <name>            Guided delete: summary + reason + memories, then soft delete (7-day grace)\n  /sessions restore <name>           Undo a soft delete during its grace period\n  /stop                              Stop a stuck in-flight turn (console: Esc; mobile: the \u{25a0} button)\n  /status, /soul, /identity, /mode\n  /provider                          Show active provider, model, session\n  /provider <anthropic|gemini|ollama|lm_studio>  Switch provider for future turns\n  /provider --setup <anthropic|gemini>  Add/replace an API key, then pick the model (multi-turn)\n  /provider --setup <ollama|lm_studio>  Reconfigure endpoint, bearer, and model (multi-turn)\n  /model                             Show the model; on Ollama / LM Studio, pick one from the models the server lists\n  /model <name>                      Switch the model (next message): opus-5.5|opus-5|opus-4.8|fable-5.1|fable-5 on Anthropic, gemini-3.1-pro|gemini-4-argon on Gemini, a listed model on Ollama / LM Studio\n  /effort                            Show the active provider's effort level and what is sent\n  /effort <low|medium|high|xhigh|max>  Set effort for the active provider (Anthropic default max; Gemini high; local presets: sent verbatim, model-validated); /effort reset clears\n  /local-ai-gateway                  Show WSDS Local AI Gateway's URL and whether requests go through it\n  /local-ai-gateway <url>|on|off|reset  Route every provider request through its unified listener (http(s)://host:4000, which must answer with its x-local-ai-gateway header); off keeps the URL, reset forgets it\n  /embeddings                        Show the local semantic-similarity layer: model, index, how many nodes are embedded\n  /embeddings <on|off>               Enable or disable semantic similarity in retrieval (default on)\n  /embeddings backfill [--force]     Embed nodes that need it (local CPU, ~55ms/node); --force re-embeds all\n  /iter-cap                          Show the per-turn tool iteration cap\n  /iter-cap <N>                      Set the cap (1..=1000, default 100)\n  /iter-cap reset                    Restore the default cap\n  /show-reasoning                    Show whether reasoning streams to the panel\n  /show-reasoning <on|off>           Toggle live reasoning in the expression panel (default on)\n  /attach <id|path>                  Attach an image or a text file (uploaded id, uploads/ path or any workspace path) to your next message\n  /attach list | clear               Show or drop the staged attachments\n  /download <path>                   Hand a workspace file to your browser as a download (regular files up to 12 MiB)\n  /image-provider                    Show the image-generation backend, model, and key status\n  /image-provider gemini             Use Gemini image models for image_generate\n  /image-provider model <id>         gemini-3-pro-image (default) | gemini-3.1-flash-image | gemini-3.1-flash-lite-image | gemini-2.5-flash-image\n  /image-provider key <token>        Set a dedicated image-generation key (STATE, 0600); `key remove` deletes it\n  /github-token <token>              Set GitHub token\n  /git-token <host> <token>          Set a token for a self-hosted git server (remove: /git-token <host> remove)\n  /ssh-keygen                        Generate the SSH key pair (its comment: the instance's name at embraos), or show the public key\n  /ssh-keygen remove|rotate          Delete the key pair after showing its public key and fingerprint, or replace it with a new one\n  /ssh-copy-id <user@host>           Copy SSH key to host\n  /git-setup <name> | <email>        Set git user config\n  /guardian-define                   Paste a Rust module to define a dynamic tool\n  /guardian list|status <name>|show <name>|delete <name>  Manage dynamic tools\n  /guardian approve <name>|reject <name>  Approve/reject a brain-proposed tool (replicant-checked)\n  /guardian rebuild <name>|--all     Rebuild tools an OS update left unloaded, or failed builds (validated + replicant-checked again)\n  /guardian key brave <token>        Set the Brave Search API key (enables web_search tools)\n  /guardian secret <host> <header> <value>  Store a credential the host adds to every http_request to that host (never in a tool's input); `secret` lists, `<host> remove` forgets\n  /guardian egress allow|deny <host>   The allowlist of private hosts the tools may reach (loopback is refused always); `egress` lists\n  /feedback-loop                     (EXPERIMENTAL) trigger Phase 3 feedback-loop protocol\n  /help".to_string()).await;
        }
        "/feedback-loop" => {
            send_msg(tx, "\u{26A0} EXPERIMENTAL: Phase 3 Continuity Engine preview (manual trigger)\nInitiating feedback loop per feedback-loop-spec-v2.md.\nThe Brain will now begin Step 1.1 (Gather \u{2192} Introspect).\nThis is a multi-turn protocol \u{2014} expect 5+ tool invocations.".to_string()).await;
            return Some(build_feedback_loop_prompt());
        }
        "/provider" => {
            handle_provider_command(args, tx, ctx).await;
        }
        "/iter-cap" => {
            handle_iter_cap_command(args, tx, db).await;
        }
        "/show-reasoning" => {
            handle_show_reasoning_command(args, tx, db).await;
        }
        "/local-ai-gateway" => {
            handle_gateway_command(args, tx, db).await;
        }
        "/model" => {
            handle_model_command(args, tx, db, session_mgr, pending_openai_compat_setup).await;
        }
        "/effort" => {
            handle_effort_command(args, tx, db).await;
        }
        "/attach" => {
            handle_attach_command(args, tx, db, session_mgr).await;
        }
        "/download" => {
            handle_download_command(args, tx).await;
        }
        "/embeddings" => { handle_embeddings_command(args, tx, db).await; }
        "/image-provider" => {
            handle_image_provider_command(args, tx, db).await;
        }
        "/guardian" => {
            // Operator affordance (define/list/status/show/delete/key).
            // The body for `define` is `define\n<module>` from the
            // console's /guardian-define capture mode. Reports via
            // SystemMessage; never feeds a synthetic model turn.
            let msg = crate::guardian::handle_guardian_slash(args, db).await;
            send_msg(tx, msg).await;
        }
        "/status" => {
            let status = tools::system_status(db).await;
            send_msg(tx, serde_json::to_string_pretty(&status).unwrap_or_else(|_| "Failed to get status".to_string())).await;
        }
        "/stop" => {
            // In-band /stop can only ever be READ when no turn is running
            // (the Converse loop is parked during a turn — that's why the
            // real stop path is the out-of-band StopTurn unary the clients
            // fire). Reaching here means there is nothing to stop.
            send_msg(tx, "No turn in flight — nothing to stop. (While a turn is running, Esc on the console or the ■ button on mobile stops it.)".to_string()).await;
        }
        "/sessions" => {
            // Subcommand parse (the /guardian split_once pattern): the
            // client sends the full remainder as one args string.
            let (sub, rest) = match args.trim().split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args.trim(), ""),
            };
            match sub {
                "" => {
                    let mgr = session_mgr.read().await;
                    let is_sealed = learning::is_soul_sealed(db).await.unwrap_or(false);
                    // Configured timezone for the last-active render — fresh
                    // config wins over the threaded boot value (the SessionAttach
                    // ModeChange pattern), so a runtime timezone change shows
                    // immediately.
                    let cfg = config::load_config(db).await.ok();
                    let display_tz = cfg
                        .as_ref()
                        .map(|c| c.timezone.clone())
                        .unwrap_or_else(|| config_tz.to_string());
                    let output = match mgr.list_with_counts().await {
                        Ok(sessions) => {
                            let (deleted, live): (Vec<_>, Vec<_>) = sessions
                                .into_iter()
                                .partition(|s| s.state == SessionState::Deleted);
                            let mut out = if live.is_empty() {
                                "No sessions.".to_string()
                            } else {
                                live.iter()
                                    .map(|s| session_list_line(s, is_sealed, &display_tz))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            };
                            // Without the config the active provider is not
                            // known, and the footer says nothing.
                            if let Some(footer) = cfg.as_ref().and_then(|c| {
                                other_provider_footer(&live, &c.api_provider, mgr.active_session.is_some())
                            }) {
                                out.push('\n');
                                out.push_str(&footer);
                            }
                            if !deleted.is_empty() {
                                let names = deleted
                                    .iter()
                                    .map(|s| s.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                out.push_str(&format!(
                                    "\n  {} deleted session{} in grace period ({}) — /sessions restore <name> to recover",
                                    deleted.len(),
                                    if deleted.len() == 1 { "" } else { "s" },
                                    names,
                                ));
                            }
                            out
                        }
                        Err(e) => format!("Error listing sessions: {}", e),
                    };
                    send_msg(tx, output).await;
                }
                "delete" => {
                    if rest.is_empty() {
                        send_msg(tx, "Usage: /sessions delete <name>".to_string()).await;
                        return None;
                    }
                    // Never deletable: it is the record of how the sealed
                    // identity came to be. Refused before the flow starts.
                    if rest == "learning" {
                        send_msg(tx, "The learning session is the sealed identity record and cannot be deleted.".to_string()).await;
                        return None;
                    }
                    let mgr = session_mgr.read().await;
                    let meta = mgr.get_meta(rest).await.ok().flatten();
                    let Some(meta) = meta else {
                        drop(mgr);
                        send_msg(tx, format!("Session '{}' does not exist", rest)).await;
                        return None;
                    };
                    if meta.state == SessionState::Deleted {
                        drop(mgr);
                        send_msg(tx, format!(
                            "Session '{}' is already deleted (grace period) — /sessions restore {} to recover it.",
                            rest, rest
                        )).await;
                        return None;
                    }
                    if mgr.active_session.is_none() {
                        drop(mgr);
                        send_msg(tx, "The delete flow runs as a conversation — attach a session first (/switch <name> or /new <name>).".to_string()).await;
                        return None;
                    }
                    drop(mgr);
                    // Arm the flow, announce, and dispatch the kickoff
                    // synthetic turn via the Some(prompt) convention. The
                    // UserMessage intercept advances AwaitingKickoff →
                    // AwaitingReason when that synthetic arrives, so the
                    // kickoff itself is never mistaken for the reason.
                    *pending_session_delete.lock().await = Some(PendingSessionDelete {
                        name: rest.to_string(),
                        phase: DeleteFlowPhase::AwaitingKickoff,
                    });
                    send_msg(tx, format!(
                        "Starting guided deletion of session '{}' — summarizing it first. (Any slash command cancels.)",
                        rest
                    )).await;
                    return Some(build_delete_kickoff_prompt(rest));
                }
                "restore" => {
                    if rest.is_empty() {
                        send_msg(tx, "Usage: /sessions restore <name>".to_string()).await;
                        return None;
                    }
                    let store = session_mgr.read().await.store();
                    let meta = store.get_meta(rest).await.ok().flatten();
                    match meta {
                        None => {
                            send_msg(tx, format!(
                                "Session '{}' does not exist (or its grace period already expired).",
                                rest
                            )).await;
                        }
                        Some(m) if m.state != SessionState::Deleted => {
                            send_msg(tx, format!("Session '{}' isn't deleted.", rest)).await;
                        }
                        Some(_) => match store.restore(rest).await {
                            Ok(()) => {
                                send_msg(tx, format!(
                                    "Session '{}' restored (Detached) — /switch {} to attach.",
                                    rest, rest
                                )).await;
                            }
                            Err(e) => {
                                send_msg(tx, format!("Restore of '{}' failed: {}", rest, e)).await;
                            }
                        },
                    }
                }
                other => {
                    send_msg(tx, format!(
                        "Unknown /sessions subcommand '{}'. Usage: /sessions [delete <name>|restore <name>]",
                        other
                    )).await;
                }
            }
        }
        "/new" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /new <session-name>".to_string()).await;
            } else {
                let store = session_mgr.read().await.store();
                // Duplicate names are refused: create() would append a
                // SECOND meta doc to the existing collection — shadowed by
                // the canonical-first read, visible only as a listing
                // ghost, and a soft-delete survivor. A soft-deleted name
                // stays reserved through its grace period (restore is the
                // recovery path). create() carries the same guard as
                // defense-in-depth; this pre-check exists for the tailored
                // messages.
                if let Some(existing) = store.get_meta(args).await.ok().flatten() {
                    let msg = if existing.state == SessionState::Deleted {
                        format!(
                            "A deleted session '{}' is in its grace period — /sessions restore {} to recover it, or pick another name.",
                            args, args
                        )
                    } else {
                        format!(
                            "Session '{}' already exists — /switch {} to attach.",
                            args, args
                        )
                    };
                    send_msg(tx, msg).await;
                    return None;
                }
                match store.create(args).await {
                    Ok(s) => {
                        session_mgr.write().await.active_session = Some(s.name.clone());
                        let name = s.name.clone();
                        // A new session is an empty transcript: the clients
                        // clear what they show before the confirmation.
                        send_transcript_reset(tx, &name).await;
                        send_msg(tx, format!("Created and switched to session '{}'", name)).await;
                        send_session_update(tx, &name).await;
                    }
                    Err(e) => { send_msg(tx, format!("Error creating session: {}", e)).await; }
                }
            }
        }
        "/switch" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /switch <session-name>".to_string()).await;
            } else if args == "learning" {
                send_msg(tx, "Learning session is read-only. Use /soul to view the sealed soul document.".to_string()).await;
            } else {
                // The round trips run on the store handle, the manager
                // written for a blink once the history is in hand (#59).
                let store = session_mgr.read().await.store();
                let exists = match store.session_exists(args).await {
                    Ok(exists) => exists,
                    Err(e) => {
                        send_msg(tx, format!("Could not check session '{}': {}. Nothing was switched; try again.", args, e)).await;
                        return None;
                    }
                };
                if exists {
                    // Soft-deleted sessions are unattachable during their
                    // grace period — reattach would silently resurrect a
                    // half-stamped session; restore is the sanctioned path.
                    let meta = store.get_meta(args).await.ok().flatten();
                    let is_deleted = meta.as_ref().is_some_and(|m| m.state == SessionState::Deleted);
                    // The start of the absence, read before `reattach`
                    // stamps `last_active` with now.
                    let away_since = meta.map(|m| m.last_active);
                    if is_deleted {
                        send_msg(tx, format!(
                            "Session '{}' is deleted (grace period) — /sessions restore {} first.",
                            args, args
                        )).await;
                        return None;
                    }
                    // Cross-provider guard — same rule as SessionAttach.
                    if let Err((session_provider, active_provider)) =
                        check_session_provider(db, args).await
                    {
                        let active_session = session_mgr.read().await.active_session.clone();
                        send_msg(
                            tx,
                            cross_provider_block_message(
                                args,
                                &session_provider,
                                &active_provider,
                                active_session.as_deref(),
                            ),
                        )
                        .await;
                        return None;
                    }
                    // One load: `reattach` marks the session active in the
                    // store and returns its history (the second
                    // `load_history` that followed read the same document
                    // again); the manager's active session moves once the
                    // history is in hand. A reattach that fails is said and
                    // leaves the active session as it was (#59): never
                    // "Switched" over a history that could not be read.
                    let history_len: usize = match store.reattach(args).await {
                        Ok(history) => {
                            session_mgr.write().await.active_session = Some(args.to_string());
                            replay_history(tx, args, &history).await;
                            send_msg(tx, format!("Switched to session '{}' ({} messages)", args, history.len())).await;
                            history.len()
                        }
                        Err(e) => {
                            send_system(
                                tx,
                                format!("Could not switch to session '{}': {}. The active session is unchanged.", args, e),
                                SystemMessageType::Error,
                            )
                            .await;
                            return None;
                        }
                    };
                    send_session_update(tx, args).await;

                    // Resume-briefing trigger — same gate as SessionAttach:
                    // existing session with prior history, Operational mode
                    // (soul sealed). Set the flag and signal a synthetic
                    // UserMessage dispatch via this function's Some(prompt)
                    // return convention; the outer SlashCommand handler at
                    // ~grpc_service.rs:1292-1304 will `Box::pin(handle_request(...))`
                    // it through, the UserMessage handler will read-and-clear
                    // the flag and substitute the `<session_resumption>`
                    // wrapper. Raw `[Session resumed]` is what persists.
                    // Unlike SessionAttach, /switch briefs unconditionally —
                    // it is an explicit user action, not a transport reconnect.
                    // It still records the attempt so a WS re-attach seconds
                    // later (while this briefing may still be in flight, with
                    // last_active not yet bumped) is cooldown-suppressed.
                    let is_sealed = learning::is_soul_sealed(db).await.unwrap_or(false);
                    if is_sealed && history_len > 0 {
                        let mut mgr = session_mgr.write().await;
                        mgr.record_briefing_attempt(args);
                        mgr.pending_resume_briefing =
                            Some(crate::sessions::ResumeBriefing { away_since });
                        drop(mgr);
                        return Some("[Session resumed]".to_string());
                    }
                } else {
                    send_msg(tx, format!("Session '{}' does not exist", args)).await;
                }
            }
        }
        "/close" => {
            let (store, active) = {
                let mgr = session_mgr.read().await;
                (mgr.store(), mgr.active_session.clone())
            };
            if let Some(name) = active {
                let _ = store.close(&name).await;
                session_mgr.write().await.clear_active(&name);
                send_msg(tx, format!("Closed session '{}'", name)).await;
            } else {
                send_msg(tx, "No active session".to_string()).await;
            }
        }
        "/soul" => {
            let output = match learning::load_soul(db).await {
                Ok(Some(soul)) => {
                    if crate::identity_graph::is_graph_soul(&soul) {
                        // Graph mode: grouped prose + the seal header
                        // (sealed_at / sha256 from the envelope) instead of
                        // a ~46 KB raw JSON dump.
                        let header = match db.read("soul.invariant", "soul").await {
                            Ok(doc) => format!(
                                "Sealed identity graph — sealed_at: {} — sha256: {}\n\n",
                                doc.get("sealed_at").and_then(|v| v.as_str()).unwrap_or("?"),
                                doc.get("sha256").and_then(|v| v.as_str()).unwrap_or("?"),
                            ),
                            Err(_) => String::new(),
                        };
                        format!("{header}{}", crate::brain::render_sealed_graph(&soul))
                    } else {
                        serde_json::to_string_pretty(&soul).unwrap_or_default()
                    }
                }
                Ok(None) => "No soul sealed yet.".to_string(),
                Err(e) => format!("Error loading soul: {}", e),
            };
            send_msg(tx, output).await;
        }
        "/identity" => {
            let output = match db.read("memory.identity", "identity").await {
                Ok(doc) => serde_json::to_string_pretty(&doc).unwrap_or_default(),
                Err(_) => {
                    // Graph mode (imported instances write no identity
                    // doc): identity lives in the sealed graph.
                    match learning::load_soul(db).await {
                        Ok(Some(soul)) if crate::identity_graph::is_graph_soul(&soul) => {
                            format!(
                                "Identity is part of the sealed identity graph:\n\n{}",
                                crate::brain::render_sealed_graph(&soul)
                            )
                        }
                        _ => "No identity document found.".to_string(),
                    }
                }
            };
            send_msg(tx, output).await;
        }
        "/mode" => {
            let sealed = learning::is_soul_sealed(db).await.unwrap_or(false);
            send_msg(tx, if sealed { "Operational (soul sealed)".to_string() } else { "Learning (soul not sealed)".to_string() }).await;
        }
        "/github-token" => {
            if args.is_empty() {
                let has_token = tools::engineering::resolve_github_token(db).await.is_some();
                if has_token {
                    send_msg(tx, "GitHub token is configured. Use /github-token <token> to update it.".to_string()).await;
                } else {
                    send_msg(tx, "Usage: /github-token <your-github-token>\nSets GITHUB_TOKEN for GitHub API access (issues, PRs, clone).".to_string()).await;
                }
            } else {
                let token = args.trim().to_string();
                if !token.starts_with("ghp_") && !token.starts_with("gho_") && !token.starts_with("github_pat_") {
                    send_msg(tx, "Warning: token doesn't look like a GitHub token (expected ghp_/gho_/github_pat_ prefix). Saving anyway.".to_string()).await;
                }
                // Save to WardSONDB config.system
                match config::load_config(db).await {
                    Ok(mut cfg) => {
                        cfg.github_token = Some(token.clone());
                        if let Err(e) = config::save_config(db, &cfg).await {
                            send_msg(tx, format!("Failed to save token: {}", e)).await;
                            return None;
                        }
                    }
                    Err(_) => {
                        send_msg(tx, "No system config found. Run config wizard first.".to_string()).await;
                        return None;
                    }
                }
                // Persist to STATE partition for boot propagation
                let path = "/embra/state/github_token";
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, &token);
                // Set env var for immediate availability
                // SAFETY: single-threaded access to env at this point in the slash command handler
                unsafe { std::env::set_var("GITHUB_TOKEN", &token); }
                send_msg(tx, "GitHub token saved. GitHub tools (gh_issues, gh_prs, git_clone, etc.) are now active.".to_string()).await;
            }
        }
        "/git-token" => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            match parts.as_slice() {
                [] => {
                    let tokens = tools::engineering::resolve_git_tokens(db).await;
                    if tokens.is_empty() {
                        send_msg(tx, "No per-host git tokens configured.\nUsage: /git-token <host> <token>   (e.g. /git-token gitlab.example.com glpat-...)\n       /git-token <host> remove\nFor github.com use /github-token.".to_string()).await;
                    } else {
                        let hosts: Vec<&str> = tokens.keys().map(|h| h.as_str()).collect();
                        send_msg(tx, format!(
                            "Per-host git tokens configured for: {}\nUse /git-token <host> <token> to update, /git-token <host> remove to delete.",
                            hosts.join(", ")
                        )).await;
                    }
                }
                [host, token_or_remove] => {
                    let host = match tools::engineering::normalize_git_host(host) {
                        Ok(h) => h,
                        Err(e) => {
                            send_msg(tx, format!("Invalid host: {}", e)).await;
                            return None;
                        }
                    };
                    if host == "github.com" {
                        send_msg(tx, "Use /github-token for github.com (it uses GitHub's x-access-token convention).".to_string()).await;
                        return None;
                    }
                    let remove = *token_or_remove == "remove";
                    match config::load_config(db).await {
                        Ok(mut cfg) => {
                            let mut tokens = cfg.git_tokens.take().unwrap_or_default();
                            let msg = if remove {
                                if tokens.remove(&host).is_some() {
                                    format!("Git token for {} removed.", host)
                                } else {
                                    format!("No git token configured for {}.", host)
                                }
                            } else {
                                tokens.insert(host.clone(), token_or_remove.to_string());
                                format!("Git token for {} saved. git_clone/git_push/git_pull now authenticate to it (oauth2 over HTTPS).", host)
                            };
                            cfg.git_tokens = if tokens.is_empty() { None } else { Some(tokens) };
                            if let Err(e) = config::save_config(db, &cfg).await {
                                send_msg(tx, format!("Failed to save token: {}", e)).await;
                                return None;
                            }
                            send_msg(tx, msg).await;
                        }
                        Err(_) => {
                            send_msg(tx, "No system config found. Run config wizard first.".to_string()).await;
                        }
                    }
                }
                _ => {
                    send_msg(tx, "Usage: /git-token <host> <token>  or  /git-token <host> remove".to_string()).await;
                }
            }
        }
        "/ssh-keygen" => {
            use crate::tools::security::{
                close_ssh_session_if_open, remove_ssh_key_pair, ssh_keygen_action,
                ssh_pub_key_fingerprint, SshKeygenAction, SSH_KEY_PATH,
            };
            let action = match ssh_keygen_action(args) {
                Ok(action) => action,
                Err(usage) => {
                    send_msg(tx, usage).await;
                    return None;
                }
            };
            let key_path = std::path::Path::new(SSH_KEY_PATH);
            let pub_path = format!("{SSH_KEY_PATH}.pub");
            let mut parts: Vec<String> = Vec::new();
            if action == SshKeygenAction::ShowOrGenerate {
                if key_path.exists() {
                    match std::fs::read_to_string(&pub_path) {
                        Ok(pubkey) => {
                            send_msg(tx, format!(
                                "SSH key already exists. Public key:\n\n{}\n\nAdd this to ~/.ssh/authorized_keys on your target hosts. /ssh-keygen rotate replaces it; /ssh-keygen remove deletes it.",
                                pubkey.trim()
                            )).await;
                        }
                        Err(_) => {
                            send_msg(tx, "SSH private key exists but public key is missing. /ssh-keygen rotate replaces the pair.".to_string()).await;
                        }
                    }
                    return None;
                }
            } else {
                // The session first: a ControlMaster that authenticated
                // with the key would keep serving without the file.
                let closed = close_ssh_session_if_open().await;
                match remove_ssh_key_pair(key_path) {
                    Ok(Some(old)) => {
                        let fingerprint = ssh_pub_key_fingerprint(&old)
                            .unwrap_or_else(|| "fingerprint unavailable".to_string());
                        parts.push(format!("SSH key removed ({fingerprint})."));
                        if old.is_empty() {
                            parts.push(
                                "Its public key file was already gone, so there is no line to show."
                                    .to_string(),
                            );
                        } else {
                            parts.push(format!(
                                "Its public key was:\n\n{old}\n\nRemove that line from ~/.ssh/authorized_keys on every host that trusted it; the OS cannot do that for you."
                            ));
                        }
                        if let Some(note) = closed {
                            parts.push(note);
                        }
                    }
                    Ok(None) => {
                        if action == SshKeygenAction::Remove {
                            send_msg(tx, "No SSH key to remove.".to_string()).await;
                            return None;
                        }
                        parts.push("No SSH key to remove.".to_string());
                    }
                    Err(e) => {
                        let word = if action == SshKeygenAction::Remove { "remove" } else { "rotate" };
                        send_msg(tx, format!("/ssh-keygen {word}: {e}")).await;
                        return None;
                    }
                }
                if action == SshKeygenAction::Remove {
                    parts.push("The ssh tools have no key until /ssh-keygen generates one.".to_string());
                    send_msg(tx, parts.join("\n\n")).await;
                    return None;
                }
            }
            match generate_ssh_key(db).await {
                Ok(pubkey) => {
                    let (head, tail) = if action == SshKeygenAction::Rotate {
                        ("New SSH key generated. Public key:", ", replacing the old line")
                    } else {
                        ("SSH key generated. Public key:", "")
                    };
                    parts.push(format!(
                        "{head}\n\n{pubkey}\n\nAdd this to ~/.ssh/authorized_keys on your target hosts{tail}."
                    ));
                }
                Err(e) => parts.push(e),
            }
            send_msg(tx, parts.join("\n\n")).await;
        }
        "/ssh-copy-id" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /ssh-copy-id <user@host>\nCopies SSH public key to remote host (RFC 1918 only).".to_string()).await;
                return None;
            }

            let pub_path = "/embra/workspace/.ssh/id_ed25519.pub";
            if !std::path::Path::new(pub_path).exists() {
                send_msg(tx, "No SSH key found. Run /ssh-keygen first.".to_string()).await;
                return None;
            }

            let target = args.trim();
            let host = target.rsplit('@').next().unwrap_or(target);
            if !tools::security::is_private_address(host) {
                send_msg(tx, format!(
                    "Denied: '{}' is not a private address. SSH is restricted to RFC 1918 ranges.",
                    host
                )).await;
                return None;
            }

            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                crate::tools::subprocess("ssh-copy-id")
                    .args([
                        "-i", pub_path,
                        "-o", "StrictHostKeyChecking=accept-new",
                        "-o", "UserKnownHostsFile=/embra/workspace/.ssh/known_hosts",
                        target,
                    ])
                    .output(),
            ).await {
                Ok(Ok(out)) if out.status.success() => {
                    send_msg(tx, format!("SSH public key copied to {}.", target)).await;
                }
                Ok(Ok(out)) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    send_msg(tx, format!(
                        "ssh-copy-id failed (password auth may be required or key already present):\n{}",
                        stderr.trim()
                    )).await;
                }
                Ok(Err(e)) => {
                    send_msg(tx, format!("Failed to run ssh-copy-id: {}", e)).await;
                }
                Err(_) => {
                    send_msg(tx, "ssh-copy-id timed out after 30 seconds.".to_string()).await;
                }
            }
        }
        "/git-setup" => {
            if args.is_empty() {
                match crate::tools::engineering::git::identity().await {
                    Ok((name, email)) => {
                        let shown = |v: String| if v.is_empty() { "(not set)".to_string() } else { v };
                        send_msg(tx, format!(
                            "Git config:\n  user.name:  {}\n  user.email: {}\n\nUsage: /git-setup <name> | <email>",
                            shown(name),
                            shown(email),
                        )).await;
                    }
                    Err(e) => send_msg(tx, format!("Git config could not be read: {e}")).await,
                }
            } else {
                let parts: Vec<&str> = args.splitn(2, " | ").collect();
                if parts.len() < 2 {
                    send_msg(tx, "Usage: /git-setup <name> | <email>".to_string()).await;
                    return None;
                }
                let name = parts[0].trim();
                let email = parts[1].trim();
                // The result is reported as it is: a failed write is said,
                // never "updated" (William's live check of 2026-10-23 found
                // no trace of the command either; the change is logged).
                match crate::tools::engineering::git::set_identity(name, email).await {
                    Ok(()) => {
                        info!("git identity set by the operator: user.name='{name}', user.email='{email}'");
                        send_msg(tx, format!("Git config updated: user.name='{}', user.email='{}'", name, email)).await;
                    }
                    Err(e) => {
                        warn!("git identity not written: {e}");
                        send_msg(tx, format!("Git config not updated: {e}")).await;
                    }
                }
            }
        }
        _ => {
            send_msg(tx, format!("Unknown command: {}. Type /help for available commands.", command)).await;
        }
    }
    None
}

/// `/iter-cap [<N> | reset]` — show, set, or reset the per-turn tool
/// iteration cap. Persisted via `SystemConfig.max_tool_iterations`;
/// the loop driver reads `loaded_config.max_tool_iterations` fresh
/// each turn so changes take effect on the next user message
/// (Embra_Debug #80 follow-up).
pub(super) async fn handle_iter_cap_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
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

    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send_msg(
                format!("/iter-cap: failed to load config: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    let trimmed = args.trim();

    // No args → status
    if trimmed.is_empty() {
        let current = cfg
            .max_tool_iterations
            .unwrap_or(DEFAULT_MAX_TOOL_ITERATIONS);
        let suffix = if cfg.max_tool_iterations.is_some() {
            "set"
        } else {
            "default"
        };
        send_msg(
            format!(
                "Tool iteration cap: {current} ({suffix}). Default {DEFAULT_MAX_TOOL_ITERATIONS}. \
                 Use `/iter-cap <N>` (1..=1000) to change, `/iter-cap reset` to restore default."
            ),
            SystemMessageType::Info,
        )
        .await;
        return;
    }

    // Reset / default → clear the override
    if trimmed.eq_ignore_ascii_case("reset") || trimmed.eq_ignore_ascii_case("default") {
        cfg.max_tool_iterations = None;
        if let Err(e) = config::save_config(db, &cfg).await {
            send_msg(
                format!("/iter-cap: failed to save: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
        send_msg(
            format!(
                "Tool iteration cap reset to default ({DEFAULT_MAX_TOOL_ITERATIONS}). \
                 Takes effect on the next user message."
            ),
            SystemMessageType::Info,
        )
        .await;
        return;
    }

    // Numeric set
    match parse_iter_cap_value(trimmed) {
        Ok(n) => {
            cfg.max_tool_iterations = Some(n);
            if let Err(e) = config::save_config(db, &cfg).await {
                send_msg(
                    format!("/iter-cap: failed to save: {e}"),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }
            send_msg(
                format!(
                    "Tool iteration cap set to {n}. Takes effect on the next user message."
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        Err(msg) => {
            let current = cfg
                .max_tool_iterations
                .unwrap_or(DEFAULT_MAX_TOOL_ITERATIONS);
            send_msg(
                format!(
                    "/iter-cap: {msg}. Usage: `/iter-cap [<N> | reset]` (current: {current})."
                ),
                SystemMessageType::Error,
            )
            .await;
        }
    }
}

/// `/show-reasoning [on|off]` — show, set, or reset whether reasoning
/// streams to the expression panel. Persisted via
/// `SystemConfig.show_reasoning`; the loop driver reads it fresh each
/// turn through `LlmRequestOptions.include_reasoning`, so changes
/// take effect on the next user message. Default-on with opt-out:
/// unset (`None`) and `Some(true)` both mean *show*; `Some(false)`
/// suppresses panel reasoning and strips the request-body opt-ins so
/// providers don't spend tokens on summaries.
pub(super) async fn handle_show_reasoning_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
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

    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send_msg(
                format!("/show-reasoning: failed to load config: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    let trimmed = args.trim();

    if trimmed.is_empty() {
        let on = cfg.show_reasoning();
        let suffix = match cfg.show_reasoning {
            Some(true) => "set on",
            Some(false) => "set off",
            None => "default on",
        };
        send_msg(
            format!(
                "Live reasoning to expression panel: {} ({}). \
                 Use `/show-reasoning on` or `/show-reasoning off` to change. \
                 When off, providers omit reasoning from request bodies (no token cost).",
                if on { "on" } else { "off" },
                suffix,
            ),
            SystemMessageType::Info,
        )
        .await;
        return;
    }

    let new_value = match trimmed.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Some(true),
        "off" | "false" | "0" | "no" => Some(false),
        "reset" | "default" => None,
        _ => {
            send_msg(
                format!(
                    "/show-reasoning: '{trimmed}' is not a valid value. Use `on`, `off`, or `reset`."
                ),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    cfg.show_reasoning = new_value;
    if let Err(e) = config::save_config(db, &cfg).await {
        send_msg(
            format!("/show-reasoning: failed to save: {e}"),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    let label = match new_value {
        Some(true) => "on (explicit)",
        Some(false) => "off",
        None => "default (on)",
    };
    send_msg(
        format!(
            "Live reasoning panel set to {label}. Takes effect on the next user message."
        ),
        SystemMessageType::Info,
    )
    .await;
}

/// Parse a positive integer in the inclusive range 1..=1000. Pure
/// free-fn so the parser can be unit-tested without spinning up a
/// gRPC channel.
pub(super) fn parse_iter_cap_value(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("'{s}' is not a positive integer"))?;
    if !(1..=1000).contains(&n) {
        return Err(format!("value {n} out of range (expected 1..=1000)"));
    }
    Ok(n)
}

/// `/ssh-keygen`'s generation: the ed25519 pair at `SSH_KEY_PATH`, its
/// directory created `0700`, the comment the instance's name
/// (`security::ssh_key_comment`; the default when the configuration cannot
/// be read, so a label never holds up the key). Returns the public key
/// line.
pub(super) async fn generate_ssh_key(db: &Arc<WardsonDbClient>) -> Result<String, String> {
    use crate::tools::security::{ssh_key_comment, SSH_KEY_PATH};
    let pub_path = format!("{SSH_KEY_PATH}.pub");
    let ssh_dir = std::path::Path::new(SSH_KEY_PATH)
        .parent()
        .ok_or_else(|| "the key path has no directory".to_string())?;
    std::fs::create_dir_all(ssh_dir)
        .map_err(|e| format!("could not create {}: {e}", ssh_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(ssh_dir, std::fs::Permissions::from_mode(0o700));
    }
    let comment = match config::load_config(db).await {
        Ok(cfg) => ssh_key_comment(&cfg.name),
        Err(_) => ssh_key_comment(""),
    };
    let out = crate::tools::subprocess("ssh-keygen")
        .args(["-t", "ed25519", "-f", SSH_KEY_PATH, "-N", "", "-C", &comment])
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh-keygen: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::read_to_string(&pub_path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("Key generated but could not read public key: {e}"))
}

/// `/local-ai-gateway`: show the provider gateway, store a URL and turn it on, turn
/// it on or off, or forget it (`provider::gateway::parse_action` and
/// `apply`). Before a URL is stored or turned on, the listener behind it
/// identifies itself (`gateway::identify`: the `x-local-ai-gateway` header
/// on `GET {url}/v1/models`); a listener that does not, or does not answer,
/// is refused and nothing is saved. A change is saved and followed by a
/// health probe through the new route. It takes effect on the next message,
/// because the provider is rebuilt from the configuration at every turn
/// start; a turn in flight keeps the route it was built with, so the probe
/// that runs now may disagree with that one turn.
pub(super) async fn handle_gateway_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
) {
    use crate::provider::gateway::{self, GatewayAction};
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

    let action = match gateway::parse_action(args) {
        Ok(action) => action,
        Err(msg) => {
            send_msg(msg, SystemMessageType::Error).await;
            return;
        }
    };
    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send_msg(format!("/local-ai-gateway: failed to load config: {e}"), SystemMessageType::Error)
                .await;
            return;
        }
    };
    let version = match gateway::url_to_identify(&cfg, &action) {
        Some(url) => match gateway::identify(&url).await {
            Ok(version) => Some(version),
            Err(reason) => {
                send_msg(gateway::refusal(&reason), SystemMessageType::Error).await;
                return;
            }
        },
        None => None,
    };
    let message = match gateway::apply(&mut cfg, &action, version.as_deref()) {
        Ok(message) => message,
        Err(msg) => {
            send_msg(msg, SystemMessageType::Error).await;
            return;
        }
    };
    if action != GatewayAction::Show {
        if let Err(e) = config::save_config(db, &cfg).await {
            send_msg(format!("/local-ai-gateway: failed to save: {e}"), SystemMessageType::Error).await;
            return;
        }
        crate::provider::health::request_probe();
    }
    send_msg(message, SystemMessageType::Info).await;
}

/// Refresh the status bar after a model switch. Reaching a slash command
/// implies the soul is sealed (Operational) — the learning loop ignores
/// slash commands — so an Operational→Operational ModeChange with the new
/// Brain is correct.
pub(super) async fn send_brain_status(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    cfg: &config::SystemConfig,
    session_mgr: &Arc<RwLock<SessionManager>>,
    brain: &str,
) {
    let session = session_mgr
        .read()
        .await
        .active_session
        .clone()
        .unwrap_or_else(|| "<none>".to_string());
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::ModeChange(
                ModeTransition {
                    from_mode: OperatingMode::Operational as i32,
                    to_mode: OperatingMode::Operational as i32,
                    message: operational_status_line(&cfg.name, &session, &cfg.timezone, brain),
                },
            )),
        }))
        .await;
}

/// `/download <path>` — offer a workspace file to the operator's browser:
/// the same jail as `file_offer`, a `File` frame (`origin: offered`) and
/// an Info note. The web console renders the card with its download link,
/// the terminal a file line. No session and no seal are involved.
pub(super) async fn handle_download_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage { content, msg_type: kind as i32 }
                )),
            })).await;
        }
    };
    let path = args.trim();
    if path.is_empty() || path == "help" {
        send(
            "Usage: /download <path> — a workspace file, absolute under /embra/workspace or relative to it (regular files up to 12 MiB).".to_string(),
            SystemMessageType::Info,
        )
        .await;
        return;
    }
    match media::offer::resolve_offer(path).await {
        Ok(f) => {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::File(
                    media::offer::file_ref_frame(&f, false, "", ""),
                )),
            })).await;
            info!(target: "media", path = %f.shown.display(), bytes = f.bytes, "file offered via /download");
            send(
                format!("Offered {} ({} KB) for download: {}", f.name, f.bytes / 1024, f.shown.display()),
                SystemMessageType::Info,
            )
            .await;
        }
        Err(e) => {
            send(format!("download failed: {}", e), SystemMessageType::Error).await;
        }
    }
}

/// `/attach <id|path>` — stage an image for the session's next message;
/// `/attach` / `/attach list` — show the staging + store usage;
/// `/attach clear` — drop the staging. Refused pre-seal (learning mode
/// dispatches no tools and runs its own loop). Uploaded ids come from
/// PutMedia (embra-web's PUT /api/media); paths import through the store
/// (`file_read`'s unjailed read policy, then normalize + copy into MEDIA).
pub(super) async fn handle_attach_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
) {
    let send = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage { content, msg_type: kind as i32 }
                )),
            })).await;
        }
    };
    if !matches!(learning::is_soul_sealed(db).await, Ok(true)) {
        send(
            "Attachments are available once Learning Mode completes (the soul is not sealed yet).".to_string(),
            SystemMessageType::Warning,
        )
        .await;
        return;
    }
    let session = match session_mgr.read().await.active_session.clone() {
        Some(s) => s,
        None => {
            send("No active session — /new <name> or /switch <name> first.".to_string(), SystemMessageType::Error).await;
            return;
        }
    };
    let store = media::MediaStore::default_store();
    let trimmed = args.trim();
    match trimmed {
        "" | "list" => {
            let mgr = session_mgr.read().await;
            let staged = mgr.staged_media(&session);
            let staged_files = mgr.staged_files(&session);
            let (files, bytes) = store.usage().await.unwrap_or((0, 0));
            let mut out = if staged.is_empty() && staged_files.is_empty() {
                format!("No attachments staged for session '{}'.", session)
            } else {
                let mut lines = vec![format!(
                    "{} attachment(s) staged for session '{}' (sent with your next message):",
                    staged.len() + staged_files.len(),
                    session
                )];
                for m in staged {
                    lines.push(format!("  {}  {} ({}×{}, {} KB)", m.id, m.name, m.width, m.height, m.byte_size / 1024));
                }
                for u in staged_files {
                    lines.push(format!("  {}  ({} KB, {})", u.path.display(), u.bytes / 1024, u.media_type));
                }
                lines.join("\n")
            };
            out.push_str(&format!(
                "\nMEDIA store: {} file(s), {:.1} MB at {}\nUsage: /attach <id|path> · /attach clear",
                files,
                bytes as f64 / 1_048_576.0,
                store.dir().display()
            ));
            send(out, SystemMessageType::Info).await;
        }
        "clear" => {
            let (images, files) = {
                let mut mgr = session_mgr.write().await;
                (mgr.take_staged_media(&session).len(), mgr.take_staged_files(&session).len())
            };
            send(
                format!("Dropped {} staged image(s) and {} file(s) for session '{}'.", images, files, session),
                SystemMessageType::Info,
            )
            .await;
        }
        arg => {
            let is_id = media::store::parse_media_id(arg).is_ok();
            // A path that does not start like an image is a text file:
            // staged by its path, read again when the turn is sent. The
            // read is not jailed, like an image path or `file_read`.
            if !is_id {
                let resolved = media::store::resolve_read_path(arg);
                if !media::text::looks_like_an_image(&resolved).await {
                    match media::text::read_text_file(&resolved).await {
                        Ok((upload, _)) => {
                            let (count, images) = {
                                let mut mgr = session_mgr.write().await;
                                let n = mgr.stage_file(&session, upload.clone());
                                (n, mgr.staged_media(&session).len())
                            };
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::File(
                                    media::text::file_ref_frame(&upload, "attached", false, "", ""),
                                )),
                            })).await;
                            info!(target: "media", session = %session, path = %upload.path.display(), staged = count, "text file staged via /attach");
                            send(
                                format!(
                                    "Attached {} ({} KB, {}) — {} attachment(s) will be sent with your next message. /attach list · /attach clear",
                                    upload.name,
                                    upload.bytes / 1024,
                                    upload.media_type,
                                    count + images
                                ),
                                SystemMessageType::Info,
                            )
                            .await;
                        }
                        Err(e) => {
                            send(format!("attach failed: {}", e), SystemMessageType::Error).await;
                        }
                    }
                    return;
                }
            }
            let result = if is_id {
                store.meta(arg).await
            } else {
                store.import_path(arg, &session).await
            };
            match result {
                Ok(meta) => {
                    let count = session_mgr.write().await.stage_media(&session, meta.clone());
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Media(
                            media::media_ref_frame(&meta, store.dir(), false, "", ""),
                        )),
                    })).await;
                    let mut note = format!(
                        "Attached {} ({}×{}, {} KB) — {} image(s) will be sent with your next message. /attach list · /attach clear",
                        meta.name,
                        meta.width,
                        meta.height,
                        meta.byte_size / 1024,
                        count
                    );
                    if let Ok(cfg) = config::load_config(db).await
                        && matches!(cfg.api_provider.as_str(), "ollama" | "lm_studio")
                    {
                        note.push_str(&format!(
                            "\nNote: the active provider is {} — image input works only if the loaded model is vision-capable.",
                            cfg.api_provider
                        ));
                    }
                    info!(target: "media", session = %session, id = %meta.id, staged = count, "media staged via /attach");
                    send(note, SystemMessageType::Info).await;
                }
                Err(e) => {
                    send(format!("attach failed: {}", e), SystemMessageType::Error).await;
                }
            }
        }
    }
}

/// `/embeddings` — the local semantic-similarity layer (KG-02). Status (no
/// args), `on`, `off`, `backfill [--force]`. Modeled on
/// `/image-provider` (config load → validate → save) with no key subcommand:
/// inference is in-OS, so there is no secret to hold.
pub(super) async fn handle_embeddings_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
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

    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send(format!("Failed to load config: {}", e), SystemMessageType::Error).await;
            return;
        }
    };

    let usage = "Usage: /embeddings | /embeddings on | /embeddings off | /embeddings backfill [--force]";
    let parts: Vec<&str> = args.split_whitespace().collect();

    match parts.as_slice() {
        [] => {
            let enabled = crate::embedding::embedding_enabled(&cfg);
            let model = crate::embedding::model_id(&cfg);
            let mut out = String::from("Embeddings (local semantic similarity)\n");
            out.push_str(&format!("  enabled:  {}\n", if enabled { "yes" } else { "no" }));
            out.push_str(&format!("  model:    {}\n", model));
            match crate::embedding::resolve_model_dir(&cfg) {
                Some((dir, source)) => {
                    out.push_str(&format!("  model at: {} ({})\n", dir.display(), source))
                }
                None => out.push_str("  model at: NOT FOUND — retrieval is lexical-only\n"),
            }
            if enabled {
                if let Some(provider) = crate::embedding::provider(&cfg).await {
                    // `stats` brings the index current first, so a fresh boot
                    // that has not yet run a retrieval never misreports an
                    // empty in-memory index as a lost backfill.
                    let (vectors, m) = crate::embedding::cache::stats(db, provider.as_ref()).await;
                    // The loaded provider's id, not the config field: they can only
                    // drift if config changes after load, and then the index is the truth.
                    let cov = crate::embedding::backfill::survey(db, provider.model_id()).await;
                    out.push_str(&format!(
                        "  loaded:   yes ({} dims)\n  index:    {} vectors in memory{}\n  nodes:    {}/{} embedded",
                        crate::embedding::EMBEDDING_DIM,
                        vectors,
                        if m.is_empty() { String::new() } else { format!(" [{m}]") },
                        cov.embedded(),
                        cov.total(),
                    ));
                    let per: Vec<String> = cov
                        .per_collection
                        .iter()
                        .map(|(c, e, t)| format!("{c} {e}/{t}"))
                        .collect();
                    if !per.is_empty() {
                        out.push_str(&format!(" — {}", per.join(", ")));
                    }
                    let needed = cov.needed();
                    if needed > 0 {
                        out.push_str(&format!(
                            "\n  {} node(s) need embedding — run /embeddings backfill",
                            needed
                        ));
                    }
                } else {
                    out.push_str("  loaded:   no — retrieval is lexical-only");
                }
            }
            // Failures never fail a write and were WARN lines only; this is
            // where a broken model or missing weights shows (Embra#16).
            let failures = crate::embedding::cache::failures().await;
            out.push_str(&format!(
                "\n  failures: {} write, {} query since boot",
                failures.write, failures.query
            ));
            if let Some(last) = failures.last {
                out.push_str(&format!(" — last {} at {}: {}", last.subject, last.at, last.reason));
            }
            send(out, SystemMessageType::Info).await;
        }
        ["on"] | ["off"] => {
            let on = parts[0] == "on";
            cfg.embedding_enabled = Some(on);
            match config::save_config(db, &cfg).await {
                Ok(_) => {
                    send(
                        format!(
                            "Embeddings {}.{}",
                            if on { "enabled" } else { "disabled" },
                            if on { " Takes effect on the next message." } else { "" }
                        ),
                        SystemMessageType::Info,
                    )
                    .await
                }
                Err(e) => send(format!("Failed to save config: {}", e), SystemMessageType::Error).await,
            }
        }
        ["backfill"] | ["backfill", "--force"] => {
            let force = parts.len() == 2;
            if !crate::embedding::embedding_enabled(&cfg) {
                send(
                    "Embeddings are disabled — run /embeddings on first.".to_string(),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }
            let Some(provider) = crate::embedding::provider(&cfg).await else {
                send("No embedding model available — see /embeddings".to_string(), SystemMessageType::Error).await;
                return;
            };
            let model = provider.model_id().to_string();
            let cov = crate::embedding::backfill::survey(db, &model).await;
            let total = cov.total();
            let needed = if force { total } else { cov.needed() };
            if needed == 0 {
                send(
                    format!("Nothing to do — all {total} node(s) are embedded with {model}."),
                    SystemMessageType::Info,
                )
                .await;
                return;
            }
            send(
                format!(
                    "Backfilling {needed} of {total} node(s) with {model}. This runs locally and \
                     takes roughly {} — the session is busy until it finishes.",
                    humanize_secs((needed as f64 * 0.055) as u64)
                ),
                SystemMessageType::Info,
            )
            .await;
            let started = std::time::Instant::now();
            let report = crate::embedding::backfill::run(db, &cfg, force, |line| {
                let tx = tx.clone();
                async move {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: line,
                                    msg_type: SystemMessageType::Info as i32,
                                },
                            )),
                        }))
                        .await;
                }
            })
            .await;
            match report {
                Ok(r) => {
                    let mut out = format!(
                        "Backfill complete in {}: {} embedded",
                        humanize_secs(started.elapsed().as_secs()),
                        r.embedded
                    );
                    if r.already_current > 0 {
                        out.push_str(&format!(", {} already current", r.already_current));
                    }
                    if r.skipped_empty > 0 {
                        out.push_str(&format!(", {} skipped (no text)", r.skipped_empty));
                    }
                    if r.failed > 0 {
                        out.push_str(&format!(", {} FAILED (see system_logs)", r.failed));
                    }
                    send(
                        out,
                        if r.failed > 0 { SystemMessageType::Warning } else { SystemMessageType::Info },
                    )
                    .await;
                }
                Err(e) => send(format!("Backfill failed: {e}"), SystemMessageType::Error).await,
            }
        }
        _ => send(usage.to_string(), SystemMessageType::Error).await,
    }
}

/// Rough human duration for operator-facing estimates.
pub(super) fn humanize_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// `/image-provider` — the image_generate backend. Status (no args),
/// `gemini`, `model <id>`, `key [<token>|remove]`, `clear`. Modeled on
/// `/effort` (config load → validate → save) and `/guardian key brave`
/// (STATE-only secret via `write_credential_state`, never echoed).
pub(super) async fn handle_image_provider_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
) {
    use crate::provider::image::{
        resolve_image_choice, ImageProviderKind, IMAGE_KEY_ENV, IMAGE_KEY_PATH_GEMINI,
    };
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
    let mut cfg = match config::load_config(db).await {
        Ok(c) => c,
        Err(e) => {
            send(format!("/image-provider: failed to load config: {e}"), SystemMessageType::Error).await;
            return;
        }
    };
    let parts: Vec<&str> = args.split_whitespace().collect();
    let usage = "Usage: /image-provider | /image-provider gemini | /image-provider model <id> | /image-provider key <token> | /image-provider key remove | /image-provider clear";
    match parts.as_slice() {
        [] => {
            let status = match resolve_image_choice(&cfg) {
                Ok(c) => {
                    let key = match c.key_source {
                        Some("env") => format!("SET ({IMAGE_KEY_ENV})"),
                        Some("state") => "SET (dedicated image key on STATE)".to_string(),
                        Some("gemini-llm-key") => "SET (falling back to the Gemini LLM key)".to_string(),
                        _ => "NOT set — /image-provider key <token> (or configure Gemini as the LLM provider)".to_string(),
                    };
                    format!(
                        "Image generation: provider {} ({}), model {}, key {}.\nModels: {}\n{}",
                        c.kind.as_str(),
                        if cfg.image_provider.is_some() { "explicit" } else { "default" },
                        c.model,
                        key,
                        c.kind.models().join(" | "),
                        usage
                    )
                }
                Err(e) => format!("Image generation: misconfigured — {e}\n{usage}"),
            };
            send(status, SystemMessageType::Info).await;
        }
        ["gemini"] => {
            cfg.image_provider = Some("gemini".to_string());
            match config::save_config(db, &cfg).await {
                Ok(()) => send(
                    format!(
                        "Image provider set to gemini (model {}). image_generate uses it on its next call.",
                        cfg.image_model.clone().unwrap_or_else(|| ImageProviderKind::Gemini.default_model().to_string())
                    ),
                    SystemMessageType::Info,
                )
                .await,
                Err(e) => send(format!("/image-provider: failed to save config: {e}"), SystemMessageType::Error).await,
            }
        }
        ["model", id] => {
            let kind = cfg
                .image_provider
                .as_deref()
                .and_then(ImageProviderKind::parse)
                .unwrap_or(ImageProviderKind::Gemini);
            if !kind.models().contains(id) {
                send(
                    format!("Unknown image model '{}'. Options: {}", id, kind.models().join(" | ")),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }
            cfg.image_model = Some((*id).to_string());
            match config::save_config(db, &cfg).await {
                Ok(()) => send(format!("Image model set to {id}. Takes effect on the next image_generate call."), SystemMessageType::Info).await,
                Err(e) => send(format!("/image-provider: failed to save config: {e}"), SystemMessageType::Error).await,
            }
        }
        ["key"] => {
            let set = std::fs::read_to_string(IMAGE_KEY_PATH_GEMINI)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            send(
                format!(
                    "Dedicated image-generation key: {}. Set with /image-provider key <token>; remove with /image-provider key remove. Without it image_generate falls back to the Gemini LLM key when one is configured.",
                    if set { "SET" } else { "NOT set" }
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        ["key", "remove"] => {
            match std::fs::remove_file(IMAGE_KEY_PATH_GEMINI) {
                Ok(()) => send("Dedicated image-generation key removed.".to_string(), SystemMessageType::Info).await,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    send("No dedicated image-generation key was set.".to_string(), SystemMessageType::Info).await
                }
                Err(e) => send(format!("/image-provider: failed to remove the key: {e}"), SystemMessageType::Error).await,
            }
        }
        ["key", token] => {
            match config::write_credential_state(IMAGE_KEY_PATH_GEMINI, token.trim()) {
                Ok(()) => {
                    info!(target: "media", "image-generation key saved to STATE");
                    send("Image-generation key saved (STATE, 0600). It is never echoed; image_generate uses it on its next call.".to_string(), SystemMessageType::Info).await
                }
                Err(e) => send(format!("/image-provider: failed to save the key: {e}"), SystemMessageType::Error).await,
            }
        }
        ["clear"] => {
            cfg.image_provider = None;
            cfg.image_model = None;
            match config::save_config(db, &cfg).await {
                Ok(()) => send("Image provider settings cleared (back to the defaults: gemini / gemini-3-pro-image; the key file is untouched).".to_string(), SystemMessageType::Info).await,
                Err(e) => send(format!("/image-provider: failed to save config: {e}"), SystemMessageType::Error).await,
            }
        }
        _ => send(usage.to_string(), SystemMessageType::Error).await,
    }
}

/// `/effort [<low|medium|high|xhigh|max> | reset]` — show or set the effort
/// level of the ACTIVE provider. Each provider has its own field
/// (`anthropic_effort`, `gemini_effort`; on the OpenAI-compatible presets one
/// level per model, `openai_compat.ollama_effort_by_model` /
/// `lm_studio_effort_by_model`), and its own wire form: Anthropic sends the
/// level as `output_config.effort`, Gemini clamps it to a `thinkingLevel`,
/// the OpenAI-compatible presets send it verbatim as `reasoning_effort` —
/// the accepted set is the model's, so nothing is clamped there. `reset`
/// clears the active provider's field (the active model's level).
///
/// The loop driver rebuilds the provider from config every turn, so a
/// change takes effect on the next user message. No ModeChange is emitted:
/// the status pill shows the model's display name, which effort does not
/// change.
pub(super) async fn handle_effort_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
) {
    use crate::provider::gemini::{thinking_level_for, THINKING_LEVEL};
    use crate::provider::openai_compat::{
        known_effort_ladder, reasoning_effort_for_model, OpenAiCompatPreset,
    };
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
                format!("/effort: failed to load config: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };

    let kind = ProviderKind::from_str(&cfg.api_provider).unwrap_or(ProviderKind::Anthropic);
    let label = kind.label();
    let preset = match kind {
        ProviderKind::Ollama => Some(OpenAiCompatPreset::Ollama),
        ProviderKind::LmStudio => Some(OpenAiCompatPreset::LmStudio),
        _ => None,
    };
    let local_model: Option<String> = preset.and_then(|p| non_empty(cfg.openai_compat.fields(p).1));
    let trimmed = args.trim();

    // No args → show the ACTIVE provider's posture, its default, and the
    // wire value it maps to.
    if trimmed.is_empty() {
        let msg = match kind {
            ProviderKind::Anthropic => {
                let current = resolve_anthropic_effort(&cfg);
                format!(
                    "Anthropic effort: {current}. Options: low, medium, high, xhigh, max \
                     (default max). Use `/effort high` to switch (takes effect on your \
                     next message); `/effort reset` restores the default."
                )
            }
            ProviderKind::Gemini => {
                let level = resolve_provider_effort(kind, &cfg);
                let wire = level
                    .as_deref()
                    .map(thinking_level_for)
                    .unwrap_or(THINKING_LEVEL);
                let shown = level
                    .clone()
                    .unwrap_or_else(|| format!("not set (default {THINKING_LEVEL})"));
                format!(
                    "Gemini effort: {shown} — sent as thinkingLevel={wire}. Options: low, \
                     medium, high, xhigh, max (xhigh and max send high, Gemini's \
                     ceiling). `/effort <level>` takes effect on your next message; \
                     `/effort reset` restores the default."
                )
            }
            ProviderKind::Ollama | ProviderKind::LmStudio => {
                // The level is the active model's: each model keeps its own.
                let level = resolve_provider_effort(kind, &cfg);
                let sent = match (&level, &local_model) {
                    (Some(l), _) => format!("sent as reasoning_effort={l}"),
                    (None, Some(m)) => automatic_effort_phrase(m),
                    (None, None) => "no model configured".to_string(),
                };
                let hint = local_model
                    .as_deref()
                    .and_then(|m| known_effort_ladder(m).map(|l| (m, l)))
                    .map(|(m, l)| format!(" {m} documents: {}.", l.join(", ")))
                    .unwrap_or_default();
                let shown = level.clone().unwrap_or_else(|| "not set".to_string());
                let whose = local_model.as_deref().map(|m| format!(" for {m}")).unwrap_or_default();
                format!(
                    "{label} effort{whose}: {shown} — {sent}.{hint} Options: low, medium, high, \
                     xhigh, max — sent verbatim; the server validates. `/effort <level>` \
                     sets this model's level and takes effect on your next message; \
                     `/effort reset` clears it."
                )
            }
        };
        send(msg, SystemMessageType::Info).await;
        return;
    }

    if trimmed.eq_ignore_ascii_case("reset") {
        set_effort_field(&mut cfg, kind, None);
        if let Err(e) = config::save_config(db, &cfg).await {
            send(
                format!("/effort: failed to save: {e}"),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
        // On Ollama and LM Studio only the active model's level goes.
        let msg = match local_model.as_deref() {
            Some(m) => format!(
                "{label} effort for {m} cleared — {}. Takes effect on your next message.",
                automatic_effort_phrase(m)
            ),
            None => format!("{label} effort reset to the default. Takes effect on your next message."),
        };
        send(msg, SystemMessageType::Info).await;
        return;
    }

    let Some(level) = parse_effort_choice(trimmed) else {
        send(
            format!(
                "'{trimmed}' is not a recognized effort level. Options: low, medium, \
                 high, xhigh, max."
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    };

    set_effort_field(&mut cfg, kind, Some(level.to_string()));
    if let Err(e) = config::save_config(db, &cfg).await {
        send(
            format!("/effort: failed to save: {e}"),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    match kind {
        ProviderKind::Anthropic => {
            send(
                format!("Anthropic effort set to {level}. Takes effect on your next message."),
                SystemMessageType::Info,
            )
            .await;
        }
        ProviderKind::Gemini => {
            let wire = thinking_level_for(level);
            send(
                format!(
                    "Gemini effort set to {level} (sent as thinkingLevel={wire}). Takes \
                     effect on your next message."
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        ProviderKind::Ollama | ProviderKind::LmStudio => {
            let whose = local_model
                .as_deref()
                .map(|m| format!(" for {m}"))
                .unwrap_or_default();
            send(
                format!(
                    "{label} effort{whose} set to {level} (sent as reasoning_effort={level}). \
                     Takes effect on your next message."
                ),
                SystemMessageType::Info,
            )
            .await;
            // The level is sent regardless (operator intent wins); say so
            // when the model's documented set does not include it, or when
            // embraOS does not know the model as reasoning-effort-aware.
            if let Some(m) = local_model.as_deref() {
                match known_effort_ladder(m) {
                    Some(ladder) if !ladder.contains(&level) => {
                        send(
                            format!(
                                "Note: {m} documents reasoning_effort values {}; '{level}' is \
                                 outside that set and the server may reject or ignore it.",
                                ladder.join(", ")
                            ),
                            SystemMessageType::Warning,
                        )
                        .await;
                    }
                    None if reasoning_effort_for_model(m).is_none() => {
                        send(
                            format!(
                                "Note: embraOS does not know {m} as a reasoning-effort model; \
                                 the value is sent anyway — LM Studio and Ollama ignore it for \
                                 models without the field."
                            ),
                            SystemMessageType::Warning,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Embedded feedback-loop protocol spec (Phase 3 preview, v2).
/// Read-only: baked into the binary at compile time.
pub(super) const FEEDBACK_LOOP_SPEC_V2: &str = include_str!("../brain/feedback_loop_spec_v2.md");

/// Build the synthetic user message that kicks off the feedback-loop protocol.
/// Fed through `handle_request` as if the user had typed it.
pub(super) fn build_feedback_loop_prompt() -> String {
    format!(
        "MANUAL FEEDBACK LOOP TRIGGER \u{2014} EXPERIMENTAL (Phase 3 Continuity Engine preview)\n\
\n\
Will has invoked /feedback-loop. Work through the embedded spec sequentially,\n\
starting at Step 1.1. Governance boundary: S0/S1 actions auto-execute; S2/S3\n\
actions pause at Step 4.2 for Will's approval.\n\
\n\
=== FEEDBACK LOOP SPEC v2.3 (read-only, embedded in binary) ===\n\
\n\
{}\n\
\n\
=== END SPEC ===\n\
\n\
Acknowledge, then execute Step 1.1.",
        FEEDBACK_LOOP_SPEC_V2
    )
}
