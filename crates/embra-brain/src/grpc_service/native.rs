//! Native models (experimental) in the operator's commands.
//!
//! The pick of a model: `/model` while a native model leads the session and
//! `/provider --setup native` offer the catalogue (`native_model_selection`),
//! and `/native use` takes a model's id; the pick asks embrad to keep the
//! model loaded (`pick_native_model`), after its file was downloaded and
//! checked when it is not on the machine.
//!
//! `/native` itself: the models' files and the jobs on them, which are the
//! machine's matters and are answered before a session too, and what the
//! stored configuration keeps loaded (`use`, `off`, `context`), in a
//! session.

use super::*;
use crate::native_model::jobs::{JobEnd, JobEvent, JobKind, Jobs};
use crate::native_model::{self, Env, mb};
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
pub(super) fn native_model_selection(current: Option<&str>, oc: &config::OpenAiCompatConfig, hands: NativeHands) -> (SetupPrompt, OpenAiCompatSetupState) {
    let (question, ids) = native_model::catalogue_question(current, &hands.env, &|model| oc.native_context(model));
    let prompt = SetupPrompt {
        field_type: SetupFieldType::Selector as i32,
        prompt: question,
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

/// What asking for a model came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Loaded {
    /// Another model than before, or the first.
    Changed,
    /// The model it was already.
    AsItWas,
    /// The model it was already, whose server embrad no longer started: it
    /// is started again (`native_model::ask_again_if_halted`).
    Restarted,
}

/// Ask for `model` to be kept loaded: the stored configuration names it,
/// the record embrad follows is written from it, and the health probe is
/// asked to look. Returns the configuration and what the asking came to.
pub(super) async fn load_native_model(
    model: &'static NativeModel,
    db: &Arc<WardsonDbClient>,
    env: &Env,
) -> Result<(config::SystemConfig, Loaded), String> {
    let mut cfg = config::load_config(db).await.map_err(|e| format!("the configuration could not be read ({e})"))?;
    let changed = cfg.openai_compat.native_model != model.id;
    if changed {
        cfg.openai_compat.set_model(OpenAiCompatPreset::Native, model.id);
        config::save_config(db, &cfg).await.map_err(|e| format!("the configuration could not be saved ({e})"))?;
    }
    // The record also when the configuration said it already: one that was
    // lost is written again.
    native_model::follow(env, &cfg.openai_compat)?;
    let loaded = match changed {
        true => Loaded::Changed,
        false if native_model::ask_again_if_halted(env).await? => Loaded::Restarted,
        false => Loaded::AsItWas,
    };
    crate::provider::health::request_probe();
    Ok((cfg, loaded))
}

/// What a load says, by whether a native model leads the session.
fn loaded_words(cfg: &config::SystemConfig, model: &NativeModel, loaded: Loaded) -> String {
    let leads = ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native);
    let switch = "Switch to it with /provider native when ready.";
    match (leads, loaded) {
        (true, Loaded::Changed) => format!(
            "Native model set to {} (experimental). embrad loads it within seconds, and your next message waits for it.",
            model.id
        ),
        (true, Loaded::AsItWas) => format!("{} is the native model already.", model.id),
        (false, Loaded::Changed) => format!("{} is asked for (experimental): embrad loads it within seconds. {switch}", model.id),
        (false, Loaded::AsItWas) => format!("{} is asked for already. {switch}", model.id),
        (leads, Loaded::Restarted) => format!(
            "{} is asked for again: its model server had ended three times before it answered and was no longer started. embrad starts it now{}",
            model.id,
            if leads { ", and your next message waits for it.".to_string() } else { format!(". {switch}") }
        ),
    }
}

/// What is said of memory when `model` is asked for with a window, on a
/// machine that is short for it (`native_model::memory_note`), as a warning.
async fn say_of_memory(
    model: &NativeModel,
    context: u32,
    machine: Option<native_model::MachineMemory>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    env: &Env,
) {
    if let Some(note) = native_model::memory_note(model, context, native_model::would_read_images(model, env), machine, true) {
        say(tx, note.text, SystemMessageType::Warning).await;
    }
}

/// Load, and say it; the status bar gets the new Brain when a native model
/// leads the session. With `of_memory` a machine that is short for the
/// model is told so; a load that follows an install was told when the
/// install began.
async fn load_and_say(
    model: &'static NativeModel,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    hands: &NativeHands,
    of_memory: bool,
) {
    // Read before the load is asked for: the model server that runs now
    // still holds what the next one gets.
    let machine = native_model::machine_memory(&hands.env);
    match load_native_model(model, db, &hands.env).await {
        Ok((cfg, loaded)) => {
            say(tx, loaded_words(&cfg, model, loaded), SystemMessageType::Info).await;
            if of_memory {
                say_of_memory(model, cfg.openai_compat.native_context(model), machine, tx, &hands.env).await;
            }
            if loaded == Loaded::Changed && ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native) {
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
        load_and_say(model, tx, db, hands, true).await;
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
            "{} is not ready on this machine: its files ({} MB, and {} MB to read images) are installed and checked first, and the lines below follow the updater. The session stays usable; the model is loaded when the job is done.",
            model.id,
            mb(model.bytes),
            mb(model.images.bytes)
        ),
        SystemMessageType::Info,
    )
    .await;
    // Said now, while the download can still be stopped (/native cancel),
    // and not again when it is done.
    let context = config::load_config(db).await.map(|cfg| cfg.openai_compat.native_context(model)).unwrap_or(model.default_context);
    say_of_memory(model, context, native_model::machine_memory(&hands.env), tx, &hands.env).await;
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
            load_and_say(model, tx, db, hands, false).await;
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
        let (prompt, state) = native_model_selection(non_empty(&cfg.openai_compat.native_model).as_deref(), &cfg.openai_compat, hands);
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
/// The words are the model's family's (`native::Dialect`): a family
/// without levels thinks or does not, and its stored word for "off" is the
/// level `low`, so that the one set of levels stays the configuration's; a
/// family with levels takes its own, and no other word reaches the server.
pub(super) fn apply_native_effort(args: &str, oc: &mut config::OpenAiCompatConfig) -> (bool, String, SystemMessageType) {
    let Some(model) = non_empty(&oc.native_model) else {
        return (false, config::PresetNotReady::NoModel(OpenAiCompatPreset::Native).to_string(), SystemMessageType::Error);
    };
    native_effort(crate::provider::openai_compat::native::family_of(&model), &model, args, oc)
}

/// [`apply_native_effort`] for a model of `family`.
fn native_effort(family: contract::Family, model: &str, args: &str, oc: &mut config::OpenAiCompatConfig) -> (bool, String, SystemMessageType) {
    use crate::provider::openai_compat::native::{Thinks, dialect, thinking};
    let dialect = dialect(family);
    let word = args.trim().to_ascii_lowercase();
    let next = "Takes effect on your next message.";
    let Some(own) = dialect.own_level else {
        // On or off, and nothing between.
        let how = "A native model has no levels (experimental): /effort off answers without thinking first, /effort on thinks first.";
        return match word.as_str() {
            "" => {
                let now = if thinking(family, oc.effort_for_preset(OpenAiCompatPreset::Native)) == Thinks::Off { "off" } else { "on (the default)" };
                (false, format!("Thinking is {now} for {model}. {how} A change takes effect on your next message."), SystemMessageType::Info)
            }
            "off" | "low" => {
                oc.set_effort(OpenAiCompatPreset::Native, Some(dialect.off.to_string()));
                (
                    true,
                    format!("Thinking is off for {model}: it answers without thinking first. /effort on brings it back. {next}"),
                    SystemMessageType::Info,
                )
            }
            "on" | "reset" => {
                oc.set_effort(OpenAiCompatPreset::Native, None);
                (true, format!("Thinking is on for {model} (the default). {next}"), SystemMessageType::Info)
            }
            level if parse_effort_choice(level).is_some() => {
                oc.set_effort(OpenAiCompatPreset::Native, None);
                (
                    true,
                    format!("Thinking is on for {model}. A native model has no levels (experimental): any level but low is on, and low is off. {next}"),
                    SystemMessageType::Info,
                )
            }
            other => (false, format!("'{other}' is not on or off. {how}"), SystemMessageType::Error),
        };
    };
    // Levels of its own: the template refuses any other word, so none is stored.
    let levels = dialect.levels.join(", ");
    let how = format!(
        "This native model (experimental) thinks at one of its levels ({levels}): /effort <level> sets one, /effort on is its own ({own}), and /effort off answers without thinking first."
    );
    match word.as_str() {
        "" => {
            let now = match thinking(family, oc.effort_for_preset(OpenAiCompatPreset::Native)) {
                Thinks::Off => "off".to_string(),
                Thinks::On(None) => format!("on at the model's own level, {own} (the default)"),
                Thinks::On(Some(level)) => format!("on at {level}"),
            };
            (false, format!("Thinking is {now} for {model}. {how} A change takes effect on your next message."), SystemMessageType::Info)
        }
        "off" => {
            oc.set_effort(OpenAiCompatPreset::Native, Some(dialect.off.to_string()));
            (
                true,
                format!("Thinking is off for {model}: it answers without thinking first. /effort on brings it back. {next}"),
                SystemMessageType::Info,
            )
        }
        "on" | "reset" => {
            oc.set_effort(OpenAiCompatPreset::Native, None);
            (true, format!("Thinking is on for {model} at its own level, {own} (the default). {next}"), SystemMessageType::Info)
        }
        level if dialect.levels.contains(&level) => {
            oc.set_effort(OpenAiCompatPreset::Native, Some(level.to_string()));
            (true, format!("Thinking is on for {model} at {level}. {next}"), SystemMessageType::Info)
        }
        level if parse_effort_choice(level).is_some() => {
            (false, format!("'{level}' is no level of {model}: it takes {levels}. Nothing was changed."), SystemMessageType::Error)
        }
        other => (false, format!("'{other}' is not on, off or a level. {how}"), SystemMessageType::Error),
    }
}

// ── /native ──────────────────────────────────────────────────────────────

/// The catalogue's ids, as a usage or a refusal names them.
fn catalogue_ids() -> String {
    contract::CATALOGUE.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
}

/// What `/native help` answers.
pub(super) fn native_usage() -> String {
    format!(
        "Usage: /native | /native install <model> | /native import | /native use <model> | /native off | /native context [<tokens>|reset] | /native remove <model> | /native cancel\n  \
         A native model (experimental) runs on this machine; the catalogue: {}. /native shows the models, what is kept loaded, the model server and the job that runs. \
         install downloads a model's two files, the model file and the file it reads images through, and checks them; \
         import says how a file comes in through the web console, for a machine that cannot reach the source. \
         use loads a model, installing it first when it is not there, and off frees its memory: \
         both whichever provider leads the session, and /provider native switches to a loaded model. \
         context is the window the loaded model is given. remove deletes a model's files. cancel stops the job that runs.",
        catalogue_ids()
    )
}

/// What `/native use`, `off` and `context` answer before a session: they
/// change the stored configuration, which the setup is still writing.
pub(super) const NATIVE_NOT_BEFORE_A_SESSION: &str = "Not run before setup is complete: /native use, off and context change the stored configuration, and the setup's own question chooses the model. /native, /native install, /native import, /native remove and /native cancel are answered now.";

/// What `/native import` answers. The upload itself is the web console's
/// (`PUT /api/models/<id>`, embra-web's `models.rs`): the brain holds
/// neither the file nor the console's token, and never prints the console's
/// URL. The source of each file is the catalogue's, where another machine
/// can fetch it.
pub(super) fn import_instructions() -> String {
    let files: Vec<String> = contract::CATALOGUE
        .iter()
        .map(|m| {
            format!(
                "for {}, {} ({} MB, from {}) and, to read images, {} ({} MB, from {})",
                m.id,
                m.file,
                mb(m.bytes),
                m.url(),
                m.images.file,
                mb(m.images.bytes),
                m.url_of(contract::Part::Images)
            )
        })
        .collect();
    format!(
        "A model's files come in through the web console, from a browser on another machine (experimental): in the sidebar, /native with the action import, then a file. \
         It is for a machine that cannot reach the catalogue's source. A model has two files, each sent by itself: {}. \
         The machine tells which file of which model it is by its name and size, checks it against the catalogue's sha256 while it arrives, and keeps nothing of a file that is not the catalogue's. \
         A model runs once its model file is there and reads images once its image file is. /native use <model> loads it then.",
        files.join("; ")
    )
}

/// The forms of `/native` that work on the models' files and the jobs: the
/// machine's matters, answered before a session too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineAction {
    Show,
    Help,
    Install(&'static NativeModel),
    /// How a model's file comes in by upload: said, not done here.
    Import,
    Remove(&'static NativeModel),
    Cancel,
}

/// The forms that change what the stored configuration keeps loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredAction {
    Use(&'static NativeModel),
    Off,
    Context(ContextAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextAction {
    Show,
    Set(u32),
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NativeAction {
    Machine(MachineAction),
    Stored(StoredAction),
    /// What was wrong with the command, in a sentence.
    Refused(String),
}

/// Read `/native`'s arguments; pure.
pub(super) fn parse_native(args: &str) -> NativeAction {
    let mut words = args.split_whitespace();
    let verb = words.next().unwrap_or("").to_ascii_lowercase();
    let (first, more) = (words.next(), words.next().is_some());
    let refused = |what: String| NativeAction::Refused(format!("{what} /native help shows the forms."));
    // A form that takes nothing after its word.
    let alone = |action: NativeAction| match first {
        None => action,
        Some(_) => refused(format!("/native {verb} takes nothing after it.")),
    };
    // A form that takes one model of the catalogue.
    let model = |action: fn(&'static NativeModel) -> NativeAction| match (first, more) {
        (Some(name), false) => match contract::CATALOGUE.iter().find(|m| m.id.eq_ignore_ascii_case(name)) {
            Some(model) => action(model),
            None => refused(format!("'{name}' is not a native model; the catalogue: {}.", catalogue_ids())),
        },
        _ => refused(format!("/native {verb} takes one model: {}.", catalogue_ids())),
    };
    match verb.as_str() {
        "" | "status" | "list" => alone(NativeAction::Machine(MachineAction::Show)),
        "help" => alone(NativeAction::Machine(MachineAction::Help)),
        "import" => alone(NativeAction::Machine(MachineAction::Import)),
        "cancel" => alone(NativeAction::Machine(MachineAction::Cancel)),
        "off" => alone(NativeAction::Stored(StoredAction::Off)),
        "install" => model(|m| NativeAction::Machine(MachineAction::Install(m))),
        "remove" => model(|m| NativeAction::Machine(MachineAction::Remove(m))),
        "use" => model(|m| NativeAction::Stored(StoredAction::Use(m))),
        "context" => match (first, more) {
            (None, _) => NativeAction::Stored(StoredAction::Context(ContextAction::Show)),
            (Some(word), false) if word.eq_ignore_ascii_case("reset") => NativeAction::Stored(StoredAction::Context(ContextAction::Reset)),
            // 65536, or 65,536 as a figure is written.
            (Some(word), false) => match word.replace([',', '_'], "").parse::<u32>() {
                Ok(tokens) => NativeAction::Stored(StoredAction::Context(ContextAction::Set(tokens))),
                Err(_) => refused(format!("'{}' is not a number of tokens.", word.chars().take(40).collect::<String>())),
            },
            (Some(_), true) => refused("/native context takes one number of tokens, or reset.".to_string()),
        },
        other => refused(format!("'{}' is not a form of /native.", other.chars().take(40).collect::<String>())),
    }
}

/// How a job ended, for the line that shows the last one.
fn end_words(end: &JobEnd) -> String {
    match end {
        JobEnd::Done => "done".to_string(),
        JobEnd::Nothing(line) => line.clone(),
        JobEnd::Refused(why) | JobEnd::Failed(why) => format!("it did not finish ({why})"),
        JobEnd::Stopped => "stopped".to_string(),
    }
}

/// `/native`: the models' files as the updater lists them, what embrad is
/// asked to keep loaded, what it says of the model server, and the job that
/// runs or ran last. `cfg` is the stored configuration when there is one:
/// before a session there may be none, and the record says what is asked.
pub(super) async fn show_native(env: &Env, jobs: &Jobs, cfg: Option<&config::SystemConfig>) -> String {
    let mut lines = vec!["Native models (experimental): a model that runs on this machine.".to_string()];
    match native_model::jobs::list(env).await {
        Ok(listed) => lines.extend(listed.lines().map(|line| format!("  {line}"))),
        Err(why) => lines.push(format!("  The models' files could not be listed: {why}.")),
    }
    let (wanted, _) = env.records();
    lines.push(match wanted {
        Ok(Some(wanted)) => {
            let thinking = cfg.map(|cfg| {
                use crate::provider::openai_compat::native::{Thinks, thinking};
                match thinking(wanted.model.family, cfg.openai_compat.native_effort_by_model.get(wanted.model.id).map(String::as_str)) {
                    Thinks::Off => ", thinking off".to_string(),
                    Thinks::On(None) => ", thinking on".to_string(),
                    Thinks::On(Some(level)) => format!(", thinking on at {level}"),
                }
            });
            let leads = cfg.is_some_and(|cfg| ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native));
            format!(
                "  Kept loaded: {}, context window {}{}.{}",
                wanted.model.id,
                wanted.context,
                thinking.unwrap_or_default(),
                if leads { " It leads the session." } else { "" }
            )
        }
        Ok(None) => "  Kept loaded: none. /native use <model> loads one.".to_string(),
        Err(why) => format!("  Kept loaded: the settings cannot be read ({why})."),
    });
    if let Some(words) = native_model::server_words_with_backend(env) {
        lines.push(format!("  The model server: {words}."));
    }
    match (jobs.running(), jobs.last()) {
        (Some(running), _) => {
            let how_far = running.last_line.map(|line| format!(": {line}")).unwrap_or_default();
            lines.push(format!("  The {} of {} runs{how_far}. /native cancel stops it.", running.kind.noun(), running.model.id));
        }
        (None, _) if contract::job_running(&env.models) => {
            lines.push("  A model job runs that this brain did not start: an upload through the web console.".to_string());
        }
        (None, Some(last)) => lines.push(format!("  The last job, the {} of {}: {}.", last.kind.noun(), last.model.id, end_words(&last.end))),
        (None, None) => {}
    }
    lines.push("  /native help shows the forms.".to_string());
    lines.join("\n")
}

/// Start a job on a model's file and follow it for the console that asked.
async fn start_job(kind: JobKind, model: &'static NativeModel, tx: &mpsc::Sender<Result<ConversationResponse, Status>>, env: &Env, jobs: &Jobs) {
    let events = match jobs.start(env, kind, model) {
        Ok(events) => events,
        Err(running) => {
            let how_far = running.last_line.map(|line| format!(" ({line})")).unwrap_or_default();
            say(
                tx,
                format!(
                    "Not started: the {} of {} runs{how_far}, and one job runs at a time. /native cancel stops it.",
                    running.kind.noun(),
                    running.model.id
                ),
                SystemMessageType::Error,
            )
            .await;
            return;
        }
    };
    let started = match kind {
        JobKind::Install => format!(
            "Installing {}: its model file ({} MB) and its image file ({} MB) are downloaded and checked, a file that is there already is left alone, and the lines below follow the updater. The session stays usable; /native cancel stops it.",
            model.id,
            mb(model.bytes),
            mb(model.images.bytes)
        ),
        JobKind::Remove => format!("Removing the files of {}.", model.id),
    };
    say(tx, started, SystemMessageType::Info).await;
    let (tx, env) = (tx.clone(), env.clone());
    tokio::spawn(async move { follow_to_the_end(kind, model, events, &tx, &env).await });
}

/// A job's lines to the console that started it, then how it ended.
async fn follow_to_the_end(
    kind: JobKind,
    model: &'static NativeModel,
    mut events: broadcast::Receiver<JobEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    env: &Env,
) {
    let end = loop {
        match events.recv().await {
            Ok(JobEvent::Line(line)) => say(tx, line, SystemMessageType::Info).await,
            Ok(JobEvent::Ended(end)) => break end,
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    };
    let (said, how) = match (kind, end) {
        // An install for the model that is kept loaded brought a file the
        // model server was started without (its image file): embrad sees it
        // and starts the server again.
        (JobKind::Install, JobEnd::Done) if env.records().0.is_ok_and(|wanted| wanted.is_some_and(|wanted| wanted.model.id == model.id)) => (
            format!(
                "{} is installed. It is the model kept loaded: the model server starts again with what the install brought, and a call that runs on it now is cut.",
                model.id
            ),
            SystemMessageType::Info,
        ),
        (JobKind::Install, JobEnd::Done) => (format!("{} is installed. /native use {} loads it.", model.id, model.id), SystemMessageType::Info),
        // The updater's own last line said it.
        (JobKind::Remove, JobEnd::Done) | (_, JobEnd::Nothing(_)) => return,
        (_, JobEnd::Refused(why) | JobEnd::Failed(why)) => {
            (format!("The {} of {} did not finish: {why}.", kind.noun(), model.id), SystemMessageType::Error)
        }
        (JobKind::Install, JobEnd::Stopped) => (
            format!("The download of {} was stopped. What arrived is kept, and the next install goes on from it.", model.id),
            SystemMessageType::Info,
        ),
        (JobKind::Remove, JobEnd::Stopped) => (format!("The removal of {} was stopped.", model.id), SystemMessageType::Info),
    };
    say(tx, said, how).await;
}

/// The forms of `/native` that need no stored configuration.
async fn machine_action(
    action: MachineAction,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    env: &Env,
    jobs: &Jobs,
    cfg: Option<&config::SystemConfig>,
) {
    match action {
        MachineAction::Show => say(tx, show_native(env, jobs, cfg).await, SystemMessageType::Info).await,
        MachineAction::Help => say(tx, native_usage(), SystemMessageType::Info).await,
        MachineAction::Import => say(tx, import_instructions(), SystemMessageType::Info).await,
        MachineAction::Install(model) => start_job(JobKind::Install, model, tx, env, jobs).await,
        MachineAction::Remove(model) => start_job(JobKind::Remove, model, tx, env, jobs).await,
        MachineAction::Cancel => {
            let said = match jobs.stop() {
                Some(running) => format!("Stopping the {} of {}.", running.kind.noun(), running.model.id),
                None if contract::job_running(&env.models) => {
                    "The model job that runs was not started by this brain (an upload through the web console): it ends with its upload.".to_string()
                }
                None => "No model job runs.".to_string(),
            };
            say(tx, said, SystemMessageType::Info).await;
        }
    }
}

/// `/native off`: no model is kept loaded any more. Refused while a native
/// model leads the session, which would be left without its model.
async fn native_off(tx: &mpsc::Sender<Result<ConversationResponse, Status>>, db: &Arc<WardsonDbClient>, env: &Env) {
    let mut cfg = match config::load_config(db).await {
        Ok(cfg) => cfg,
        Err(e) => return say(tx, format!("/native off: the configuration could not be read ({e}). Nothing was changed."), SystemMessageType::Error).await,
    };
    if ProviderKind::from_str(&cfg.api_provider) == Some(ProviderKind::Native) {
        return say(
            tx,
            "Not freed: a native model leads the session, and /native off would leave it without its model. Switch to another provider first (/provider <name>, with no session open), then /native off.".to_string(),
            SystemMessageType::Error,
        )
        .await;
    }
    let Some(model) = non_empty(&cfg.openai_compat.native_model) else {
        // A record that asks for one all the same is put right.
        let _ = native_model::follow(env, &cfg.openai_compat);
        return say(tx, "No native model is kept loaded.".to_string(), SystemMessageType::Info).await;
    };
    cfg.openai_compat.native_model.clear();
    if let Err(e) = config::save_config(db, &cfg).await {
        return say(tx, format!("/native off: the configuration could not be saved ({e}). Nothing was changed."), SystemMessageType::Error).await;
    }
    match native_model::follow(env, &cfg.openai_compat) {
        Ok(_) => {
            say(
                tx,
                format!(
                    "{model} is no longer kept loaded (experimental): embrad ends the model server within seconds, and its memory is free. /native use {model} loads it again."
                ),
                SystemMessageType::Info,
            )
            .await
        }
        Err(why) => say(tx, format!("/native off: {why}. The brain writes the record again at its next start."), SystemMessageType::Error).await,
    }
}

/// `/native context`: the window the model kept loaded is given. A change
/// is a new record: embrad ends the model server and starts it again.
async fn native_context(action: ContextAction, tx: &mpsc::Sender<Result<ConversationResponse, Status>>, db: &Arc<WardsonDbClient>, env: &Env) {
    let mut cfg = match config::load_config(db).await {
        Ok(cfg) => cfg,
        Err(e) => return say(tx, format!("/native context: the configuration could not be read ({e}). Nothing was changed."), SystemMessageType::Error).await,
    };
    let Some(model) = cfg.openai_compat.chosen_native_model() else {
        return say(tx, config::PresetNotReady::NoModel(OpenAiCompatPreset::Native).to_string(), SystemMessageType::Error).await;
    };
    let current = cfg.openai_compat.native_context(model);
    let stored = cfg.openai_compat.native_context_by_model.get(model.id).copied();
    // The window asked for, and the figure the configuration keeps for it:
    // none is the model's default.
    let (wanted, keep) = match action {
        ContextAction::Show => {
            let whose = if stored.is_some() { "set with /native context" } else { "the default" };
            return say(
                tx,
                format!(
                    "{} is kept loaded with a context window of {current} tokens ({whose}). {} takes {} to {} tokens. /native context <tokens> loads it again with another window; a larger one takes more memory. /native context reset goes back to {}.",
                    model.id,
                    model.label,
                    contract::SMALLEST_CONTEXT,
                    model.largest_context,
                    model.default_context
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        ContextAction::Set(tokens) => match model.context_ok(tokens) {
            Ok(tokens) => (tokens, Some(tokens)),
            Err(why) => return say(tx, format!("{why}. Nothing was changed."), SystemMessageType::Error).await,
        },
        ContextAction::Reset => (model.default_context, None),
    };
    if keep == stored {
        return say(tx, format!("{} is kept loaded with a context window of {current} tokens already.", model.id), SystemMessageType::Info).await;
    }
    // Read before the record changes: the model server that runs now still
    // holds what the next one gets.
    let machine = native_model::machine_memory(env);
    match keep {
        Some(tokens) => cfg.openai_compat.native_context_by_model.insert(model.id.to_string(), tokens),
        None => cfg.openai_compat.native_context_by_model.remove(model.id),
    };
    if let Err(e) = config::save_config(db, &cfg).await {
        return say(tx, format!("/native context: the configuration could not be saved ({e}). Nothing was changed."), SystemMessageType::Error).await;
    }
    if let Err(why) = native_model::follow(env, &cfg.openai_compat) {
        return say(tx, format!("/native context: {why}. The brain writes the record again at its next start."), SystemMessageType::Error).await;
    }
    crate::provider::health::request_probe();
    let said = if wanted == current {
        // The same window as before: only whose figure it is has changed.
        format!("{} keeps its context window of {current} tokens.", model.id)
    } else {
        format!(
            "{} gets a context window of {wanted} tokens (experimental): embrad ends the model server and loads the model again within seconds. A turn in flight on it is cut, and /continue sends its last call again.",
            model.id
        )
    };
    say(tx, said, SystemMessageType::Info).await;
    if wanted != current {
        say_of_memory(model, wanted, machine, tx, env).await;
    }
}

/// `/native` in a session.
pub(super) async fn handle_native_command(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>, db: &Arc<WardsonDbClient>, hands: &NativeHands) {
    match parse_native(args) {
        NativeAction::Refused(why) => say(tx, why, SystemMessageType::Error).await,
        NativeAction::Stored(StoredAction::Use(model)) => pick_native_model(model, tx, db, hands).await,
        NativeAction::Stored(StoredAction::Off) => native_off(tx, db, &hands.env).await,
        NativeAction::Stored(StoredAction::Context(action)) => native_context(action, tx, db, &hands.env).await,
        NativeAction::Machine(action) => {
            let cfg = config::load_config(db).await.ok();
            machine_action(action, tx, &hands.env, &hands.jobs, cfg.as_ref()).await
        }
    }
}

/// `/native` before a session (the wizard, Learning Mode, the identity
/// import's dialogue): the models' files and the jobs are the machine's
/// matters and are answered; what the stored configuration keeps loaded is
/// the setup's to choose.
pub(super) async fn handle_native_before_a_session(args: &str, tx: &mpsc::Sender<Result<ConversationResponse, Status>>, env: &Env, jobs: &Jobs) {
    match parse_native(args) {
        NativeAction::Refused(why) => say(tx, why, SystemMessageType::Error).await,
        NativeAction::Stored(_) => say(tx, NATIVE_NOT_BEFORE_A_SESSION.to_string(), SystemMessageType::Info).await,
        NativeAction::Machine(action) => machine_action(action, tx, env, jobs, None).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_model::test_support::{install, machine, stand_in};
    use embra_boot::native_model::{Wanted, by_id};
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
        let oc = config_of("anthropic", serde_json::json!({})).openai_compat;
        let (prompt, state) = native_model_selection(Some("qwen3.5-9b"), &oc, hands(&env));
        assert_eq!(prompt.field_type, SetupFieldType::Selector as i32);
        assert_eq!(prompt.options, ["qwen3.5-9b", "qwen3.5-4b", "qwen3.8-27b"], "the model kept loaded first");
        assert_eq!(prompt.default_value, "qwen3.5-9b");
        // Each model with what it takes when it is loaded: the 4B as it
        // stands here, without its image file; the 9B as an install brings it.
        assert_eq!(
            prompt.prompt,
            "Select a native model (experimental). qwen3.5-4b is Qwen3.5 4B (Q4_K_M, 2685 MB): installed, about 4.7 GB of memory when loaded; qwen3.5-9b is Qwen3.5 9B (Q4_K_M, 5561 MB): not installed, downloaded first, about 7.8 GB of memory when loaded; qwen3.8-27b is Qwen3.8 27B (Q4_K_M, 16217 MB): not installed, downloaded first, about 20.0 GB of memory when loaded."
        );
        // The machine's memory beside them, when the kernel says it; and a
        // window stored for a model is the one its figure is for.
        native_model::test_support::memory(&env, 12_900, 11_800);
        let narrowed = config_of("anthropic", serde_json::json!({"native_context_by_model": {"qwen3.5-4b": 32768}})).openai_compat;
        let (with_machine, _) = native_model_selection(None, &narrowed, hands(&env));
        assert_eq!(
            with_machine.prompt,
            "Select a native model (experimental). qwen3.5-4b is Qwen3.5 4B (Q4_K_M, 2685 MB): installed, about 3.7 GB of memory when loaded; qwen3.5-9b is Qwen3.5 9B (Q4_K_M, 5561 MB): not installed, downloaded first, about 7.8 GB of memory when loaded; qwen3.8-27b is Qwen3.8 27B (Q4_K_M, 16217 MB): not installed, downloaded first, about 20.0 GB of memory when loaded. This machine has 12.6 GB of memory, 11.5 GB of it free for a model now."
        );
        std::fs::remove_dir_all(&env.proc).unwrap();
        assert!(matches!(state.preset, OpenAiCompatPreset::Native) && state.model_only && state.native.is_some());
        assert!(matches!(&state.step, OpenAiCompatSetupStep::AwaitingModelSelection { models, .. } if models == &prompt.options));

        // None kept loaded: the catalogue's own order.
        let (prompt, _) = native_model_selection(None, &oc, hands(&env));
        assert_eq!(prompt.options, ["qwen3.5-4b", "qwen3.5-9b", "qwen3.8-27b"]);
        // Every model of the catalogue is offered, and each is a name `/model` takes.
        for model in &contract::CATALOGUE {
            assert!(prompt.options.iter().any(|id| id == model.id), "{}", model.id);
            assert_eq!(native_model_choice(model.id).map(|m| m.id), Ok(model.id));
            assert_eq!(native_model_choice(&model.id.to_uppercase()).map(|m| m.id), Ok(model.id));
        }
        assert_eq!(
            native_model_choice("qwen3.5-27b").unwrap_err(),
            "'qwen3.5-27b' is not a native model. The catalogue (experimental): qwen3.5-4b, qwen3.5-9b, qwen3.8-27b. /model alone picks from the list."
        );

        // A download that stopped, and a file nobody checked.
        std::fs::create_dir_all(nine().dir(&env.models)).unwrap();
        std::fs::File::create(nine().partial_path(&env.models)).unwrap().set_len(1024 * 1024 * 1024).unwrap();
        assert_eq!(native_model::file_words(nine(), &env), "1024 of 5561 MB downloaded, the rest is downloaded first");
        std::fs::remove_file(four().verified_path(&env.models)).unwrap();
        assert!(native_model::file_words(four(), &env).starts_with("its file is checked first ("), "{}", native_model::file_words(four(), &env));
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

    /// On a machine that is short of memory for a model, a load and a
    /// change of its window say so after their own sentence, as a warning,
    /// and are done all the same; with the room nothing is said.
    #[tokio::test]
    async fn a_load_and_a_window_say_when_the_machine_is_short_of_memory_and_are_done_all_the_same() {
        let (_root, env) = machine("pick-memory");
        install(four(), &env);
        let (tx, mut rx) = mpsc::channel(16);
        let heard = |rx: &mut mpsc::Receiver<Result<ConversationResponse, Status>>| {
            let frame = rx.try_recv().expect("something was said").unwrap();
            match frame.response_type {
                Some(conversation_response::ResponseType::System(m)) => (m.msg_type, m.content),
                other => panic!("not a message: {other:?}"),
            }
        };
        // 8 GB: the 4B fits at its load (4.7 GB of 7.1 free) and may outgrow it.
        native_model::test_support::memory(&env, 7_700, 7_300);
        let db_server = config_db("anthropic", "").await;
        let db = Arc::new(WardsonDbClient::from_url(&db_server.uri()));
        pick_native_model(four(), &tx, &db, &hands(&env)).await;
        assert_eq!(heard(&mut rx).0, SystemMessageType::Info as i32);
        let (kind, said) = heard(&mut rx);
        assert_eq!(kind, SystemMessageType::Warning as i32);
        assert_eq!(
            said,
            "Memory: Qwen3.5 4B with a context window of 65536 tokens takes about 4.7 GB when it is loaded, as measured, and its model server can grow by up to 5.3 GB as a session goes on: it keeps what it read of earlier prompts. For both this machine has 7.1 GB free for it (7.5 GB in all). When memory runs short the answers come slowly, or the kernel ends the model server and embrad starts it again. /native off and /native use free what it kept. A smaller window lowers both figures (/native context <tokens>)."
        );
        assert_eq!(saved_models(&db_server).await, ["qwen3.5-4b"], "it is asked for all the same");
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(four()))));
        assert!(rx.try_recv().is_err());

        // 4 GB: what it takes at its load is itself more than is free.
        native_model::test_support::memory(&env, 3_900, 3_400);
        let cfg = config_of("anthropic", serde_json::json!({"native_model": "qwen3.5-4b"}));
        let server = db_with(&cfg).await;
        native_context(ContextAction::Set(32_768), &tx, &Arc::new(WardsonDbClient::from_url(&server.uri())), &env).await;
        assert!(heard(&mut rx).1.starts_with("qwen3.5-4b gets a context window of 32768 tokens (experimental): "));
        let (kind, said) = heard(&mut rx);
        assert_eq!(kind, SystemMessageType::Warning as i32);
        assert_eq!(
            said,
            "Memory: Qwen3.5 4B with a context window of 32768 tokens takes about 3.7 GB when it is loaded, as measured, and this machine has 3.3 GB free for it (3.8 GB in all). Without the room its answers come very slowly, or the kernel ends the model server. A smaller window takes less, about 32 MB for every 1,024 tokens (/native context <tokens>)."
        );
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted { context: 32_768, ..Wanted::new(four()) })), "the window is set all the same");

        // A machine with the room, and one whose memory the kernel does not say: the sentence alone.
        for roomy in [true, false] {
            match roomy {
                true => native_model::test_support::memory(&env, 64_000, 60_000),
                false => std::fs::remove_dir_all(&env.proc).unwrap(),
            }
            let db_server = config_db("anthropic", "").await;
            pick_native_model(four(), &tx, &Arc::new(WardsonDbClient::from_url(&db_server.uri())), &hands(&env)).await;
            assert_eq!(heard(&mut rx).0, SystemMessageType::Info as i32);
            assert!(rx.try_recv().is_err(), "nothing of memory: roomy {roomy}");
        }
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
                "echo \"$2: downloading its file\"\necho \"$2: received 270 of 5561 MB (5%)\"\nuntil [ -e '{}' ]; do sleep 0.02; done\necho \"$2: installed and verified\"",
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
            "qwen3.5-9b is not ready on this machine: its files (5561 MB, and 875 MB to read images) are installed and checked first, and the lines below follow the updater. The session stays usable; the model is loaded when the job is done."
        );
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: downloading its file");
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");
        assert!(saved_models(&db_server).await.is_empty(), "the model in use stays until the file is there");
        assert_eq!(contract::read_settings(&env.settings), Ok(None));

        // A second pick while it runs: one job at a time, and how far that one is.
        let (other, mut other_rx) = mpsc::channel(4);
        pick_native_model(nine(), &other, &db, &hands).await;
        assert_eq!(
            told(&mut other_rx).await,
            "qwen3.5-9b was not installed: the download of qwen3.5-9b runs (qwen3.5-9b: received 270 of 5561 MB (5%)), and one job runs at a time. Pick again when it is done."
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
        env.updater = stand_in(&root, &format!("echo $$ > '{}'\necho \"$2: received 270 of 5561 MB (5%)\"\nwhile :; do sleep 0.02; done", pid_file.display()));
        let stopping = NativeHands { env: env.clone(), ..hands.clone() };
        pick_native_model(nine(), &tx, &db, &stopping).await;
        assert!(told(&mut rx).await.starts_with("qwen3.5-9b is not ready on this machine:"));
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");
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
        let thinking = |oc: &config::OpenAiCompatConfig| crate::provider::openai_compat::native::thinks(contract::Family::Qwen35, oc.effort_for_preset(OpenAiCompatPreset::Native));
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

    // ── /native ──────────────────────────────────────────────────────────

    /// A model whose family has levels: `/effort` shows the level, takes
    /// each of the family's and no other, and `off` is its own word, since
    /// `low` is a level here. Nothing but the family's own words is stored.
    #[test]
    fn effort_on_a_native_model_with_levels_takes_its_own_levels_and_no_other() {
        let mut oc = config::OpenAiCompatConfig::default();
        oc.set_model(OpenAiCompatPreset::Native, "qwen3.8-27b");
        let stored = |oc: &config::OpenAiCompatConfig| oc.effort_for_preset(OpenAiCompatPreset::Native).map(str::to_string);
        let run = |args: &str, oc: &mut config::OpenAiCompatConfig| native_effort(contract::Family::Qwen38, "qwen3.8-27b", args, oc);

        let (changed, said, kind) = run("", &mut oc);
        assert_eq!((changed, kind), (false, SystemMessageType::Info));
        assert_eq!(
            said,
            "Thinking is on at the model's own level, xhigh (the default) for qwen3.8-27b. This native model (experimental) thinks at one of its levels (low, medium, xhigh): /effort <level> sets one, /effort on is its own (xhigh), and /effort off answers without thinking first. A change takes effect on your next message."
        );
        for level in ["low", "medium", "xhigh"] {
            let (changed, said, kind) = run(level, &mut oc);
            assert_eq!((changed, kind), (true, SystemMessageType::Info));
            assert_eq!(said, format!("Thinking is on for qwen3.8-27b at {level}. Takes effect on your next message."));
            assert_eq!(stored(&oc).as_deref(), Some(level));
        }
        assert!(run("", &mut oc).1.starts_with("Thinking is on at xhigh for qwen3.8-27b. "));
        // A level of other providers is none of this model's: refused, and what was stored stays.
        for no_level in ["high", "max", "HIGH"] {
            let (changed, said, kind) = run(no_level, &mut oc);
            assert_eq!((changed, kind), (false, SystemMessageType::Error), "{no_level}");
            assert_eq!(said, format!("'{}' is no level of qwen3.8-27b: it takes low, medium, xhigh. Nothing was changed.", no_level.to_ascii_lowercase()));
            assert_eq!(stored(&oc).as_deref(), Some("xhigh"));
        }
        let (changed, said, _) = run("off", &mut oc);
        assert!(changed && said.starts_with("Thinking is off for qwen3.8-27b: it answers without thinking first."), "{said}");
        assert_eq!(stored(&oc).as_deref(), Some("off"));
        assert!(run("", &mut oc).1.starts_with("Thinking is off for qwen3.8-27b. "));
        for back in ["on", "reset"] {
            let (changed, said, _) = run(back, &mut oc);
            assert!(changed);
            assert_eq!(said, "Thinking is on for qwen3.8-27b at its own level, xhigh (the default). Takes effect on your next message.");
            assert_eq!(stored(&oc), None);
        }
        let (changed, said, kind) = run("sometimes", &mut oc);
        assert_eq!((changed, kind), (false, SystemMessageType::Error));
        assert!(said.starts_with("'sometimes' is not on, off or a level. "), "{said}");
        // The command finds the family by the model the configuration names.
        let (changed, said, _) = apply_native_effort("medium", &mut oc);
        assert!(changed && said == "Thinking is on for qwen3.8-27b at medium. Takes effect on your next message.", "{said}");
        oc.set_model(OpenAiCompatPreset::Native, "qwen3.5-4b");
        let (_, said, _) = apply_native_effort("medium", &mut oc);
        assert!(said.starts_with("Thinking is on for qwen3.5-4b. A native model has no levels (experimental)"), "{said}");
    }

    #[test]
    fn native_is_read_into_its_forms() {
        use {MachineAction as M, StoredAction as S};
        let machine = NativeAction::Machine;
        let stored = NativeAction::Stored;
        for show in ["", "   ", "status", "list", "LIST"] {
            assert_eq!(parse_native(show), machine(M::Show), "{show:?}");
        }
        assert_eq!(parse_native("help"), machine(M::Help));
        assert_eq!(parse_native("import"), machine(M::Import));
        assert_eq!(parse_native("import qwen3.5-4b"), NativeAction::Refused("/native import takes nothing after it. /native help shows the forms.".into()));
        assert_eq!(parse_native("install qwen3.5-4b"), machine(M::Install(four())));
        assert_eq!(parse_native("remove QWEN3.5-9B"), machine(M::Remove(nine())));
        assert_eq!(parse_native("cancel"), machine(M::Cancel));
        assert_eq!(parse_native("use qwen3.5-9b"), stored(S::Use(nine())));
        assert_eq!(parse_native("off"), stored(S::Off));
        assert_eq!(parse_native("context"), stored(S::Context(ContextAction::Show)));
        assert_eq!(parse_native("context 32768"), stored(S::Context(ContextAction::Set(32_768))));
        assert_eq!(parse_native("context 65,536"), stored(S::Context(ContextAction::Set(65_536))));
        assert_eq!(parse_native("Context Reset"), stored(S::Context(ContextAction::Reset)));
        for (bad, why) in [
            ("install", "/native install takes one model: qwen3.5-4b, qwen3.5-9b, qwen3.8-27b."),
            ("use qwen3.5-4b qwen3.5-9b", "/native use takes one model: qwen3.5-4b, qwen3.5-9b, qwen3.8-27b."),
            ("remove qwen3.5-27b", "'qwen3.5-27b' is not a native model; the catalogue: qwen3.5-4b, qwen3.5-9b, qwen3.8-27b."),
            ("off now", "/native off takes nothing after it."),
            ("list all", "/native list takes nothing after it."),
            ("context many", "'many' is not a number of tokens."),
            ("context 1 2", "/native context takes one number of tokens, or reset."),
            ("context -4", "'-4' is not a number of tokens."),
            ("frobnicate", "'frobnicate' is not a form of /native."),
        ] {
            assert_eq!(parse_native(bad), NativeAction::Refused(format!("{why} /native help shows the forms.")), "{bad}");
        }
        // Every model of the catalogue is a model each form takes, and the usage names it.
        for model in &contract::CATALOGUE {
            for verb in ["install", "use", "remove"] {
                assert!(!matches!(parse_native(&format!("{verb} {}", model.id)), NativeAction::Refused(_)), "{verb} {}", model.id);
            }
            assert!(native_usage().contains(model.id), "{}", model.id);
        }
        assert!(native_usage().starts_with("Usage: /native | /native install <model> | /native import | /native use <model> | /native off | /native context [<tokens>|reset] | /native remove <model> | /native cancel\n"));
    }

    /// `/native import` says how and where from, and prints no address of
    /// the console and no token: the brain holds neither.
    #[test]
    fn the_import_form_says_how_and_prints_no_address_or_token_of_its_own() {
        let said = import_instructions();
        assert!(said.starts_with("A model's files come in through the web console, from a browser on another machine (experimental): in the sidebar, /native with the action import, then a file."), "{said}");
        for model in &contract::CATALOGUE {
            // Both files of each model, each with its size and where another machine fetches it.
            let both = format!(
                "for {}, {} ({} MB, from {}) and, to read images, {} ({} MB, from {})",
                model.id,
                model.file,
                mb(model.bytes),
                model.url(),
                model.images.file,
                mb(model.images.bytes),
                model.url_of(contract::Part::Images)
            );
            assert!(said.contains(&both), "{}: {said}", model.id);
        }
        assert!(said.contains("keeps nothing of a file that is not the catalogue's") && said.ends_with("/native use <model> loads it then."), "{said}");
        for never in ["3345", "token", "/embraOS", "localhost", "127.0.0.1"] {
            assert!(!said.contains(never), "{never}: {said}");
        }
    }

    /// A stand-in for the updater that answers the three commands `/native`
    /// runs: the list, an install that waits for `gate`, and a removal the
    /// updater refuses.
    fn updater(root: &std::path::Path, gate: &std::path::Path) -> Vec<OsString> {
        stand_in(
            root,
            &format!(
                "case \"$1\" in\n\
                 model-list)\n  echo 'qwen3.5-4b  Qwen3.5 4B (Q4_K_M, 2685 MB, Apache-2.0): installed'\n  echo 'qwen3.5-9b  Qwen3.5 9B (Q4_K_M, 5561 MB, Apache-2.0): not installed'\n  echo 'DATA has 1840 MB free' ;;\n\
                 model-install)\n  echo \"$2: received 270 of 5561 MB (5%)\"\n  until [ -e '{}' ]; do sleep 0.02; done\n  echo \"$2: installed and verified\" ;;\n\
                 model-remove)\n  echo \"refused: the settings ask for $2 to be kept loaded; free it first (/native off)\" >&2\n  exit 2 ;;\n\
                 esac",
                gate.display()
            ),
        )
    }

    fn config_of(provider: &str, openai_compat: serde_json::Value) -> config::SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase2",
            "created_at": "", "version": "test", "api_provider": provider, "openai_compat": openai_compat,
        }))
        .expect("a configuration")
    }

    #[tokio::test]
    async fn native_shows_the_files_what_is_kept_loaded_the_server_and_the_job() {
        use crate::native_model::test_support::status;
        use embra_boot::native_model::ServiceState;
        let (root, mut env) = machine("native-show");
        let gate = root.join("go-on");
        env.updater = updater(&root, &gate);
        let jobs = Jobs::default();

        // Before a session, with nothing asked for and no embrad on the machine.
        assert_eq!(
            show_native(&env, &jobs, None).await,
            "Native models (experimental): a model that runs on this machine.\n  \
             qwen3.5-4b  Qwen3.5 4B (Q4_K_M, 2685 MB, Apache-2.0): installed\n  \
             qwen3.5-9b  Qwen3.5 9B (Q4_K_M, 5561 MB, Apache-2.0): not installed\n  \
             DATA has 1840 MB free\n  \
             Kept loaded: none. /native use <model> loads one.\n  \
             /native help shows the forms."
        );

        // A model kept loaded that leads the session, thinking off, its server ready.
        let wanted = Wanted::new(four());
        contract::write_settings(&env.settings, Some(&wanted)).unwrap();
        contract::write_status(&env.status, &status(ServiceState::Ready, Some(wanted), "")).unwrap();
        let cfg = config_of("native", serde_json::json!({"native_model": "qwen3.5-4b", "native_effort_by_model": {"qwen3.5-4b": "low"}}));
        let shown = show_native(&env, &jobs, Some(&cfg)).await;
        assert!(
            shown.contains("\n  Kept loaded: qwen3.5-4b, context window 65536, thinking off. It leads the session.\n  The model server: ready, context window 65536, reads text alone (its image file is not installed).\n  /native help"),
            "{shown}"
        );
        // Once embrad's status names the backend the processor is given, the same line says it.
        let mut named = status(ServiceState::Ready, Some(wanted), "");
        (named.images, named.backend) = (true, Some("haswell".to_string()));
        contract::write_status(&env.status, &named).unwrap();
        let shown = show_native(&env, &jobs, Some(&cfg)).await;
        assert!(shown.contains("\n  The model server: ready, context window 65536, reads images, processor backend haswell.\n  /native help"), "{shown}");
        contract::write_status(&env.status, &status(ServiceState::Ready, Some(wanted), "")).unwrap();
        // Another provider leads; the model thinks; before a session the thinking is not said.
        let cfg = config_of("anthropic", serde_json::json!({"native_model": "qwen3.5-4b"}));
        assert!(show_native(&env, &jobs, Some(&cfg)).await.contains("\n  Kept loaded: qwen3.5-4b, context window 65536, thinking on.\n"));
        assert!(show_native(&env, &jobs, None).await.contains("\n  Kept loaded: qwen3.5-4b, context window 65536.\n"));

        // A job that runs, with how far it is; then how it ended.
        let (tx, mut rx) = mpsc::channel(16);
        machine_action(MachineAction::Install(nine()), &tx, &env, &jobs, None).await;
        assert_eq!(
            told(&mut rx).await,
            "Installing qwen3.5-9b: its model file (5561 MB) and its image file (875 MB) are downloaded and checked, a file that is there already is left alone, and the lines below follow the updater. The session stays usable; /native cancel stops it."
        );
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");
        assert!(show_native(&env, &jobs, None)
            .await
            .contains("\n  The download of qwen3.5-9b runs: qwen3.5-9b: received 270 of 5561 MB (5%). /native cancel stops it.\n"));
        // One job at a time.
        machine_action(MachineAction::Remove(four()), &tx, &env, &jobs, None).await;
        assert_eq!(
            told(&mut rx).await,
            "Not started: the download of qwen3.5-9b runs (qwen3.5-9b: received 270 of 5561 MB (5%)), and one job runs at a time. /native cancel stops it."
        );
        std::fs::write(&gate, "").unwrap();
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: installed and verified");
        assert_eq!(told(&mut rx).await, "qwen3.5-9b is installed. /native use qwen3.5-9b loads it.");
        assert!(show_native(&env, &jobs, None).await.contains("\n  The last job, the download of qwen3.5-9b: done.\n"));

        // A removal the updater refuses is said in the updater's words.
        machine_action(MachineAction::Remove(four()), &tx, &env, &jobs, None).await;
        assert_eq!(told(&mut rx).await, "Removing the files of qwen3.5-4b.");
        assert_eq!(
            told(&mut rx).await,
            "The removal of qwen3.5-4b did not finish: the settings ask for qwen3.5-4b to be kept loaded; free it first (/native off)."
        );

        // No updater on this machine: the rest is shown all the same.
        env.updater = vec![OsString::from(root.join("no-such-updater"))];
        let shown = show_native(&env, &jobs, None).await;
        assert!(shown.contains("\n  The models' files could not be listed: the updater could not be run ("), "{shown}");
        assert!(shown.contains("\n  Kept loaded: qwen3.5-4b, context window 65536.\n"), "{shown}");

        machine_action(MachineAction::Help, &tx, &env, &jobs, None).await;
        assert_eq!(told(&mut rx).await, native_usage());
    }

    #[tokio::test]
    async fn native_cancel_stops_the_job_that_runs() {
        let (root, mut env) = machine("native-cancel");
        env.updater = updater(&root, &root.join("never"));
        let jobs = Jobs::default();
        let (tx, mut rx) = mpsc::channel(16);
        machine_action(MachineAction::Cancel, &tx, &env, &jobs, None).await;
        assert_eq!(told(&mut rx).await, "No model job runs.");

        machine_action(MachineAction::Install(nine()), &tx, &env, &jobs, None).await;
        assert!(told(&mut rx).await.starts_with("Installing qwen3.5-9b: "));
        assert_eq!(told(&mut rx).await, "qwen3.5-9b: received 270 of 5561 MB (5%)");
        machine_action(MachineAction::Cancel, &tx, &env, &jobs, None).await;
        assert_eq!(told(&mut rx).await, "Stopping the download of qwen3.5-9b.");
        assert_eq!(told(&mut rx).await, "The download of qwen3.5-9b was stopped. What arrived is kept, and the next install goes on from it.");
        assert!(jobs.running().is_none());

        // A job this brain did not start holds the models' lock: an upload.
        let lock = contract::job_lock(&env.models).unwrap().expect("no job holds it");
        machine_action(MachineAction::Cancel, &tx, &env, &jobs, None).await;
        assert_eq!(
            told(&mut rx).await,
            "The model job that runs was not started by this brain (an upload through the web console): it ends with its upload."
        );
        assert!(show_native(&env, &jobs, None)
            .await
            .contains("\n  A model job runs that this brain did not start: an upload through the web console.\n"));
        drop(lock);
    }

    /// A database that holds `cfg` and takes its saves.
    async fn db_with(cfg: &config::SystemConfig) -> MockServer {
        let server = MockServer::start().await;
        let envelope = |data: serde_json::Value| ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "meta": {}, "data": data}));
        Mock::given(method("GET")).and(path("/config.system/docs/config")).respond_with(envelope(serde_json::to_value(cfg).unwrap())).mount(&server).await;
        Mock::given(method("GET")).and(path("/config.system")).respond_with(envelope(serde_json::json!({}))).mount(&server).await;
        Mock::given(method("POST")).and(path("/config.system/docs")).respond_with(envelope(serde_json::json!({"_id": "config"}))).mount(&server).await;
        server
    }

    /// The `openai_compat` block of each configuration saved, in order.
    async fn saved(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/config.system/docs")
            .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).expect("a configuration")["openai_compat"].clone())
            .collect()
    }

    #[tokio::test]
    async fn native_off_frees_the_model_and_is_refused_while_one_leads_the_session() {
        let (_root, env) = machine("native-off");
        let wanted = Wanted::new(four());
        contract::write_settings(&env.settings, Some(&wanted)).unwrap();
        let (tx, mut rx) = mpsc::channel(16);

        // A native model leads the session: refused, and nothing is touched.
        let server = db_with(&config_of("native", serde_json::json!({"native_model": "qwen3.5-4b"}))).await;
        native_off(&tx, &Arc::new(WardsonDbClient::from_url(&server.uri())), &env).await;
        assert_eq!(
            told(&mut rx).await,
            "Not freed: a native model leads the session, and /native off would leave it without its model. Switch to another provider first (/provider <name>, with no session open), then /native off."
        );
        assert!(saved(&server).await.is_empty());
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(wanted)));

        // Another provider leads: the configuration keeps none, and the record names none.
        let server = db_with(&config_of("anthropic", serde_json::json!({"native_model": "qwen3.5-4b", "native_context_by_model": {"qwen3.5-4b": 32768}}))).await;
        native_off(&tx, &Arc::new(WardsonDbClient::from_url(&server.uri())), &env).await;
        assert_eq!(
            told(&mut rx).await,
            "qwen3.5-4b is no longer kept loaded (experimental): embrad ends the model server within seconds, and its memory is free. /native use qwen3.5-4b loads it again."
        );
        let saved = saved(&server).await;
        assert_eq!(saved.len(), 1);
        assert!(saved[0].get("native_model").is_none(), "none is kept loaded: {}", saved[0]);
        assert_eq!(saved[0]["native_context_by_model"]["qwen3.5-4b"], 32768, "the model's window stays its own");
        assert_eq!(contract::read_settings(&env.settings), Ok(None));

        // None is kept loaded.
        let server = db_with(&config_of("anthropic", serde_json::json!({}))).await;
        native_off(&tx, &Arc::new(WardsonDbClient::from_url(&server.uri())), &env).await;
        assert_eq!(told(&mut rx).await, "No native model is kept loaded.");
    }

    #[tokio::test]
    async fn native_context_shows_sets_and_resets_the_loaded_models_window() {
        let (_root, env) = machine("native-context");
        contract::write_settings(&env.settings, Some(&Wanted::new(four()))).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let run = |cfg: config::SystemConfig, action: ContextAction| {
            let (tx, env) = (tx.clone(), env.clone());
            async move {
                let server = db_with(&cfg).await;
                native_context(action, &tx, &Arc::new(WardsonDbClient::from_url(&server.uri())), &env).await;
                saved(&server).await
            }
        };
        let default = || config_of("native", serde_json::json!({"native_model": "qwen3.5-4b"}));
        let narrowed = || config_of("native", serde_json::json!({"native_model": "qwen3.5-4b", "native_context_by_model": {"qwen3.5-4b": 32768}}));

        assert!(run(default(), ContextAction::Show).await.is_empty());
        assert_eq!(
            told(&mut rx).await,
            "qwen3.5-4b is kept loaded with a context window of 65536 tokens (the default). Qwen3.5 4B takes 1024 to 262144 tokens. /native context <tokens> loads it again with another window; a larger one takes more memory. /native context reset goes back to 65536."
        );
        assert!(run(narrowed(), ContextAction::Show).await.is_empty());
        assert!(told(&mut rx).await.starts_with("qwen3.5-4b is kept loaded with a context window of 32768 tokens (set with /native context). "));

        // A window: stored for the model, and the record asks for it.
        let saved_set = run(default(), ContextAction::Set(32_768)).await;
        assert_eq!(
            told(&mut rx).await,
            "qwen3.5-4b gets a context window of 32768 tokens (experimental): embrad ends the model server and loads the model again within seconds. A turn in flight on it is cut, and /continue sends its last call again."
        );
        assert_eq!(saved_set.len(), 1);
        assert_eq!(saved_set[0]["native_context_by_model"]["qwen3.5-4b"], 32768);
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted { context: 32_768, ..Wanted::new(four()) })));

        // The same window again: nothing to do.
        assert!(run(narrowed(), ContextAction::Set(32_768)).await.is_empty());
        assert_eq!(told(&mut rx).await, "qwen3.5-4b is kept loaded with a context window of 32768 tokens already.");
        assert!(run(default(), ContextAction::Reset).await.is_empty());
        assert_eq!(told(&mut rx).await, "qwen3.5-4b is kept loaded with a context window of 65536 tokens already.");

        // Back to the default.
        let saved_reset = run(narrowed(), ContextAction::Reset).await;
        assert!(told(&mut rx).await.starts_with("qwen3.5-4b gets a context window of 65536 tokens (experimental): "));
        assert!(saved_reset[0].get("native_context_by_model").is_none(), "{}", saved_reset[0]);
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(Wanted::new(four()))));
        // The default's own figure, set as the model's: the window stays.
        let saved_same = run(default(), ContextAction::Set(65_536)).await;
        assert_eq!(told(&mut rx).await, "qwen3.5-4b keeps its context window of 65536 tokens.");
        assert_eq!(saved_same[0]["native_context_by_model"]["qwen3.5-4b"], 65536);

        // A window the model cannot be given, and no model kept loaded.
        assert!(run(default(), ContextAction::Set(12)).await.is_empty());
        assert_eq!(told(&mut rx).await, "12 is not a context window for Qwen3.5 4B: 1024 to 262144 tokens. Nothing was changed.");
        assert!(run(config_of("anthropic", serde_json::json!({})), ContextAction::Set(32_768)).await.is_empty());
        assert_eq!(told(&mut rx).await, "No native model is chosen. /native lists the models, and /native use <model> loads one (experimental).");
    }

    /// `/native use` on the model kept loaded whose server embrad no longer
    /// starts: the load asks for it again, and the reply says so.
    #[test]
    fn a_load_says_when_a_halted_server_is_started_again() {
        let leads = config_of("native", serde_json::json!({"native_model": "qwen3.5-4b"}));
        assert_eq!(
            loaded_words(&leads, four(), Loaded::Restarted),
            "qwen3.5-4b is asked for again: its model server had ended three times before it answered and was no longer started. embrad starts it now, and your next message waits for it."
        );
        let other = config_of("gemini", serde_json::json!({"native_model": "qwen3.5-4b"}));
        assert_eq!(
            loaded_words(&other, four(), Loaded::Restarted),
            "qwen3.5-4b is asked for again: its model server had ended three times before it answered and was no longer started. embrad starts it now. Switch to it with /provider native when ready."
        );
        assert_eq!(loaded_words(&leads, four(), Loaded::AsItWas), "qwen3.5-4b is the native model already.");
    }

    /// In a session every form is answered; before one, the forms that
    /// change the stored configuration are told to wait.
    #[tokio::test]
    async fn native_before_a_session_answers_the_machines_forms_alone() {
        let (root, mut env) = machine("native-before");
        env.updater = updater(&root, &root.join("never"));
        let jobs = Jobs::default();
        let (tx, mut rx) = mpsc::channel(16);
        for stored in ["use qwen3.5-4b", "off", "context", "context 32768", "context reset"] {
            handle_native_before_a_session(stored, &tx, &env, &jobs).await;
            assert_eq!(told(&mut rx).await, NATIVE_NOT_BEFORE_A_SESSION, "{stored}");
        }
        assert!(!env.settings.exists(), "nothing was asked of embrad");
        handle_native_before_a_session("", &tx, &env, &jobs).await;
        assert!(told(&mut rx).await.starts_with("Native models (experimental): a model that runs on this machine.\n  qwen3.5-4b  "));
        handle_native_before_a_session("frobnicate", &tx, &env, &jobs).await;
        assert_eq!(told(&mut rx).await, "'frobnicate' is not a form of /native. /native help shows the forms.");
        handle_native_before_a_session("cancel", &tx, &env, &jobs).await;
        assert_eq!(told(&mut rx).await, "No model job runs.");
        handle_native_before_a_session("import", &tx, &env, &jobs).await;
        assert_eq!(told(&mut rx).await, import_instructions());
    }
}
