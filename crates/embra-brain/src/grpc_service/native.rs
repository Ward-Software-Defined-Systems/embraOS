//! Native models (experimental) in the operator's commands: the pick of a
//! model. `/model` while a native model leads the session and `/provider
//! --setup native` offer the catalogue (`native_model_selection`); the pick
//! asks embrad to keep the model loaded (`pick_native_model`), after its
//! file was downloaded and checked when it is not on the machine.

use super::*;
use crate::native_model::jobs::{JobEnd, JobEvent, JobKind, Jobs};
use crate::native_model::{self, Env};
use crate::provider::openai_compat::OpenAiCompatPreset;
use embra_boot::native_model::{self as contract, Installed, NativeModel};
use std::time::Duration;
use tokio::sync::broadcast;

/// What a pick of a native model works with beyond the console that asked:
/// the machine's records and updater, the brain's jobs, and the two handles
/// of the service that a download's end needs when it comes much later.
#[derive(Clone)]
pub(crate) struct NativeHands {
    pub(super) env: Env,
    pub(super) jobs: Jobs,
    /// The mark of a turn in flight. A model that had to be downloaded
    /// first is asked for between turns: embrad ends the server that holds
    /// the model before it, and a turn on that one would be cut.
    pub(super) in_turn: Arc<AtomicBool>,
    pub(super) session_mgr: Arc<RwLock<SessionManager>>,
}

impl std::fmt::Debug for NativeHands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeHands").field("env", &self.env).finish_non_exhaustive()
    }
}

impl NativeHands {
    /// The machine's own.
    pub(super) fn in_force(in_turn: &Arc<AtomicBool>, session_mgr: &Arc<RwLock<SessionManager>>) -> Self {
        NativeHands { env: Env::in_force(), jobs: Jobs::shared().clone(), in_turn: in_turn.clone(), session_mgr: session_mgr.clone() }
    }
}

/// A size as the updater's lines say it.
fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// How a model's file stands on this machine, for the selector's prompt.
fn file_words(model: &NativeModel, env: &Env) -> String {
    match contract::installed(model, &env.models) {
        Installed::Verified => "installed".to_string(),
        Installed::No => "not installed, downloaded first".to_string(),
        Installed::Partial(bytes) => format!("{} of {} MB downloaded, the rest is downloaded first", mb(bytes), mb(model.bytes)),
        Installed::Unverified(why) => format!("its file is checked first ({why})"),
    }
}

/// The native model a typed name means: an id of the catalogue, whatever
/// its case. The error names the catalogue.
pub(super) fn native_model_choice(wanted: &str) -> Result<&'static NativeModel, String> {
    let wanted = wanted.trim();
    contract::CATALOGUE.iter().find(|m| m.id.eq_ignore_ascii_case(wanted)).ok_or_else(|| {
        let ids: Vec<&str> = contract::CATALOGUE.iter().map(|m| m.id).collect();
        format!("'{wanted}' is not a native model. The catalogue (experimental): {}. /model alone picks from the list.", ids.join(", "))
    })
}

/// The selector of the catalogue's models, and the state that receives the
/// pick at the reconfigure flow's model step. The model kept loaded is the
/// first option as well as the default: the console's selector starts on
/// the first option whatever the default, so Enter keeps the model.
pub(super) fn native_model_selection(current: Option<&str>, hands: NativeHands) -> (SetupPrompt, OpenAiCompatSetupState) {
    let mut ids: Vec<String> = contract::CATALOGUE.iter().map(|m| m.id.to_string()).collect();
    if let Some(at) = current.and_then(|current| ids.iter().position(|id| id == current)) {
        let kept = ids.remove(at);
        ids.insert(0, kept);
    }
    let stands: Vec<String> = contract::CATALOGUE
        .iter()
        .map(|m| format!("{} is {} ({}, {} MB): {}", m.id, m.label, m.quantisation, mb(m.bytes), file_words(m, &hands.env)))
        .collect();
    let prompt = SetupPrompt {
        field_type: SetupFieldType::Selector as i32,
        prompt: format!("Select a native model (experimental). {}.", stands.join("; ")),
        options: ids.clone(),
        default_value: ids.first().cloned().unwrap_or_default(),
    };
    let state = OpenAiCompatSetupState {
        preset: OpenAiCompatPreset::Native,
        current_endpoint: None,
        current_bearer: None,
        current_model: current.map(str::to_string),
        // A native model has no endpoint and no bearer to carry to the step.
        step: OpenAiCompatSetupStep::AwaitingModelSelection { endpoint: String::new(), bearer: None, models: ids },
        // The pick moves the model alone, as `/model` on a server preset.
        model_only: true,
        gateway: None,
        native: Some(hands),
    };
    (prompt, state)
}

async fn say(tx: &mpsc::Sender<Result<ConversationResponse, Status>>, content: String, kind: SystemMessageType) {
    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(SystemMessage { content, msg_type: kind as i32 })),
        }))
        .await;
}

/// Ask for `model` to be kept loaded: the stored configuration names it,
/// the record embrad follows is written from it, and the health probe is
/// asked to look. Returns the configuration and whether the model changed.
pub(super) async fn load_native_model(
    model: &'static NativeModel,
    db: &Arc<WardsonDbClient>,
    env: &Env,
) -> Result<(config::SystemConfig, bool), String> {
    let mut cfg = config::load_config(db).await.map_err(|e| format!("the configuration could not be read ({e})"))?;
    let changed = cfg.openai_compat.native_model != model.id;
    if changed {
        cfg.openai_compat.set_model(OpenAiCompatPreset::Native, model.id);
        config::save_config(db, &cfg).await.map_err(|e| format!("the configuration could not be saved ({e})"))?;
    }
    // The record also when the configuration said it already: one that was
    // lost is written again.
    native_model::follow(env, &cfg.openai_compat)?;
    crate::provider::health::request_probe();
    Ok((cfg, changed))
}

/// What a load says, by whether a native model leads the session.
fn loaded_words(cfg: &config::SystemConfig, model: &NativeModel, changed: bool) -> String {
    let leads = ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native);
    match (leads, changed) {
        (true, true) => format!(
            "Native model set to {} (experimental). embrad loads it within seconds, and your next message waits for it.",
            model.id
        ),
        (true, false) => format!("{} is the native model already.", model.id),
        (false, true) => format!(
            "{} is asked for (experimental): embrad loads it within seconds. Switch to it with /provider native when ready.",
            model.id
        ),
        (false, false) => format!("{} is asked for already. Switch to it with /provider native when ready.", model.id),
    }
}

/// Load, and say it; the status bar gets the new Brain when a native model
/// leads the session.
async fn load_and_say(model: &'static NativeModel, tx: &mpsc::Sender<Result<ConversationResponse, Status>>, db: &Arc<WardsonDbClient>, hands: &NativeHands) {
    match load_native_model(model, db, &hands.env).await {
        Ok((cfg, changed)) => {
            say(tx, loaded_words(&cfg, model, changed), SystemMessageType::Info).await;
            if changed && ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native) {
                send_brain_status(tx, &cfg, &hands.session_mgr, model.id).await;
            }
        }
        Err(why) => say(tx, format!("{} was not loaded: {why}. Nothing was changed.", model.id), SystemMessageType::Error).await,
    }
}

/// Resolves when no turn is marked as running, looked at every second.
pub(super) async fn until_no_turn(in_turn: Arc<AtomicBool>) {
    while in_turn.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The pick of a native model. One whose file is on the machine and checked
/// is loaded now. Any other is installed first by the updater (a file that
/// is there is checked, a download that stopped goes on, a missing one is
/// downloaded), the console follows the job's lines, and the model is
/// loaded when the job is done: between turns, and whether or not the
/// console is still there.
pub(super) async fn pick_native_model(
    model: &'static NativeModel,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    hands: &NativeHands,
) {
    if contract::installed(model, &hands.env.models) == Installed::Verified {
        load_and_say(model, tx, db, hands).await;
        return;
    }
    let events = match hands.jobs.start(&hands.env, JobKind::Install, model) {
        Ok(events) => events,
        Err(running) => {
            let how_far = running.last_line.map(|line| format!(" ({line})")).unwrap_or_default();
            say(
                tx,
                format!(
                    "{} was not installed: the {} of {} runs{how_far}, and one job runs at a time. Pick again when it is done.",
                    model.id,
                    running.kind.noun(),
                    running.model.id
                ),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };
    say(
        tx,
        format!(
            "{} is not ready on this machine: its file ({} MB) is installed and checked first, and the lines below follow the updater. The session stays usable; the model is loaded when the job is done.",
            model.id,
            mb(model.bytes)
        ),
        SystemMessageType::Info,
    )
    .await;
    let (tx, db, hands) = (tx.clone(), db.clone(), hands.clone());
    tokio::spawn(async move { after_install(model, events, &tx, &db, &hands).await });
}

/// Follow an install to its end, then load the model or say why not.
async fn after_install(
    model: &'static NativeModel,
    mut events: broadcast::Receiver<JobEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    hands: &NativeHands,
) {
    let end = loop {
        match events.recv().await {
            // A console that left gets none of it; the job goes on.
            Ok(JobEvent::Line(line)) => say(tx, line, SystemMessageType::Info).await,
            Ok(JobEvent::Ended(end)) => break end,
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                break hands.jobs.last().map(|last| last.end).unwrap_or_else(|| JobEnd::Failed("its end was not heard".to_string()));
            }
        }
    };
    match end {
        JobEnd::Done | JobEnd::Nothing(_) => {
            until_no_turn(hands.in_turn.clone()).await;
            load_and_say(model, tx, db, hands).await;
        }
        JobEnd::Refused(why) | JobEnd::Failed(why) => {
            say(tx, format!("{} was not installed: {why}. The model in use is unchanged.", model.id), SystemMessageType::Error).await
        }
        JobEnd::Stopped => {
            say(
                tx,
                format!("The install of {} was stopped. What arrived is kept, and the next pick goes on from it. The model in use is unchanged.", model.id),
                SystemMessageType::Info,
            )
            .await
        }
    }
}

/// `/model` while a native model leads the session, and the model step of
/// `/provider --setup native`: no name opens the selector of the catalogue,
/// a name is the pick.
pub(super) async fn handle_native_model_command(
    wanted: &str,
    cfg: &config::SystemConfig,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending: &Arc<Mutex<Option<OpenAiCompatSetupState>>>,
    hands: NativeHands,
) {
    if wanted.is_empty() {
        let (prompt, state) = native_model_selection(non_empty(&cfg.openai_compat.native_model).as_deref(), hands);
        *pending.lock().await = Some(state);
        let _ = tx.send(Ok(ConversationResponse { response_type: Some(conversation_response::ResponseType::Setup(prompt)) })).await;
        return;
    }
    match native_model_choice(wanted) {
        Ok(model) => pick_native_model(model, tx, db, &hands).await,
        Err(why) => say(tx, why, SystemMessageType::Error).await,
    }
}

/// What `/effort` does while a native model leads the session, and says:
/// the change, if any, is made in `oc`; `true` when there is one to save.
/// The models think or they do not: there are no levels to send. The
/// stored word for "off" is the level `low`, so that the one set of levels
/// stays the configuration's.
pub(super) fn apply_native_effort(args: &str, oc: &mut config::OpenAiCompatConfig) -> (bool, String, SystemMessageType) {
    use crate::provider::openai_compat::native::{EFFORT_OFF, thinks};
    let Some(model) = non_empty(&oc.native_model) else {
        return (false, config::PresetNotReady::NoModel(OpenAiCompatPreset::Native).to_string(), SystemMessageType::Error);
    };
    let thinking = thinks(oc.effort_for_preset(OpenAiCompatPreset::Native));
    let how = "A native model has no levels (experimental): /effort off answers without thinking first, /effort on thinks first.";
    let word = args.trim().to_ascii_lowercase();
    match word.as_str() {
        "" => {
            let now = if thinking { "on (the default)" } else { "off" };
            (false, format!("Thinking is {now} for {model}. {how} A change takes effect on your next message."), SystemMessageType::Info)
        }
        "off" | "low" => {
            oc.set_effort(OpenAiCompatPreset::Native, Some(EFFORT_OFF.to_string()));
            (
                true,
                format!("Thinking is off for {model}: it answers without thinking first. /effort on brings it back. Takes effect on your next message."),
                SystemMessageType::Info,
            )
        }
        "on" | "reset" => {
            oc.set_effort(OpenAiCompatPreset::Native, None);
            (true, format!("Thinking is on for {model} (the default). Takes effect on your next message."), SystemMessageType::Info)
        }
        level if parse_effort_choice(level).is_some() => {
            oc.set_effort(OpenAiCompatPreset::Native, None);
            (
                true,
                format!("Thinking is on for {model}. A native model has no levels (experimental): any level but low is on, and low is off. Takes effect on your next message."),
                SystemMessageType::Info,
            )
        }
        other => (false, format!("'{other}' is not on or off. {how}"), SystemMessageType::Error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_model::test_support::machine;
    use embra_boot::native_model::{Verified, Wanted, by_id};
    use std::ffi::OsString;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn four() -> &'static NativeModel {
        by_id("qwen3.5-4b").unwrap()
    }

    fn nine() -> &'static NativeModel {
        by_id("qwen3.5-9b").unwrap()
    }

    /// A database that holds a configuration on `provider` with
    /// `native_model` kept loaded, and takes its saves.
    async fn config_db(provider: &str, native_model: &str) -> MockServer {
        let server = MockServer::start().await;
        let mut cfg = serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase2",
            "created_at": "", "version": "test", "api_provider": provider,
        });
        if !native_model.is_empty() {
            cfg["openai_compat"] = serde_json::json!({"native_model": native_model});
        }
        let envelope = |data: serde_json::Value| ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "meta": {}, "data": data}));
        Mock::given(method("GET")).and(path("/config.system/docs/config")).respond_with(envelope(cfg)).mount(&server).await;
        Mock::given(method("GET")).and(path("/config.system")).respond_with(envelope(serde_json::json!({}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/config.system/docs")).respond_with(envelope(serde_json::json!({"_id": "config"}))).mount(&server).await;
        server
    }

    /// The native model of each configuration saved, in order.
    async fn saved_models(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| {
                let doc: serde_json::Value = serde_json::from_slice(&r.body).expect("a configuration");
                doc["openai_compat"]["native_model"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    }

    fn hands(env: &Env) -> NativeHands {
        NativeHands {
            env: env.clone(),
            jobs: Jobs::default(),
            in_turn: Arc::new(AtomicBool::new(false)),
            session_mgr: Arc::new(RwLock::new(SessionManager::new(WardsonDbClient::from_url("http://127.0.0.1:1")))),
        }
    }

    /// The model's file as the updater leaves it: of the catalogue's size
    /// (sparse: no byte is written) with the record that vouches for it.
    fn install(model: &'static NativeModel, env: &Env) {
        std::fs::create_dir_all(model.dir(&env.models)).unwrap();
        let file = std::fs::File::create(model.file_path(&env.models)).unwrap();
        file.set_len(model.bytes).unwrap();
        let meta = std::fs::metadata(model.file_path(&env.models)).unwrap();
        contract::write_verified(&model.verified_path(&env.models), &Verified::of(model, &meta)).unwrap();
        assert_eq!(contract::installed(model, &env.models), Installed::Verified);
    }

    /// A stand-in for the updater, run as `/bin/sh <script>` (see the jobs'
    /// own tests).
    fn stand_in(root: &std::path::Path, body: &str) -> Vec<OsString> {
        let script = root.join("updater.sh");
        std::fs::write(&script, format!("{body}\n")).unwrap();
        vec![OsString::from("/bin/sh"), script.into_os_string()]
    }

    /// What a console is told next, in two minutes at most: a deadline no
    /// loaded machine reaches when the job works.
    async fn told(rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>) -> String {
        let frame = tokio::time::timeout(Duration::from_secs(120), rx.recv()).await.expect("something is said").expect("the console is attached").unwrap();
        match frame.response_type {
            Some(conversation_response::ResponseType::System(m)) => m.content,
            Some(conversation_response::ResponseType::ModeChange(m)) => format!("[status] {}", m.message),
            other => panic!("neither a message nor the status bar: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_selector_offers_the_whole_catalogue_with_the_model_kept_loaded_first() {
        let (_root, env) = machine("pick-selector");
        install(four(), &env);
        let (prompt, state) = native_model_selection(Some("qwen3.5-9b"), hands(&env));
        assert_eq!(prompt.field_type, SetupFieldType::Selector as i32);
        assert_eq!(prompt.options, ["qwen3.5-9b", "qwen3.5-4b"], "the model kept loaded first");
        assert_eq!(prompt.default_value, "qwen3.5-9b");
        assert_eq!(
            prompt.prompt,
            "Select a native model (experimental). qwen3.5-4b is Qwen3.5 4B (Q4_K_M, 2613 MB): installed; qwen3.5-9b is Qwen3.5 9B (Q4_K_M, 5417 MB): not installed, downloaded first."
        );
        assert!(matches!(state.preset, OpenAiCompatPreset::Native) && state.model_only && state.native.is_some());
        assert!(matches!(&state.step, OpenAiCompatSetupStep::AwaitingModelSelection { models, .. } if models == &prompt.options));

        // None kept loaded: the catalogue's own order.
        let (prompt, _) = native_model_selection(None, hands(&env));
        assert_eq!(prompt.options, ["qwen3.5-4b", "qwen3.5-9b"]);
        // Every model of the catalogue is offered, and each is a name `/model` takes.
        for model in &contract::CATALOGUE {
            assert!(prompt.options.iter().any(|id| id == model.id), "{}", model.id);
            assert_eq!(native_model_choice(model.id).map(|m| m.id), Ok(model.id));
            assert_eq!(native_model_choice(&model.id.to_uppercase()).map(|m| m.id), Ok(model.id));
        }
        assert_eq!(
            native_model_choice("qwen3.5-27b").unwrap_err(),
            "'qwen3.5-27b' is not a native model. The catalogue (experimental): qwen3.5-4b, qwen3.5-9b. /model alone picks from the list."
        );

        // A download that stopped, and a file nobody checked.
        std::fs::create_dir_all(nine().dir(&env.models)).unwrap();
        std::fs::File::create(nine().partial_path(&env.models)).unwrap().set_len(1024 * 1024 * 1024).unwrap();
        assert_eq!(file_words(nine(), &env), "1024 of 5417 MB downloaded, the rest is downloaded first");
        std::fs::remove_file(four().verified_path(&env.models)).unwrap();
        assert!(file_words(four(), &env).starts_with("its file is checked first ("), "{}", file_words(four(), &env));
    }

    /// A model whose file is installed is asked for at once: the stored
    /// configuration names it, the record embrad follows says it, and the
    /// console reads which way to go on.
    #[tokio::test]
    async fn a_pick_of_an_installed_model_is_loaded_now() {
        let (_root, env) = machine("pick-installed");
        install(four(), &env);
        // Another provider leads the session.
        let db_server = config_db("anthropic", "").await;
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let (tx, mut rx) = mpsc::channel(16);
        pick_native_model(four(), &tx, &db, &hands(&env)).await;
        assert_eq!(told(&mut rx).await, "qwen3.5-4b is asked for (experimental): embrad loads it within seconds. Switch to it with /provider native when ready.");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-4b"]);
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(four()))));
        assert!(rx.try_recv().is_err(), "the status bar is another provider's");

        // A native model leads it: the next message uses the pick, and the status bar says so.
        install(nine(), &env);
        let db_server = config_db("native", "qwen3.5-4b").await;
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        pick_native_model(nine(), &tx, &db, &hands(&env)).await;
        assert_eq!(told(&mut rx).await, "Native model set to qwen3.5-9b (experimental). embrad loads it within seconds, and your next message waits for it.");
        assert_eq!(told(&mut rx).await, "[status] Operational — Name: Embra — Session: <none> — TZ: Etc/UTC — Brain: qwen3.5-9b");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-9b"]);
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(nine()))));

        // The model it has already: nothing is saved, and a record that was lost is written again.
        std::fs::remove_file(&env.settings).unwrap();
        pick_native_model(four(), &tx, &db, &hands(&env)).await;
        assert_eq!(told(&mut rx).await, "qwen3.5-4b is the native model already.");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-9b"], "no second save");
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(four()))));
    }

    /// A model whose file is not on the machine is installed first: the
    /// console follows the updater's lines, and the model is asked for when
    /// the job is done. Until then nothing is changed.
    #[tokio::test]
    async fn a_pick_that_is_not_installed_installs_and_then_loads() {
        let (root, mut env) = machine("pick-installs");
        let gate = root.join("go-on");
        env.updater = stand_in(
            &root,
            &format!(
                "echo \"$2: downloading its file\"\necho \"$2: received 270 of 5417 MB (5%)\"\nuntil [ -e '{}' ]; do sleep 0.02; done\necho \"$2: installed and verified\"",
                gate.display()
            ),
        );
        let db_server = config_db("native", "qwen3.5-4b").await;
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let hands = hands(&env);
        let (tx, mut rx) = mpsc::channel(16);
        pick_native_model(nine(), &tx, &db, &hands).await;
        assert_eq!(
            told(&mut rx).await,
            "qwen3.5-9b is not ready on this machine: its file (5417 MB) is installed and checked first, and the lines below follow the updater. The session stays usable; the model is loaded when the job is done."
        );
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: downloading its file");
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5417 MB (5%)");
        assert!(saved_models(&db_server).await.is_empty(), "the model in use stays until the file is there");
        assert_eq!(contract::read_settings(&env.settings), Ok(None));

        // A second pick while it runs: one job at a time, and how far that one is.
        let (other, mut other_rx) = mpsc::channel(4);
        pick_native_model(nine(), &other, &db, &hands).await;
        assert_eq!(
            told(&mut other_rx).await,
            "qwen3.5-9b was not installed: the download of qwen3.5-9b runs (qwen3.5-9b: received 270 of 5417 MB (5%)), and one job runs at a time. Pick again when it is done."
        );

        std::fs::write(&gate, "").unwrap();
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: installed and verified");
        assert_eq!(told(&mut rx).await, "Native model set to qwen3.5-9b (experimental). embrad loads it within seconds, and your next message waits for it.");
        assert_eq!(told(&mut rx).await, "[status] Operational — Name: Embra — Session: <none> — TZ: Etc/UTC — Brain: qwen3.5-9b");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-9b"]);
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(nine()))));
    }

    /// The console that picked may be gone when the download ends: the
    /// model is loaded all the same. A job that fails changes nothing and
    /// says why.
    #[tokio::test]
    async fn an_install_ends_without_its_console_and_a_failed_one_changes_nothing() {
        let (root, mut env) = machine("pick-ends");
        env.updater = stand_in(&root, "echo \"$2: installed and verified\"");
        let db_server = config_db("anthropic", "").await;
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        let hands = hands(&env);
        let (tx, rx) = mpsc::channel(16);
        drop(rx);
        pick_native_model(four(), &tx, &db, &hands).await;
        // Nobody is told; the end is found in the brain's jobs, as a console that asks would.
        let mut follow = hands.jobs.follow();
        if let Some(events) = &mut follow {
            while !matches!(tokio::time::timeout(Duration::from_secs(120), events.recv()).await.expect("the job ends"), Ok(JobEvent::Ended(_)) | Err(_)) {}
        }
        // The load follows the job's end in the same task; its save is the sign.
        let saved = async {
            while saved_models(&db_server).await.is_empty() {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(120), saved).await.expect("the model is asked for");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-4b"]);

        // The source did not have the file: nothing is changed.
        env.updater = stand_in(&root, "echo 'the source answered 404 for the file' >&2; exit 1");
        let failing = NativeHands { env: env.clone(), ..hands.clone() };
        let (tx, mut rx) = mpsc::channel(16);
        pick_native_model(nine(), &tx, &db, &failing).await;
        assert!(told(&mut rx).await.starts_with("qwen3.5-9b is not ready on this machine:"));
        assert_eq!(told(&mut rx).await, "qwen3.5-9b was not installed: the source answered 404 for the file. The model in use is unchanged.");
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-4b"], "no save for the model that did not come");

        // The operator stopped it.
        let pid_file = root.join("pid");
        env.updater = stand_in(&root, &format!("echo $$ > '{}'\necho \"$2: received 270 of 5417 MB (5%)\"\nwhile :; do sleep 0.02; done", pid_file.display()));
        let stopping = NativeHands { env: env.clone(), ..hands.clone() };
        pick_native_model(nine(), &tx, &db, &stopping).await;
        assert!(told(&mut rx).await.starts_with("qwen3.5-9b is not ready on this machine:"));
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5417 MB (5%)");
        assert!(stopping.jobs.stop().is_some());
        assert_eq!(
            told(&mut rx).await,
            "The install of qwen3.5-9b was stopped. What arrived is kept, and the next pick goes on from it. The model in use is unchanged."
        );
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-4b"]);
    }

    /// A model that had to be downloaded first is asked for between turns:
    /// embrad ends the server of the model before it, and a turn on that
    /// one would be cut.
    #[tokio::test(start_paused = true)]
    async fn a_model_that_came_by_download_waits_for_the_turn_in_flight_to_end() {
        let in_turn = Arc::new(AtomicBool::new(true));
        let waiting = tokio::spawn(until_no_turn(in_turn.clone()));
        // An hour of the turn: the wait is still there.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(!waiting.is_finished());
        in_turn.store(false, Ordering::SeqCst);
        let start = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(24 * 3600), waiting).await.expect("the wait ends with the turn").unwrap();
        assert!(start.elapsed() <= Duration::from_secs(1), "at its next look");
        // No turn: no wait.
        let start = tokio::time::Instant::now();
        until_no_turn(Arc::new(AtomicBool::new(false))).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn effort_on_a_native_model_is_thinking_on_or_off() {
        let mut oc = config::OpenAiCompatConfig::default();
        let (changed, said, kind) = apply_native_effort("", &mut oc);
        assert_eq!((changed, kind), (false, SystemMessageType::Error));
        assert_eq!(said, "No native model is chosen. /native lists the models, and /native use <model> loads one (experimental).");

        oc.set_model(OpenAiCompatPreset::Native, "qwen3.5-4b");
        let thinking = |oc: &config::OpenAiCompatConfig| crate::provider::openai_compat::native::thinks(oc.effort_for_preset(OpenAiCompatPreset::Native));
        assert!(thinking(&oc), "on by default");
        let (changed, said, kind) = apply_native_effort("", &mut oc);
        assert_eq!((changed, kind), (false, SystemMessageType::Info));
        assert_eq!(
            said,
            "Thinking is on (the default) for qwen3.5-4b. A native model has no levels (experimental): /effort off answers without thinking first, /effort on thinks first. A change takes effect on your next message."
        );

        for off in ["off", "OFF", "low", " low "] {
            oc.set_effort(OpenAiCompatPreset::Native, None);
            let (changed, said, kind) = apply_native_effort(off, &mut oc);
            assert_eq!((changed, kind), (true, SystemMessageType::Info), "{off}");
            assert_eq!(said, "Thinking is off for qwen3.5-4b: it answers without thinking first. /effort on brings it back. Takes effect on your next message.");
            assert!(!thinking(&oc), "{off}");
            assert_eq!(oc.native_effort_by_model.get("qwen3.5-4b").map(String::as_str), Some("low"), "stored as the level");
        }
        assert!(apply_native_effort("", &mut oc).1.starts_with("Thinking is off for qwen3.5-4b. "));

        for on in ["on", "reset"] {
            oc.set_effort(OpenAiCompatPreset::Native, Some("low".into()));
            let (changed, said, _) = apply_native_effort(on, &mut oc);
            assert!(changed && thinking(&oc), "{on}");
            assert_eq!(said, "Thinking is on for qwen3.5-4b (the default). Takes effect on your next message.");
            assert!(oc.native_effort_by_model.is_empty(), "nothing is stored for the default");
        }
        for level in ["medium", "high", "xhigh", "max"] {
            oc.set_effort(OpenAiCompatPreset::Native, Some("low".into()));
            let (changed, said, kind) = apply_native_effort(level, &mut oc);
            assert_eq!((changed, kind), (true, SystemMessageType::Info));
            assert!(thinking(&oc), "{level}");
            assert!(said.contains("any level but low is on"), "{said}");
        }
        let (changed, said, kind) = apply_native_effort("turbo", &mut oc);
        assert_eq!((changed, kind), (false, SystemMessageType::Error));
        assert!(said.starts_with("'turbo' is not on or off. A native model has no levels"), "{said}");

        // The level is the model's own: the other model thinks, and the first keeps its.
        oc.set_effort(OpenAiCompatPreset::Native, Some("low".into()));
        oc.set_model(OpenAiCompatPreset::Native, "qwen3.5-9b");
        assert!(thinking(&oc));
        oc.set_model(OpenAiCompatPreset::Native, "qwen3.5-4b");
        assert!(!thinking(&oc));
    }
}
