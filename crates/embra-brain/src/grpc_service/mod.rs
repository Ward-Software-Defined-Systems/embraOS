//! gRPC service implementation for embra-brain.
//!
//! Bridges Phase 0 Brain + tools + sessions into a gRPC streaming interface.

mod attach;
mod learning_loop;
mod provider_setup;
mod slash;
mod turn;
#[cfg(test)]
mod tests;

pub(crate) use attach::*;
use learning_loop::*;
pub(crate) use provider_setup::*;
use slash::*;
pub(crate) use turn::*;

use crate::activity;

use crate::tool_calls;

use crate::brain::Message;

use crate::config;

use crate::db::WardsonDbClient;

use crate::learning;

use crate::media;

use crate::proactive::Notification;

use crate::provider::anthropic::AnthropicProvider;

use crate::provider::gemini::GeminiProvider;

use crate::provider::ir::{
    ApiMessage, AssistantTurn, Block, EarlyStopReason, StopDetails, TurnOutcome,
};

use crate::provider::{
    LlmProvider, LlmRequestOptions, ProviderKind, StreamEvent, SystemPromptBundle, ToolManifest,
};

use crate::sessions::{SessionManager, SessionMeta, SessionState};

use crate::tools;

use embra_common::proto::brain::brain_service_server::BrainService;

use embra_common::proto::brain::*;

use embra_common::proto::common;

use futures::stream::BoxStream;

use std::pin::Pin;

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, watch, Mutex, RwLock};

use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};

use tonic::{Request, Response, Status, Streaming};

use tracing::{debug, error, info, warn};

/// Default per-turn tool-iteration cap (Embra_Debug #80). Operators can
/// override via `/iter-cap`, which writes `SystemConfig.max_tool_iterations`;
/// the loop driver reads `loaded_config.max_tool_iterations.unwrap_or(DEFAULT_MAX_TOOL_ITERATIONS)`
/// fresh each turn. Used by both the cap site and the `/iter-cap` handler so
/// the canonical default lives in exactly one place.
const DEFAULT_MAX_TOOL_ITERATIONS: usize = 100;

pub struct BrainGrpcService {
    db: Arc<WardsonDbClient>,
    session_manager: Arc<RwLock<SessionManager>>,
    config_tz: String,
    api_key: String,
    proactive_rx: Arc<Mutex<mpsc::Receiver<Notification>>>,
    start_time: std::time::Instant,
    /// True while a tool-iteration loop is in flight. `/provider`
    /// switches block on this and queue via `pending_provider`. Wired
    /// into the loop driver in Stage 2; idle in this stage.
    in_turn: Arc<AtomicBool>,
    /// Operator's queued provider switch. Drained between turns by the
    /// loop driver after `in_turn` clears. Stage 8 populates it.
    pending_provider: Arc<Mutex<Option<ProviderKind>>>,
    /// Set by `/provider --setup <anthropic|gemini>`: the next user
    /// message is the key for the given provider, then (once the key is
    /// saved) the pick of its model, rather than a regular conversation
    /// turn. The UserMessage handler intercepts each before the loop
    /// driver runs. OpenAI-compat presets use the parallel
    /// `pending_openai_compat_setup` multi-step state machine instead.
    pending_key_setup: Arc<Mutex<Option<KeySetupStep>>>,
    /// Multi-step state machine for `/provider --setup <ollama|lm_studio>`.
    /// Parallel to `pending_key_setup` but for OpenAI-compat presets'
    /// 4-step reconfigure flow (Endpoint → Bearer choice → Bearer
    /// token? → Model selection). Cleared on completion or any slash
    /// command cancel hatch.
    pending_openai_compat_setup: Arc<Mutex<Option<OpenAiCompatSetupState>>>,
    /// Guided `/sessions delete` flow state. Armed by the `/sessions
    /// delete <name>` slash arm; advanced/consumed by the UserMessage
    /// intercept; cancelled by any slash command. See
    /// [`PendingSessionDelete`].
    pending_session_delete: Arc<Mutex<Option<PendingSessionDelete>>>,
    /// Operator-stop generation counter (the `onboarding_stage` watch
    /// precedent). The unary `StopTurn` RPC bumps it while `in_turn` is
    /// set; every turn snapshots the value at start and treats "value >
    /// snapshot" as a stop request — so a stop can only ever affect the
    /// turn that was in flight when it was pressed, and no reset is
    /// needed. Out-of-band on purpose: the Converse loop awaits each turn
    /// inline, so anything sent on the stream parks behind the turn.
    stop_generation: Arc<watch::Sender<u64>>,
    /// Cross-stream onboarding-stage signal (Setup → Learning →
    /// Operational, carried as `OperatingMode as i32`). Each `converse`
    /// call spawns its own task; while the soul is unsealed every attached
    /// stream independently runs its OWN first-run wizard / learning loop,
    /// blocked on `incoming.next()` with no global re-check. The driver
    /// stream advances this watch when it finishes the wizard (→ Learning)
    /// and when it seals the soul (→ Operational); the other streams' loops
    /// `select!` on it and exit cleanly instead of staying frozen waiting
    /// for input that never comes (the desktop-stuck-on-Learning bug). Mode
    /// is brain-global (soul-sealed status), so no per-session keying is
    /// needed. `send_replace` is used so a send with zero current
    /// subscribers is a no-op rather than an error.
    ///
    /// The watch carries the stage and nothing else. Each stream writes to
    /// its own channel only: there is no registry of senders, and a stream
    /// that sees the stage move announces it to its own client.
    onboarding_stage: Arc<watch::Sender<i32>>,
}

impl BrainGrpcService {
    pub fn new(
        db: WardsonDbClient,
        config_tz: String,
        api_key: String,
        proactive_rx: mpsc::Receiver<Notification>,
    ) -> Self {
        let db = Arc::new(db);
        let session_manager = Arc::new(RwLock::new(
            SessionManager::new((*db).clone())
        ));

        // Seeded at Setup; the value only matters to streams blocked in an
        // onboarding loop, which observe CHANGES. A brain that boots
        // already-sealed never enters those loops, so the seed is moot there.
        let (stage_tx, _) = watch::channel(OperatingMode::Setup as i32);

        Self {
            db,
            session_manager,
            config_tz,
            api_key,
            proactive_rx: Arc::new(Mutex::new(proactive_rx)),
            start_time: std::time::Instant::now(),
            in_turn: Arc::new(AtomicBool::new(false)),
            pending_provider: Arc::new(Mutex::new(None)),
            pending_key_setup: Arc::new(Mutex::new(None)),
            pending_openai_compat_setup: Arc::new(Mutex::new(None)),
            pending_session_delete: Arc::new(Mutex::new(None)),
            onboarding_stage: Arc::new(stage_tx),
            stop_generation: Arc::new(watch::channel(0u64).0),
        }
    }
}

#[tonic::async_trait]
impl BrainService for BrainGrpcService {
    type ConverseStream = Pin<Box<dyn Stream<Item = Result<ConversationResponse, Status>> + Send>>;

    async fn converse(
        &self,
        request: Request<Streaming<ConversationRequest>>,
    ) -> Result<Response<Self::ConverseStream>, Status> {
        let mut incoming = request.into_inner();

        let (tx, rx) = mpsc::channel::<Result<ConversationResponse, Status>>(100);

        let db = self.db.clone();
        let session_mgr = self.session_manager.clone();
        let config_tz = self.config_tz.clone();
        let api_key = self.api_key.clone();
        let proactive_rx = self.proactive_rx.clone();
        let in_turn = self.in_turn.clone();
        let pending_provider = self.pending_provider.clone();
        let pending_key_setup = self.pending_key_setup.clone();
        let pending_openai_compat_setup = self.pending_openai_compat_setup.clone();
        let pending_session_delete = self.pending_session_delete.clone();
        let onboarding_stage = self.onboarding_stage.clone();
        let stop_rx = self.stop_generation.subscribe();

        tokio::spawn(async move {
            let mut config_tz = config_tz;
            let mut api_key = api_key;
            let ctx = ServiceCtx {
                db: db.clone(),
                session_mgr: session_mgr.clone(),
                in_turn: in_turn.clone(),
                pending_provider: pending_provider.clone(),
                pending_key_setup: pending_key_setup.clone(),
                pending_openai_compat_setup: pending_openai_compat_setup.clone(),
                pending_session_delete: pending_session_delete.clone(),
            };

            // Check for first-run: no config in WardSONDB
            let is_first_run = config::load_config(&db).await.is_err();

            if is_first_run {
                info!("First run detected — starting config wizard via gRPC");
                let (wizard_tx, mut wizard_rx) = mpsc::channel::<String>(1);

                // Spawn wizard task
                let wizard_db = db.clone();
                let wizard_grpc_tx = tx.clone();
                let mut wizard_handle = tokio::spawn(async move {
                    config::run_config_wizard_grpc(&wizard_grpc_tx, &mut wizard_rx, &wizard_db).await
                });

                // Watch for ANOTHER stream finishing the wizard (multiple
                // clients can hit first-run concurrently, each spawning its
                // own wizard). If one persists config first, this stream's
                // wizard would otherwise block forever on input that never
                // comes — the stage signal lets us abort and move on.
                let mut stage_rx = onboarding_stage.subscribe();

                // Feed user responses to wizard until it completes
                loop {
                    tokio::select! {
                        msg = incoming.next() => {
                            match msg {
                                Some(Ok(req)) => {
                                    if let Some(conversation_request::RequestType::UserMessage(um)) = req.request_type {
                                        // Forward user input to wizard
                                        if wizard_tx.send(um.content).await.is_err() {
                                            break; // Wizard closed its receiver
                                        }
                                    }
                                    // Ignore non-UserMessage during wizard (SlashCommand, SessionAttach)
                                }
                                Some(Err(e)) => { warn!("Stream error during wizard: {}", e); break; }
                                None => { debug!("Client disconnected during wizard"); break; }
                            }
                        }
                        result = &mut wizard_handle => {
                            match result {
                                Ok(Ok(new_config)) => {
                                    info!("Config wizard completed: name={}", new_config.name);
                                    api_key = new_config.api_key;
                                    config_tz = new_config.timezone;
                                    // We're the driver — config is persisted.
                                    // Wake any other stream blocked in its own
                                    // wizard loop so it can adopt the config and
                                    // advance to the learning stage.
                                    let _ = onboarding_stage
                                        .send_replace(OperatingMode::Learning as i32);
                                }
                                Ok(Err(e)) => {
                                    error!("Config wizard failed: {}", e);
                                }
                                Err(e) => {
                                    error!("Config wizard task panicked: {}", e);
                                }
                            }
                            break;
                        }
                        _ = stage_rx.changed() => {
                            // Another stream finished the first-run wizard
                            // (config now persisted). Our wizard task is still
                            // blocked on input that will never arrive — abort
                            // it, adopt the saved config, and fall through to
                            // the learning stage.
                            if let Ok(cfg) = config::load_config(&db).await {
                                api_key = cfg.api_key;
                                config_tz = cfg.timezone;
                                wizard_handle.abort();
                                break;
                            }
                        }
                    }
                }
            }

            // Stage 2: Learning Mode (if soul not sealed)
            let soul_sealed = learning::is_soul_sealed(&db).await.unwrap_or(false);
            if !soul_sealed {
                info!("Soul not sealed — entering Learning Mode");
                let loaded_config = config::load_config(&db).await.unwrap_or_else(|_| config::SystemConfig::boot_fallback(api_key.clone(), config_tz.clone()));
                match run_learning_loop(&tx, &mut incoming, &db, &loaded_config, &api_key, &onboarding_stage, TurnStop { in_turn: &in_turn, stop_rx: &stop_rx }).await {
                    Ok(()) => {
                        info!("Learning Mode complete — transitioning to Operational");
                        // Reload config in case it was updated
                        if let Ok(cfg) = config::load_config(&db).await {
                            api_key = cfg.api_key;
                            config_tz = cfg.timezone;
                        }
                        // Ensure default session exists and is active
                        {
                            let store = session_mgr.read().await.store();
                            match store.session_exists("main").await {
                                Ok(true) => {}
                                Ok(false) => {
                                    if let Err(e) = store.create("main").await {
                                        error!("Session 'main' not created: {e}");
                                    }
                                }
                                Err(e) => error!("Session 'main' could not be checked: {e}; activating it as it is"),
                            }
                            session_mgr.write().await.active_session = Some("main".to_string());
                            info!("Session 'main' activated for Operational mode");
                        }
                    }
                    Err(e) => {
                        error!("Learning Mode failed: {}", e);
                    }
                }
            }

            // Try to get proactive notifications (only one Converse stream gets them)
            let mut proactive = proactive_rx.try_lock().ok();
            let mut act_budget = ActBudget::new();

            // Stage 3: Main conversation loop (Operational mode)
            loop {
                tokio::select! {
                    msg = incoming.next() => {
                        match msg {
                            Some(Ok(req)) => {
                                if let Err(e) = handle_request(
                                    req, &tx, &ctx, &config_tz, &api_key, &stop_rx
                                ).await {
                                    error!("Error handling request: {}", e);
                                    let _ = tx.send(Ok(ConversationResponse {
                                        response_type: Some(conversation_response::ResponseType::System(
                                            SystemMessage {
                                                content: format!("Error: {}", e),
                                                msg_type: SystemMessageType::Error as i32,
                                            }
                                        )),
                                    })).await;
                                }
                            }
                            Some(Err(e)) => {
                                warn!("Stream error: {}", e);
                                break;
                            }
                            None => {
                                debug!("Client stream ended");
                                break;
                            }
                        }
                    }
                    // Forward proactive notifications if we hold the lock
                    notif = async {
                        if let Some(ref mut rx) = proactive {
                            rx.recv().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        if let Some(notif) = notif {
                            activity::emit(activity::Event::Notification {
                                priority: notif.priority_label().to_string(),
                            });
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: format!("[{}] {}", notif.priority_label(), notif.message),
                                        msg_type: SystemMessageType::Notification as i32,
                                    }
                                )),
                            })).await;
                            // A reminder or job that asked for a turn gets
                            // one here, between turns, on the stream that
                            // holds the receiver: the resume briefing's
                            // re-entry, with `pending_trigger` as the marker.
                            // A refused trigger is still the notice above,
                            // plus the reason.
                            if let Some(act) = notif.act {
                                let refusal = trigger_refusal(
                                    &db, &session_mgr, &in_turn, &pending_key_setup,
                                    &pending_openai_compat_setup, &pending_session_delete, &mut act_budget,
                                ).await;
                                match refusal {
                                    Some(why) => {
                                        warn!(target: "proactive", kind = act.kind.as_str(), "trigger turn not run: {}", why);
                                        let _ = tx.send(Ok(ConversationResponse {
                                            response_type: Some(conversation_response::ResponseType::System(
                                                SystemMessage {
                                                    content: format!("[trigger not run] {}", why),
                                                    msg_type: SystemMessageType::Info as i32,
                                                }
                                            )),
                                        })).await;
                                    }
                                    None => {
                                        info!(target: "proactive", kind = act.kind.as_str(), "trigger turn starting");
                                        let content = trigger_turn_text(&act);
                                        session_mgr.write().await.pending_trigger = Some(crate::sessions::TriggerTurn {
                                            kind: act.kind,
                                            headline: act.headline,
                                            body: act.body,
                                        });
                                        let synthetic = ConversationRequest {
                                            request_type: Some(conversation_request::RequestType::UserMessage(
                                                UserMessage {
                                                    content,
                                                    timestamp: None,
                                                    attachment_ids: Vec::new(),
                                                    file_paths: Vec::new(),
                                                }
                                            )),
                                        };
                                        if let Err(e) = handle_request(
                                            synthetic, &tx, &ctx, &config_tz, &api_key, &stop_rx
                                        ).await {
                                            error!("Error handling trigger turn: {}", e);
                                            let _ = tx.send(Ok(ConversationResponse {
                                                response_type: Some(conversation_response::ResponseType::System(
                                                    SystemMessage {
                                                        content: format!("Error: {}", e),
                                                        msg_type: SystemMessageType::Error as i32,
                                                    }
                                                )),
                                            })).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }

    type WatchActivityStream = Pin<Box<dyn Stream<Item = Result<ActivityFrame, Status>> + Send>>;

    /// The activity feed (`activity.rs`): a snapshot first, then a tick
    /// every `TICK_INTERVAL` while something happens and a snapshot every
    /// `SNAPSHOT_INTERVAL`. embra-web holds the one production subscription
    /// and fans it out to the browsers.
    async fn watch_activity(
        &self,
        _req: Request<WatchActivityRequest>,
    ) -> Result<Response<Self::WatchActivityStream>, Status> {
        let mut events = activity::subscribe();
        let mut sample_rx = activity::db_sample(&self.db);
        // The first snapshot is worth a short wait for the first sample.
        if sample_rx.borrow().is_none() {
            let _ = tokio::time::timeout(activity::FIRST_SAMPLE_WAIT, sample_rx.changed()).await;
        }

        let (tx, rx) = mpsc::channel::<Result<ActivityFrame, Status>>(64);
        let session_mgr = self.session_manager.clone();
        let in_turn = self.in_turn.clone();
        let stage = self.onboarding_stage.clone();
        let start_time = self.start_time;

        tokio::spawn(async move {
            let first = activity_snapshot_frame(&session_mgr, &in_turn, &stage, start_time, &sample_rx).await;
            if tx.send(Ok(first)).await.is_err() {
                return;
            }
            let mut tick = tokio::time::interval(activity::TICK_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut snapshot = tokio::time::interval(activity::SNAPSHOT_INTERVAL);
            snapshot.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // An interval's first tick is immediate; the snapshot went out above.
            tick.tick().await;
            snapshot.tick().await;
            let mut builder = activity::TickBuilder::default();
            let mut last_frame = std::time::Instant::now();
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Ok(ev) => builder.fold(ev),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => builder.lagged(n),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = tick.tick() => {
                        if builder.is_empty() {
                            continue;
                        }
                        let tick = builder.take(last_frame.elapsed(), in_turn.load(Ordering::SeqCst));
                        last_frame = std::time::Instant::now();
                        let frame = ActivityFrame { frame: Some(activity_frame::Frame::Tick(tick)) };
                        if tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    _ = snapshot.tick() => {
                        let frame = activity_snapshot_frame(&session_mgr, &in_turn, &stage, start_time, &sample_rx).await;
                        if tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type WatchToolCallsStream = Pin<Box<dyn Stream<Item = Result<ToolCallRecord, Status>> + Send>>;

    /// The tool-call feed (`tool_calls.rs`): one record per finished call,
    /// with its input and result cut per side. embra-web holds the one
    /// production subscription and fans it out to the Tools tab.
    async fn watch_tool_calls(
        &self,
        _req: Request<WatchToolCallsRequest>,
    ) -> Result<Response<Self::WatchToolCallsStream>, Status> {
        let rx = tool_calls::subscribe();
        let (tx, out) = mpsc::channel::<Result<ToolCallRecord, Status>>(64);
        tokio::spawn(tool_calls::forward(rx, tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(out))))
    }

    /// The record read back (`tools.turn_trace`), newest first: the Tools
    /// tab's backfill after a restart and its paging.
    async fn list_tool_calls(
        &self,
        req: Request<ListToolCallsRequest>,
    ) -> Result<Response<ListToolCallsResponse>, Status> {
        let req = req.into_inner();
        let limit = if req.limit == 0 {
            tool_calls::LIST_DEFAULT
        } else {
            req.limit.min(tool_calls::LIST_MAX)
        };
        let body = tool_calls::list_query_body(limit, &req.before, &req.session);
        let docs = self
            .db
            .query_with_options("tools.turn_trace", &body)
            .await
            .map_err(|e| Status::unavailable(format!("tools.turn_trace: {e}")))?;
        let records = docs
            .as_array()
            .map(|a| a.iter().filter_map(tool_calls::record_from_trace).collect())
            .unwrap_or_default();
        Ok(Response::new(ListToolCallsResponse { records }))
    }

    // --- Session RPCs ---

    async fn list_sessions(&self, _req: Request<ListSessionsRequest>) -> Result<Response<ListSessionsResponse>, Status> {
        let mgr = self.session_manager.read().await;
        let sessions = mgr.list_with_counts().await
            .map_err(|e| Status::internal(format!("Failed to list sessions: {}", e)))?;

        Ok(Response::new(ListSessionsResponse {
            // Soft-deleted sessions are hidden from the RPC (the mobile
            // sheet renders this list as-is); the TUI /sessions arm shows
            // them separately with a restore hint.
            sessions: sessions.into_iter()
                .filter(|s| s.state != SessionState::Deleted)
                .map(|s| SessionInfo {
                    provider: s.recorded_provider().to_string(),
                    name: s.name,
                    state: format!("{:?}", s.state),
                    turn_count: s.turn_count.unwrap_or(0),
                    created_at: Some(common::Timestamp { iso8601: s.created_at.to_rfc3339() }),
                    last_active: Some(common::Timestamp { iso8601: s.last_active.to_rfc3339() }),
                    has_summary: false,
                }).collect(),
        }))
    }

    async fn stop_turn(&self, _request: Request<StopTurnRequest>) -> Result<Response<StopTurnResponse>, Status> {
        // Bump the stop generation only while a turn is in flight — an
        // idle press must not arm a stop for the NEXT turn (each turn
        // snapshots the counter at start, so a pre-turn bump would be
        // invisible anyway; the in_turn check makes the reply honest).
        let was_in_turn = self.in_turn.load(Ordering::SeqCst);
        if was_in_turn {
            self.stop_generation.send_modify(|v| *v += 1);
            info!(target: "dispatch", "operator stop requested — signalling in-flight turn");
        }
        Ok(Response::new(StopTurnResponse { was_in_turn }))
    }

    // --- Media store ---

    async fn put_media(&self, request: Request<PutMediaRequest>) -> Result<Response<PutMediaResponse>, Status> {
        let req = request.into_inner();
        if req.data.len() > media::MEDIA_UPLOAD_MAX {
            return Err(Status::resource_exhausted(format!(
                "image is {} bytes; the limit is {} bytes",
                req.data.len(),
                media::MEDIA_UPLOAD_MAX
            )));
        }
        if media::ingest::sniff(&req.data).is_none() {
            // Not an image: a text file lands in uploads/ under its name,
            // or the upload is refused with the reason.
            let dir = media::text::UploadsDir::default_dir();
            let name = if req.name.trim().is_empty() {
                media::text::FALLBACK_NAME
            } else {
                req.name.as_str()
            };
            let upload = dir.store_text(name, &req.data).await.map_err(text_status)?;
            info!(
                target: "media",
                path = %upload.path.display(),
                session = %req.session_name,
                bytes = upload.bytes,
                media_type = %upload.media_type,
                "text upload stored"
            );
            return Ok(Response::new(PutMediaResponse {
                media: None,
                file: Some(media::text::file_ref_frame(&upload, "attached", false, "", "")),
            }));
        }
        let store = media::MediaStore::default_store();
        let normalized = media::ingest::normalize(req.data)
            .await
            .map_err(|e| media_status(e.into()))?;
        let transformed = normalized.transformed;
        let name = if req.name.trim().is_empty() { "image" } else { req.name.as_str() };
        let meta = store
            .put(media::MediaOrigin::Attached, name, &req.session_name, normalized)
            .await
            .map_err(media_status)?;
        info!(
            target: "media",
            id = %meta.id,
            session = %req.session_name,
            bytes = meta.byte_size,
            width = meta.width,
            height = meta.height,
            media_type = %meta.media_type,
            transformed,
            "media stored"
        );
        Ok(Response::new(PutMediaResponse {
            media: Some(media::media_ref_frame(&meta, store.dir(), false, "", "")),
            file: None,
        }))
    }

    /// A workspace file for download. The resolver is the jail: through
    /// any symlink, regular files only, under `FILE_DOWNLOAD_MAX`.
    async fn get_file(&self, request: Request<GetFileRequest>) -> Result<Response<GetFileResponse>, Status> {
        let path = request.into_inner().path;
        let offered = media::offer::resolve_offer(&path).await.map_err(offer_status)?;
        let data = tokio::fs::read(&offered.path)
            .await
            .map_err(|e| Status::internal(format!("read {}: {e}", offered.path.display())))?;
        if data.len() as u64 > media::offer::FILE_DOWNLOAD_MAX {
            // The file grew between the check and the read.
            return Err(Status::resource_exhausted(
                media::offer::OfferError::TooLarge(data.len() as u64, media::offer::FILE_DOWNLOAD_MAX).to_string(),
            ));
        }
        info!(target: "media", path = %offered.path.display(), bytes = data.len(), "file served for download");
        Ok(Response::new(GetFileResponse {
            file: Some(media::offer::file_ref_frame(&offered, false, "", "")),
            data,
        }))
    }

    async fn get_media(&self, request: Request<GetMediaRequest>) -> Result<Response<GetMediaResponse>, Status> {
        let id = request.into_inner().id;
        let store = media::MediaStore::default_store();
        let (meta, data) = store.get(&id).await.map_err(media_status)?;
        Ok(Response::new(GetMediaResponse {
            media: Some(media::media_ref_frame(&meta, store.dir(), false, "", "")),
            data,
        }))
    }

    async fn create_session(&self, request: Request<CreateSessionRequest>) -> Result<Response<CreateSessionResponse>, Status> {
        let name = request.into_inner().name;
        let store = self.session_manager.read().await.store();
        let session = store.create(&name).await
            .map_err(|e| Status::internal(format!("Failed to create session: {}", e)))?;
        // A created session is the active one, as it was when `create`
        // set it itself.
        self.session_manager.write().await.active_session = Some(session.name.clone());

        Ok(Response::new(CreateSessionResponse {
            session: Some(SessionInfo {
                provider: session.recorded_provider().to_string(),
                name: session.name,
                state: format!("{:?}", session.state),
                turn_count: session.turn_count.unwrap_or(0),
                created_at: Some(common::Timestamp { iso8601: session.created_at.to_rfc3339() }),
                last_active: Some(common::Timestamp { iso8601: session.last_active.to_rfc3339() }),
                has_summary: false,
            }),
        }))
    }

    async fn switch_session(&self, request: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        let name = request.into_inner().name;

        // Cross-provider guard — same rule as SessionAttach. The check
        // runs before any state mutation so a rejected switch leaves
        // the existing active session intact.
        if let Err((session_provider, active_provider)) =
            check_session_provider(&self.db, &name).await
        {
            let active_session = self.session_manager.read().await.active_session.clone();
            return Err(Status::failed_precondition(cross_provider_block_message(
                &name,
                &session_provider,
                &active_provider,
                active_session.as_deref(),
            )));
        }

        // The round trips run on the store handle, the manager written for
        // a blink (#59).
        let (store, current) = {
            let mgr = self.session_manager.read().await;
            (mgr.store(), mgr.active_session.clone())
        };

        // A soft-deleted session is refused, as on the other three paths
        // that make a session active (`/switch`, `/new`, attach). Asked
        // before the current session is detached, so a refused switch
        // leaves it in place. `reattach` would refuse as well; from there
        // the reason would arrive as an internal error.
        let meta = store.get_meta(&name).await.ok().flatten();
        if let Some(refusal) = meta.as_ref().and_then(|m| crate::sessions::reattach_refusal(&name, m)) {
            return Err(Status::failed_precondition(refusal));
        }

        // Detach current session if any
        if let Some(ref current) = current {
            let _ = store.detach(current).await;
            self.session_manager.write().await.clear_active(current);
        }

        let history = store.reattach(&name).await
            .map_err(|e| Status::internal(format!("{}", e)))?;
        self.session_manager.write().await.active_session = Some(name.clone());

        Ok(Response::new(SwitchSessionResponse {
            session: Some(SessionInfo {
                name: name.clone(),
                state: "Active".to_string(),
                turn_count: history.len() as u32,
                created_at: None,
                last_active: None,
                has_summary: false,
                provider: meta
                    .as_ref()
                    .map_or(crate::sessions::LEGACY_SESSION_PROVIDER, SessionMeta::recorded_provider)
                    .to_string(),
            }),
            reconnection_briefing: String::new(),
        }))
    }

    async fn close_session(&self, _req: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        let (store, closed) = {
            let mgr = self.session_manager.read().await;
            (mgr.store(), mgr.active_session.clone().unwrap_or_default())
        };
        if !closed.is_empty() {
            let _ = store.close(&closed).await;
            self.session_manager.write().await.clear_active(&closed);
        }
        Ok(Response::new(CloseSessionResponse {
            closed_session: closed,
            switched_to: String::new(),
        }))
    }

    async fn get_session_meta(&self, _request: Request<GetSessionMetaRequest>) -> Result<Response<GetSessionMetaResponse>, Status> {
        Err(Status::unimplemented("GetSessionMeta not yet implemented"))
    }

    async fn get_system_status(&self, _req: Request<GetSystemStatusRequest>) -> Result<Response<GetSystemStatusResponse>, Status> {
        // The service rows apid's REST /status proxies and the web console
        // renders as pills (no proto change): the active LLM provider's
        // last endpoint probe as two entries, and the provider gateway as
        // configured (`on` or `off` with its URL, nothing while none is
        // stored). A configuration that cannot be read adds no row.
        let mut services = std::collections::HashMap::new();
        if let Some(p) = crate::provider::health::latest() {
            services.insert("llm-provider".to_string(), p.state().to_string());
            services.insert("llm-provider.detail".to_string(), p.summary_line());
        }
        if let Ok(cfg) = config::load_config(&self.db).await {
            services.extend(crate::provider::gateway::status_rows(&cfg));
        }
        Ok(Response::new(GetSystemStatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            soul: None,
            wardsondb_status: if self.db.health().await.unwrap_or(false) { "healthy" } else { "unhealthy" }.to_string(),
            services,
        }))
    }

    async fn get_soul_document(&self, _req: Request<GetSoulDocumentRequest>) -> Result<Response<GetSoulDocumentResponse>, Status> {
        match learning::load_soul(&self.db).await {
            Ok(Some(soul)) => {
                // A hash that cannot be computed is an error, never an empty
                // digest shown as a seal (#13); `sealed` is the seal as verified
                // now against the STATE hash, not the presence of a document (#15).
                let hash = learning::compute_soul_hash(&soul)
                    .map_err(|e| Status::internal(format!("Failed to hash the soul: {e}")))?;
                let sealed = learning::seal_of(&soul, std::path::Path::new(learning::SOUL_HASH_PATH))
                    == learning::SealState::Sealed;
                Ok(Response::new(GetSoulDocumentResponse {
                    soul_json: serde_json::to_string_pretty(&soul).unwrap_or_default(),
                    sha256: hash,
                    sealed,
                }))
            }
            Ok(None) => Ok(Response::new(GetSoulDocumentResponse {
                soul_json: String::new(),
                sha256: String::new(),
                sealed: false,
            })),
            Err(e) => Err(Status::internal(format!("Failed to load soul: {}", e))),
        }
    }

    async fn get_identity(&self, _req: Request<GetIdentityRequest>) -> Result<Response<GetIdentityResponse>, Status> {
        match self.db.read("memory.identity", "identity").await {
            Ok(doc) => Ok(Response::new(GetIdentityResponse {
                identity_json: serde_json::to_string_pretty(&doc).unwrap_or_default(),
            })),
            Err(_) => Ok(Response::new(GetIdentityResponse {
                identity_json: String::new(),
            })),
        }
    }

    async fn get_expression(&self, _req: Request<GetExpressionRequest>) -> Result<Response<ExpressionState>, Status> {
        match self.db.read("ui", "expression").await {
            Ok(doc) => Ok(Response::new(ExpressionState {
                content: doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                version: doc.get("version").and_then(|v| v.as_u64()).unwrap_or(0),
                updated_at: doc.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })),
            Err(_) => Ok(Response::new(ExpressionState {
                content: String::new(),
                version: 0,
                updated_at: String::new(),
            })),
        }
    }

    async fn get_mode(&self, _req: Request<GetModeRequest>) -> Result<Response<GetModeResponse>, Status> {
        let is_sealed = learning::is_soul_sealed(&self.db).await.unwrap_or(false);
        let mode = if is_sealed {
            OperatingMode::Operational
        } else {
            OperatingMode::Learning
        };
        // #15: the mode follows the document, as before; the word re-verifies
        // the seal against the stored hash.
        let soul_status = learning::seal_state(&self.db).await.status_word().to_string();
        Ok(Response::new(GetModeResponse {
            mode: mode as i32,
            soul_status,
        }))
    }

    async fn health_check(&self, _req: Request<common::HealthCheckRequest>) -> Result<Response<common::HealthCheckResponse>, Status> {
        Ok(Response::new(common::HealthCheckResponse {
            status: common::HealthStatus::Healthy as i32,
            service_name: "embra-brain".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            details: std::collections::HashMap::new(),
        }))
    }
}

/// The long-lived handles of one Converse stream, taken from the service
/// once and passed by reference (#38): the handlers took them one by one,
/// twelve arguments each, re-listed at every dispatch. The timezone and
/// the key stay arguments: the stream changes them after the wizard.
struct ServiceCtx {
    db: Arc<WardsonDbClient>,
    session_mgr: Arc<RwLock<SessionManager>>,
    in_turn: Arc<AtomicBool>,
    pending_provider: Arc<Mutex<Option<ProviderKind>>>,
    pending_key_setup: Arc<Mutex<Option<KeySetupStep>>>,
    pending_openai_compat_setup: Arc<Mutex<Option<OpenAiCompatSetupState>>>,
    pending_session_delete: Arc<Mutex<Option<PendingSessionDelete>>>,
}

/// The Operational status line the console's status bar parses
/// (`embra-console/src/terminal/mod.rs`): one literal for the five sites
/// that build it (#38). The model is passed explicitly, because the
/// provider-switch site renders the provider being switched to.
fn operational_status_line(name: &str, session: &str, tz: &str, model: &str) -> String {
    format!("Operational — Name: {} — Session: {} — TZ: {} — Brain: {}", name, session, tz, model)
}

/// One system frame to the client, of the given kind: the operator is told
/// what did not happen (#59), in the stream's own vocabulary.
async fn send_system(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    content: String,
    kind: SystemMessageType,
) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage {
                content,
                msg_type: kind as i32,
            })),
        }))
        .await;
}

// Bearer helpers live with the provider now (the health probe needs them
// too); imported here for the setup flows and their tests.
use crate::provider::openai_compat::{bearer_env_var, bearer_state_path};
