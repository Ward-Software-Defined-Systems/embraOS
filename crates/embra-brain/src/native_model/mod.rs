//! Native models (experimental) in the brain: what the operator asked to be
//! kept loaded, what embrad made of it, and the jobs on the models' files.
//!
//! - **The stored configuration decides.** `openai_compat.native_model` is
//!   the model kept loaded and `native_context_by_model` its window:
//!   `/native use` sets them and `/native off` clears the model, whichever
//!   provider leads the session. The record embrad follows
//!   (`embra_boot::native_model`, on STATE) is written from the
//!   configuration and never the other way round ([`follow`]): after a
//!   change, and at every start of the brain.
//! - **embrad looks at the record every five seconds.** Until it has, its
//!   status is about what was asked before, and the server on the port may
//!   hold another model: llama.cpp's server answers a request whatever model
//!   the request names. So every request to a native model first waits for
//!   the status to be about what the record asks ([`until_followed`]), and
//!   is told in a sentence when no server will come ([`gate`]).
//! - **The jobs on a model's file** (a download, a check, a removal) are
//!   runs of the OS's updater, one at a time, kept where every console can
//!   ask for them ([`jobs`]).

pub(crate) mod jobs;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use embra_boot::native_model::{self as contract, ServiceState, Status, Wanted};

use crate::config::{OpenAiCompatConfig, PresetNotReady};
use crate::provider::ProviderError;
use crate::provider::openai_compat::{OpenAiCompatPreset, span_words};

/// How often the records are read again while embrad has not followed.
const FOLLOW_POLL: Duration = Duration::from_secs(1);
/// How long embrad is given to say that it let go of a server it no longer
/// starts: four of its looks.
const RELEASE_WAIT: Duration = Duration::from_secs(20);

/// Where the records are read and written, and how the updater is run. The
/// machine's is [`Env::in_force`]; a test names its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Env {
    /// What embrad is asked to keep loaded: the brain writes it.
    pub(crate) settings: PathBuf,
    /// What embrad says of the model server: the brain reads it.
    pub(crate) status: PathBuf,
    /// The models' files: the brain reads how a model's file stands.
    pub(crate) models: PathBuf,
    /// The updater's command line: the program, and in a test the arguments
    /// a stand-in needs before a command's own.
    pub(crate) updater: Vec<OsString>,
    /// The kernel's records of the machine and its processes: the brain
    /// reads the machine's memory there, and what the model server holds.
    pub(crate) proc: PathBuf,
}

impl Env {
    pub(crate) fn in_force() -> Self {
        Env {
            settings: PathBuf::from(contract::SETTINGS_PATH),
            status: PathBuf::from(contract::STATUS_PATH),
            models: PathBuf::from(contract::MODELS_DIR),
            updater: vec![OsString::from(crate::grpc_service::UPDATER)],
            proc: PathBuf::from("/proc"),
        }
    }

    /// What is asked for, and what embrad says. A status that cannot be
    /// read is no status: a turn is never held back by a record's glitch.
    pub(crate) fn records(&self) -> (Result<Option<Wanted>, String>, Option<Status>) {
        let status = match contract::read_status(&self.status) {
            Ok(status) => status,
            Err(why) => {
                tracing::warn!(target: "native_model", "{why}");
                None
            }
        };
        (contract::read_settings(&self.settings), status)
    }

    /// Whether the model server that runs for `model` was started with the
    /// model's image file, as embrad's status says: then an image can be
    /// sent to it. A machine without a status (a development run, no
    /// embrad) says nothing, and an image is a line of text there.
    pub(crate) fn reads_images(&self, model: &str) -> bool {
        self.records().1.is_some_and(|status| status.images && status.state.runs() && status.model.is_some_and(|m| m.id == model))
    }
}

/// A size as the updater's lines say it.
pub(crate) fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// How a model's file stands on this machine, for a question that offers
/// the model.
pub(crate) fn file_words(model: &contract::NativeModel, env: &Env) -> String {
    match contract::installed(model, &env.models) {
        contract::Installed::Verified => "installed".to_string(),
        contract::Installed::No => "not installed, downloaded first".to_string(),
        contract::Installed::Partial(bytes) => format!("{} of {} MB downloaded, the rest is downloaded first", mb(bytes), mb(model.bytes)),
        contract::Installed::Unverified(why) => format!("its file is checked first ({why})"),
    }
}

/// The machine's memory in MiB, as the kernel says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MachineMemory {
    /// All of it.
    pub(crate) total: u64,
    /// What a model server could take now: what the kernel calls available,
    /// and what the model server that runs now holds of its own, since it
    /// ends when another model or another window is asked for.
    pub(crate) free: u64,
}

/// The figure of a line `<key>:  <n> kB` of a kernel record, in MiB.
fn mib_of(record: &str, key: &str) -> Option<u64> {
    record
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':')?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
        .map(|kib| kib / 1024)
}

/// The machine's memory now. A machine whose records cannot be read has
/// none to say, and nothing is said of memory there.
pub(crate) fn machine_memory(env: &Env) -> Option<MachineMemory> {
    let meminfo = std::fs::read_to_string(env.proc.join("meminfo")).ok()?;
    let total = mib_of(&meminfo, "MemTotal")?;
    let available = mib_of(&meminfo, "MemAvailable")?;
    // Of the model server that runs, its own memory: the pages of the
    // model's file it has mapped are in the kernel's figure already.
    let held = env
        .records()
        .1
        .filter(|status| status.state.runs())
        .and_then(|status| status.pid)
        .and_then(|pid| std::fs::read_to_string(env.proc.join(pid.to_string()).join("status")).ok())
        .and_then(|status| mib_of(&status, "RssAnon"))
        .unwrap_or(0);
    Some(MachineMemory { total, free: (available + held).min(total) })
}

/// A figure in MiB as it is said: gigabytes of 1,024 MiB with one decimal,
/// as the sizes of the files are said in megabytes of 1,024 KiB.
pub(crate) fn gb(mib: u64) -> String {
    format!("{:.1}", mib as f64 / 1024.0)
}

/// Whether the model server would be started with the model's image file:
/// when the file is there and checked, and when the model is not, since an
/// install brings both.
pub(crate) fn would_read_images(model: &contract::NativeModel, env: &Env) -> bool {
    contract::installed_part(model, contract::Part::Images, &env.models) == contract::Installed::Verified
        || contract::installed(model, &env.models) != contract::Installed::Verified
}

/// What is said of memory when a model is asked for on a machine that is
/// short of it, by the measured figures (`NativeModel::loaded_mib`,
/// `growth_mib`) against what the kernel says is free. Nothing is refused
/// on a figure: the model is loaded, and the operator knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryNote {
    /// Whether what the model takes when it is loaded is itself more than
    /// the machine has free; else it is what its server may grow to.
    pub(crate) at_the_load: bool,
    pub(crate) text: String,
}

/// The note for `model` with a window of `context` tokens, when the machine
/// is short for it. `in_session` says whether the commands that change what
/// is loaded are answered now: before a session they are not.
pub(crate) fn memory_note(model: &contract::NativeModel, context: u32, images: bool, machine: Option<MachineMemory>, in_session: bool) -> Option<MemoryNote> {
    let machine = machine?;
    let loaded = model.loaded_mib(context, images);
    let growth = model.growth_mib(context, images);
    let with_images = if images { ", its image file included" } else { "" };
    let has = format!("this machine has {} GB free for it ({} GB in all)", gb(machine.free), gb(machine.total));
    let window = if in_session { "/native context <tokens>" } else { "/native context <tokens>, once the setup is done" };
    if loaded > machine.free {
        let smaller = if context > contract::SMALLEST_CONTEXT {
            format!(" A smaller window takes less, about {} MB for every 1,024 tokens ({window}).", model.memory.per_32768 / 32)
        } else {
            String::new()
        };
        return Some(MemoryNote {
            at_the_load: true,
            text: format!(
                "Memory: {} with a context window of {context} tokens takes about {} GB when it is loaded{with_images}, as measured, and {has}. Without the room its answers come very slowly, or the kernel ends the model server.{smaller}",
                model.label,
                gb(loaded)
            ),
        });
    }
    if loaded + growth > machine.free {
        let frees = if in_session { " /native off and /native use free what it kept." } else { "" };
        return Some(MemoryNote {
            at_the_load: false,
            text: format!(
                "Memory: {} with a context window of {context} tokens takes about {} GB when it is loaded{with_images}, as measured, and its model server can grow by up to {} GB as a session goes on: it keeps what it read of earlier prompts. For both {has}. When memory runs short the answers come slowly, or the kernel ends the model server and embrad starts it again.{frees} A smaller window lowers both figures ({window}).",
                model.label,
                gb(loaded),
                gb(growth)
            ),
        });
    }
    None
}

/// The question that offers the catalogue's models, and its options: the
/// ids, with `current` (the model kept loaded) first, since a console's
/// selector starts on the first option whatever the default. `/model`,
/// `/provider --setup native` and the wizard ask it. Each model is said
/// with what it takes when it is loaded with the window `window_of` gives
/// it, and the machine with what it has: the choice is made here, and at
/// the first boot it is not taken back before Learning Mode is over.
pub(crate) fn catalogue_question(current: Option<&str>, env: &Env, window_of: &dyn Fn(&contract::NativeModel) -> u32) -> (String, Vec<String>) {
    let mut ids: Vec<String> = contract::CATALOGUE.iter().map(|m| m.id.to_string()).collect();
    if let Some(at) = current.and_then(|current| ids.iter().position(|id| id == current)) {
        let kept = ids.remove(at);
        ids.insert(0, kept);
    }
    let stands: Vec<String> = contract::CATALOGUE
        .iter()
        .map(|m| {
            format!(
                "{} is {} ({}, {} MB): {}, about {} GB of memory when loaded",
                m.id,
                m.label,
                m.quantisation,
                mb(m.bytes),
                file_words(m, env),
                gb(m.loaded_mib(window_of(m), would_read_images(m, env)))
            )
        })
        .collect();
    let machine = machine_memory(env)
        .map(|machine| format!(" This machine has {} GB of memory, {} GB of it free for a model now.", gb(machine.total), gb(machine.free)))
        .unwrap_or_default();
    (format!("Select a native model (experimental). {}.{machine}", stands.join("; ")), ids)
}

/// Ask for no model, when the record asks for one: the wizard's way back
/// from the native answer. `true` when the record was written.
pub(crate) fn ask_for_none(env: &Env) -> bool {
    match contract::read_settings(&env.settings) {
        Ok(None) => false,
        Ok(Some(_)) | Err(_) => contract::write_settings(&env.settings, None).is_ok(),
    }
}

/// What the stored configuration asks embrad for: the model kept loaded
/// with its window, or none. A stored window the model cannot be given (a
/// catalogue that changed under it) is the model's default.
pub(crate) fn wanted_by(oc: &OpenAiCompatConfig) -> Result<Option<Wanted>, String> {
    if oc.native_model.is_empty() {
        return Ok(None);
    }
    let Some(model) = oc.chosen_native_model() else {
        return Err(format!("the stored native model '{}' is not one this release knows", oc.native_model));
    };
    Ok(Some(Wanted { model, context: oc.native_context(model) }))
}

/// What [`follow`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Followed {
    /// The record said it already; nothing was written.
    AsItWas,
    /// The record was written: the model now asked for, or none.
    Written(Option<Wanted>),
}

/// Make the record say what the stored configuration asks. The provider is
/// no part of it: a model stays loaded while another provider leads.
pub(crate) fn follow(env: &Env, oc: &OpenAiCompatConfig) -> Result<Followed, String> {
    let wanted = wanted_by(oc)?;
    // A record that cannot be read says nothing, and is written anew.
    if contract::read_settings(&env.settings).is_ok_and(|stored| stored == wanted) {
        return Ok(Followed::AsItWas);
    }
    contract::write_settings(&env.settings, wanted.as_ref())
        .map_err(|e| format!("the native model's settings could not be written to {}: {e}", env.settings.display()))?;
    Ok(Followed::Written(wanted))
}

/// What a request to a native model does, by the two records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The server stands behind the model asked for, loading or ready; or
    /// the machine has no status (no embrad: a development host).
    Go,
    /// embrad has not looked at the record since it changed.
    Wait,
    /// No server will come, and why.
    No(Refusal),
}

/// Why no server will come for a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The reason, as a phrase that stands after a colon.
    pub(crate) why: String,
    /// The reason is the model server's state, of which `/native` shows
    /// more. A request's sentence names the command; the health probe's
    /// notice names it itself.
    see_native: bool,
}

impl Refusal {
    fn of(why: String) -> Gate {
        Gate::No(Refusal { why, see_native: false })
    }

    /// What a request is told.
    pub(crate) fn sentence(&self) -> String {
        if self.see_native {
            format!("{}. /native shows what the model server is doing", self.why)
        } else {
            self.why.clone()
        }
    }
}

/// Whether the model server is the one a request for `model` is meant for.
pub(crate) fn gate(wanted: &Result<Option<Wanted>, String>, status: Option<&Status>, model: &str) -> Gate {
    let Some(status) = status else { return Gate::Go };
    if status.state == ServiceState::Disabled {
        return Refusal::of(said(status));
    }
    let wanted = match wanted {
        Err(why) => return Refusal::of(format!("the native model's settings cannot be read ({why})")),
        Ok(None) => return Refusal::of(PresetNotReady::NoModel(OpenAiCompatPreset::Native).to_string()),
        Ok(Some(wanted)) => wanted,
    };
    if wanted.model.id != model {
        return Refusal::of(format!("the model server is asked for {} and the request is for {model}", wanted.model.id));
    }
    if status.model != Some(wanted.model) || status.context != Some(wanted.context) {
        return Gate::Wait;
    }
    match status.state {
        ServiceState::Loading | ServiceState::Ready => Gate::Go,
        ServiceState::Unavailable | ServiceState::Halted => {
            Gate::No(Refusal { why: format!("the native model is not loaded: {}", said(status)), see_native: true })
        }
        // Neither names a model, so neither is about what is asked.
        ServiceState::Off | ServiceState::Disabled => Gate::Wait,
    }
}

/// embrad's reason, or the state's word when it gave none.
fn said(status: &Status) -> String {
    if status.reason.is_empty() { format!("the model server is {}", status.state.as_str()) } else { status.reason.clone() }
}

/// Wait until the model server is the one a request for `model` is meant
/// for. A server that will not come ends the request at once; an embrad that
/// has not followed is waited for a second at a time, as long as the model
/// wait allows.
pub(crate) async fn until_followed(env: &Env, model: &str, wait: Duration) -> Result<(), ProviderError> {
    let started = tokio::time::Instant::now();
    loop {
        let (wanted, status) = env.records();
        match gate(&wanted, status.as_ref(), model) {
            Gate::Go => return Ok(()),
            Gate::No(refusal) => return Err(ProviderError::Connect(refusal.sentence())),
            Gate::Wait if started.elapsed() >= wait => {
                let state = status.map_or("not there", |s| s.state.as_str());
                return Err(ProviderError::Silent(format!(
                    "embrad has not started the native model after {}: its status is {state}; the request was dropped. /native shows what the model server is doing, and /model-wait sets how long embraOS waits",
                    span_words(wait)
                )));
            }
            Gate::Wait => tokio::time::sleep(FOLLOW_POLL).await,
        }
    }
}

/// A model server that ended three times before it answered is no longer
/// started until the record changes, and asking for the same model again
/// changes nothing in it. So when embrad's status is halted on what the
/// record asks: the record asks for none, embrad is waited for until it
/// says so, and the record asks again, which begins its counts anew.
/// `Ok(false)` when nothing is halted, and nothing was touched.
pub(crate) async fn ask_again_if_halted(env: &Env) -> Result<bool, String> {
    let (wanted, status) = env.records();
    let (Ok(Some(wanted)), Some(status)) = (wanted, status) else { return Ok(false) };
    if status.state != ServiceState::Halted || status.model != Some(wanted.model) || status.context != Some(wanted.context) {
        return Ok(false);
    }
    let write = |wanted: Option<&Wanted>| {
        contract::write_settings(&env.settings, wanted)
            .map_err(|e| format!("the native model's settings could not be written to {}: {e}", env.settings.display()))
    };
    write(None)?;
    let started = tokio::time::Instant::now();
    let let_go = loop {
        tokio::time::sleep(FOLLOW_POLL).await;
        match env.records().1 {
            Some(status) if status.state == ServiceState::Off => break true,
            _ if started.elapsed() >= RELEASE_WAIT => break false,
            _ => {}
        }
    };
    // Asked for again either way: the record never stays without the model
    // the configuration keeps.
    write(Some(&wanted))?;
    if let_go {
        Ok(true)
    } else {
        Err(format!("embrad did not let go of the model server within {} seconds", RELEASE_WAIT.as_secs()))
    }
}

/// The model server in a few words for a status line, by the two records;
/// `None` on a machine without embrad's status.
pub(crate) fn server_words(env: &Env) -> Option<String> {
    server_said(env, false)
}

/// The same for `/native`, which says more: with the CPU backend the
/// server's loader takes on this processor, once embrad has asked the
/// server's program and the status names it.
pub(crate) fn server_words_with_backend(env: &Env) -> Option<String> {
    server_said(env, true)
}

fn server_said(env: &Env, with_backend: bool) -> Option<String> {
    let (wanted, status) = env.records();
    let status = status?;
    let asked = wanted.as_ref().ok().and_then(|wanted| wanted.as_ref());
    // Whether the status is about what the record asks now.
    let follows = asked.is_some_and(|wanted| status.model == Some(wanted.model) && status.context == Some(wanted.context));
    // Said of a server that runs for what is asked, and of no other state.
    let backend = status.backend.as_deref().filter(|_| with_backend).map(|name| format!(", processor backend {name}")).unwrap_or_default();
    Some(match (status.state, asked) {
        (ServiceState::Disabled, _) => said(&status),
        (_, None) => "off".to_string(),
        (_, Some(wanted)) if !follows => format!("embrad is about to start it for {}", wanted.model.id),
        // Whether it reads images is the server's own matter: it was
        // started with the model's image file, or without.
        (ServiceState::Ready, Some(wanted)) if status.images => format!("ready, context window {}, reads images{backend}", wanted.context),
        (ServiceState::Ready, Some(wanted)) => {
            format!("ready, context window {}, reads text alone (its image file is not installed){backend}", wanted.context)
        }
        (ServiceState::Loading, Some(wanted)) => format!("loading {}, context window {}{backend}", wanted.model.id, wanted.context),
        (ServiceState::Unavailable | ServiceState::Halted | ServiceState::Off, Some(_)) => format!("not loaded: {}", said(&status)),
    })
}

/// What the records say of a model server that does not answer on its port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoAnswer {
    /// One is coming: embrad has not followed the record yet, or it started
    /// the server and the server does not listen yet.
    Starting,
    /// None will come: why, as a phrase that stands after a colon.
    NotLoaded(String),
    /// The records do not say: no status, or one that says the server
    /// answers.
    Unknown,
}

pub(crate) fn no_answer(env: &Env, model: &str) -> NoAnswer {
    let (wanted, status) = env.records();
    match gate(&wanted, status.as_ref(), model) {
        Gate::Wait => NoAnswer::Starting,
        Gate::No(refusal) => NoAnswer::NotLoaded(refusal.why),
        Gate::Go => match status.map(|s| s.state) {
            Some(ServiceState::Loading) => NoAnswer::Starting,
            Some(ServiceState::Ready | ServiceState::Off | ServiceState::Disabled | ServiceState::Unavailable | ServiceState::Halted) | None => {
                NoAnswer::Unknown
            }
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A machine in a directory of its own, with no record in it.
    pub(crate) fn machine(tag: &str) -> (PathBuf, Env) {
        let root = std::env::temp_dir().join(format!("embra-brain-native-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("run")).unwrap();
        let env = Env {
            settings: root.join("state").join("native-model").join("settings"),
            status: root.join("run").join("native-model-status"),
            models: root.join("data").join("models"),
            updater: vec![OsString::from("/nonexistent/embra-update")],
            // No record of the machine's memory: nothing is said of it
            // unless a test writes one (`memory`).
            proc: root.join("proc"),
        };
        (root, env)
    }

    /// The machine's memory as the kernel would say it, in MiB.
    pub(crate) fn memory(env: &Env, total_mib: u64, available_mib: u64) {
        std::fs::create_dir_all(&env.proc).unwrap();
        std::fs::write(
            env.proc.join("meminfo"),
            format!("MemTotal:       {} kB\nMemFree:          1024 kB\nMemAvailable:   {} kB\nBuffers:            0 kB\n", total_mib * 1024, available_mib * 1024),
        )
        .unwrap();
    }

    /// A model's file as the updater leaves it: of the catalogue's size
    /// (sparse: no byte is written) with the record that vouches for it.
    pub(crate) fn install(model: &'static contract::NativeModel, env: &Env) {
        std::fs::create_dir_all(model.dir(&env.models)).unwrap();
        let file = std::fs::File::create(model.file_path(&env.models)).unwrap();
        file.set_len(model.bytes).unwrap();
        let meta = std::fs::metadata(model.file_path(&env.models)).unwrap();
        contract::write_verified(&model.verified_path(&env.models), &contract::Verified::of(model, &meta)).unwrap();
        assert_eq!(contract::installed(model, &env.models), contract::Installed::Verified);
    }

    /// A stand-in for the updater: a shell script, run as `/bin/sh <script>`
    /// so that no test executes a file it has just written.
    pub(crate) fn stand_in(root: &std::path::Path, body: &str) -> Vec<OsString> {
        let script = root.join("updater.sh");
        std::fs::write(&script, format!("{body}\n")).unwrap();
        vec![OsString::from("/bin/sh"), script.into_os_string()]
    }

    /// embrad's status for what is asked: `state` about `wanted`.
    pub(crate) fn status(state: ServiceState, wanted: Option<Wanted>, reason: &str) -> Status {
        let mut status = Status::of(state, 100);
        status.model = wanted.map(|w| w.model);
        status.context = wanted.map(|w| w.context);
        status.pid = state.runs().then_some(4242);
        status.reason = reason.to_string();
        status
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{install, machine, memory, status};
    use super::*;
    use embra_boot::native_model::by_id;

    fn four() -> Wanted {
        Wanted::new(by_id("qwen3.5-4b").unwrap())
    }

    fn nine() -> Wanted {
        Wanted::new(by_id("qwen3.5-9b").unwrap())
    }

    fn config(provider: &str, native_model: &str) -> crate::config::SystemConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Embra", "api_key": "k", "timezone": "UTC", "deployment_mode": "phase2",
            "created_at": "", "version": "test", "api_provider": provider,
            "openai_compat": {"native_model": native_model}
        }))
        .expect("a configuration")
    }

    #[test]
    fn the_record_is_written_from_the_stored_configuration_and_left_when_it_says_the_same() {
        let (_root, env) = machine("follow");
        // No model, and no record: nothing is written, not even the directory.
        let mut cfg = config("anthropic", "");
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::AsItWas));
        assert!(!env.settings.parent().unwrap().exists());

        // A model: the record names it with its default window.
        cfg.openai_compat.set_model(OpenAiCompatPreset::Native, "qwen3.5-4b");
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::Written(Some(four()))));
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(four())));
        assert_eq!(four().context, 65_536);
        let written = std::fs::metadata(&env.settings).unwrap().modified().unwrap();
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::AsItWas));
        assert_eq!(std::fs::metadata(&env.settings).unwrap().modified().unwrap(), written, "the file was not written again");

        // Its window, as the operator set it for this model.
        cfg.openai_compat.native_context_by_model.insert("qwen3.5-4b".into(), 32_768);
        let narrower = Wanted { context: 32_768, ..four() };
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::Written(Some(narrower))));
        // Another model has its own window, the default here.
        cfg.openai_compat.set_model(OpenAiCompatPreset::Native, "qwen3.5-9b");
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::Written(Some(nine()))));
        // A stored window the model cannot be given is its default.
        cfg.openai_compat.native_context_by_model.insert("qwen3.5-9b".into(), 12);
        assert_eq!(wanted_by(&cfg.openai_compat), Ok(Some(nine())));

        // None: the record names none, and embrad ends the server.
        cfg.openai_compat.native_model.clear();
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::Written(None)));
        assert_eq!(contract::read_settings(&env.settings), Ok(None));
        assert_eq!(follow(&env, &cfg.openai_compat), Ok(Followed::AsItWas));

        // A record nobody can read says nothing and is written anew.
        std::fs::write(&env.settings, "model=qwen3.5-4b\nthreads=8\n").unwrap();
        cfg.openai_compat.set_model(OpenAiCompatPreset::Native, "qwen3.5-4b");
        assert!(matches!(follow(&env, &cfg.openai_compat), Ok(Followed::Written(Some(_)))));
        assert!(contract::read_settings(&env.settings).is_ok());

        // A model this release does not know is said, and the record stays.
        cfg.openai_compat.native_model = "qwen9-1t".into();
        assert_eq!(follow(&env, &cfg.openai_compat), Err("the stored native model 'qwen9-1t' is not one this release knows".into()));
        assert!(matches!(contract::read_settings(&env.settings), Ok(Some(_))));
    }

    /// `/native use` and `/native off` decide what is loaded; `/provider`
    /// does not. The same model is asked for under every provider, and
    /// leaving Native asks for nothing else.
    #[test]
    fn the_record_follows_the_model_kept_loaded_and_not_the_provider() {
        for provider in crate::provider::ProviderKind::ALL {
            let loaded = config(provider.as_str(), "qwen3.5-4b");
            assert_eq!(wanted_by(&loaded.openai_compat), Ok(Some(four())), "{provider:?}");
            let none = config(provider.as_str(), "");
            assert_eq!(wanted_by(&none.openai_compat), Ok(None), "{provider:?}");
        }
    }

    #[test]
    fn a_request_goes_waits_or_is_told_why_by_the_two_records() {
        let asked = |wanted: Wanted| Ok(Some(wanted));
        let go = |state| gate(&asked(four()), Some(&status(state, Some(four()), "")), "qwen3.5-4b");
        assert_eq!(go(ServiceState::Ready), Gate::Go);
        assert_eq!(go(ServiceState::Loading), Gate::Go, "the request waits for the load at the server");
        // No status at all: no embrad on this machine, nothing to ask.
        assert_eq!(gate(&asked(four()), None, "qwen3.5-4b"), Gate::Go);
        assert_eq!(gate(&Ok(None), None, "qwen3.5-4b"), Gate::Go);

        // embrad has not looked since the record changed: the status is about
        // nothing, about another model, or about another window.
        for stale in [
            status(ServiceState::Off, None, ""),
            status(ServiceState::Ready, Some(nine()), ""),
            status(ServiceState::Loading, Some(nine()), ""),
            status(ServiceState::Ready, Some(Wanted { context: 32_768, ..four() }), ""),
            status(ServiceState::Halted, Some(nine()), "the model server ended 3 times before it answered"),
            status(ServiceState::Unavailable, None, "the settings were not taken (threads=8: not a setting this release knows)"),
        ] {
            assert_eq!(gate(&asked(four()), Some(&stale), "qwen3.5-4b"), Gate::Wait, "{stale:?}");
        }

        // No server will come, and why.
        let no = |wanted: &Result<Option<Wanted>, String>, status: Status| match gate(wanted, Some(&status), "qwen3.5-4b") {
            Gate::No(refusal) => refusal.sentence(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            no(&Ok(None), status(ServiceState::Off, None, "")),
            "No native model is chosen. /native lists the models, and /native use <model> loads one (experimental)."
        );
        assert_eq!(
            no(&asked(four()), status(ServiceState::Unavailable, Some(four()), "qwen3.5-4b is not installed")),
            "the native model is not loaded: qwen3.5-4b is not installed. /native shows what the model server is doing"
        );
        assert_eq!(
            no(&asked(four()), status(ServiceState::Halted, Some(four()), "the model server ended 3 times before it answered (signal 9: killed); nothing more is tried until the settings change")),
            "the native model is not loaded: the model server ended 3 times before it answered (signal 9: killed); nothing more is tried until the settings change. /native shows what the model server is doing"
        );
        assert_eq!(
            no(&asked(four()), status(ServiceState::Halted, Some(four()), "")),
            "the native model is not loaded: the model server is halted. /native shows what the model server is doing"
        );
        assert_eq!(
            no(&asked(four()), status(ServiceState::Disabled, None, "native models are switched off on the kernel line (embra.native=0)")),
            "native models are switched off on the kernel line (embra.native=0)"
        );
        assert_eq!(
            no(&asked(nine()), status(ServiceState::Ready, Some(nine()), "")),
            "the model server is asked for qwen3.5-9b and the request is for qwen3.5-4b"
        );
        assert_eq!(
            no(&Err("model=x: not a native model this release knows".into()), status(ServiceState::Off, None, "")),
            "the native model's settings cannot be read (model=x: not a native model this release knows)"
        );
    }

    /// A day on the paused clock around a wait under test: a wait that has
    /// lost its bound is at this deadline at once, where it would never end.
    async fn within_a_day<T>(wait: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(24 * 3600), wait).await.expect("the wait ends by its own bound")
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waits_until_embrad_has_followed_the_record() {
        let (_root, env) = machine("until-followed");
        contract::write_settings(&env.settings, Some(&four())).unwrap();
        // embrad still says what it kept before: the larger model, ready.
        contract::write_status(&env.status, &status(ServiceState::Ready, Some(nine()), "")).unwrap();

        let start = tokio::time::Instant::now();
        let embrad = {
            let path = env.status.clone();
            async move {
                // Between two of the request's looks, which fall on whole seconds.
                tokio::time::sleep(Duration::from_millis(3500)).await;
                contract::write_status(&path, &status(ServiceState::Loading, Some(four()), "")).unwrap();
            }
        };
        let (went, ()) = within_a_day(async { tokio::join!(until_followed(&env, "qwen3.5-4b", Duration::from_secs(300)), embrad) }).await;
        assert!(went.is_ok(), "{went:?}");
        assert_eq!(start.elapsed(), Duration::from_secs(4), "at its first look after embrad's");

        // Already followed: no wait at all.
        let start = tokio::time::Instant::now();
        within_a_day(until_followed(&env, "qwen3.5-4b", Duration::from_secs(300))).await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_is_told_at_once_when_no_server_will_come_and_after_the_wait_when_embrad_never_follows() {
        let (_root, env) = machine("no-server");
        // Nothing is asked for.
        contract::write_status(&env.status, &status(ServiceState::Off, None, "")).unwrap();
        let start = tokio::time::Instant::now();
        let err = within_a_day(until_followed(&env, "qwen3.5-4b", Duration::from_secs(300))).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Connect(text) if text.starts_with("No native model is chosen.")), "{err:?}");
        assert_eq!(start.elapsed(), Duration::ZERO, "nothing to wait for");

        // The file is not there.
        contract::write_settings(&env.settings, Some(&four())).unwrap();
        contract::write_status(&env.status, &status(ServiceState::Unavailable, Some(four()), "qwen3.5-4b is not installed")).unwrap();
        let err = within_a_day(until_followed(&env, "qwen3.5-4b", Duration::from_secs(300))).await.unwrap_err();
        assert_eq!(err.to_string(), "the native model is not loaded: qwen3.5-4b is not installed. /native shows what the model server is doing");

        // An embrad that never looks: the model wait ends it.
        contract::write_status(&env.status, &status(ServiceState::Off, None, "")).unwrap();
        let start = tokio::time::Instant::now();
        let err = within_a_day(until_followed(&env, "qwen3.5-4b", Duration::from_secs(300))).await.unwrap_err();
        let ProviderError::Silent(text) = &err else { panic!("{err:?}") };
        assert_eq!(
            text,
            "embrad has not started the native model after 5 minutes: its status is off; the request was dropped. /native shows what the model server is doing, and /model-wait sets how long embraOS waits"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn a_halted_server_is_asked_for_again_with_nothing_asked_in_between() {
        let (_root, env) = machine("ask-again");
        contract::write_settings(&env.settings, Some(&four())).unwrap();
        let halted = status(ServiceState::Halted, Some(four()), "the model server ended 3 times before it answered");
        contract::write_status(&env.status, &halted).unwrap();

        // embrad, as far as this needs it: at a look it sees the record that
        // names none, and says off.
        let embrad = {
            let (settings, status_path) = (env.settings.clone(), env.status.clone());
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                    if contract::read_settings(&settings) == Ok(None) {
                        contract::write_status(&status_path, &status(ServiceState::Off, None, "")).unwrap();
                        return;
                    }
                }
            }
        };
        let start = tokio::time::Instant::now();
        let (asked, ()) = within_a_day(async { tokio::join!(ask_again_if_halted(&env), embrad) }).await;
        assert_eq!(asked, Ok(true));
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(four())), "the model is asked for again");
        assert_eq!(start.elapsed(), Duration::from_secs(3), "at the first look after embrad's");

        // Nothing is halted: nothing is written, and there is no wait.
        for fine in [
            status(ServiceState::Ready, Some(four()), ""),
            status(ServiceState::Loading, Some(four()), ""),
            status(ServiceState::Unavailable, Some(four()), "qwen3.5-4b is not installed"),
            // Halted on what was asked before: embrad has not looked at this record yet.
            status(ServiceState::Halted, Some(nine()), "ended"),
        ] {
            contract::write_status(&env.status, &fine).unwrap();
            let written = std::fs::metadata(&env.settings).unwrap().modified().unwrap();
            let start = tokio::time::Instant::now();
            assert_eq!(within_a_day(ask_again_if_halted(&env)).await, Ok(false), "{fine:?}");
            assert_eq!(start.elapsed(), Duration::ZERO);
            assert_eq!(std::fs::metadata(&env.settings).unwrap().modified().unwrap(), written);
        }
        std::fs::remove_file(&env.status).unwrap();
        assert_eq!(within_a_day(ask_again_if_halted(&env)).await, Ok(false), "no embrad on this machine");

        // An embrad that never lets go: said after its four looks, and the
        // record asks for the model all the same.
        contract::write_status(&env.status, &halted).unwrap();
        let start = tokio::time::Instant::now();
        assert_eq!(
            within_a_day(ask_again_if_halted(&env)).await,
            Err("embrad did not let go of the model server within 20 seconds".to_string())
        );
        assert_eq!(start.elapsed(), Duration::from_secs(20));
        assert_eq!(contract::read_settings(&env.settings), Ok(Some(four())));
    }

    /// The machine's memory is the kernel's two figures, and what the model
    /// server that runs holds of its own counts as free for the next one.
    #[test]
    fn the_machines_memory_is_the_kernels_with_what_the_running_model_server_holds() {
        let (_root, env) = machine("memory-read");
        assert_eq!(machine_memory(&env), None, "no record, nothing to say");
        memory(&env, 7_700, 3_000);
        assert_eq!(machine_memory(&env), Some(MachineMemory { total: 7_700, free: 3_000 }));
        // A model server runs: its own memory is the next one's.
        let mut ready = status(ServiceState::Ready, Some(four()), "");
        ready.pid = Some(4_242);
        contract::write_status(&env.status, &ready).unwrap();
        assert_eq!(machine_memory(&env), Some(MachineMemory { total: 7_700, free: 3_000 }), "a process the kernel does not show adds nothing");
        std::fs::create_dir_all(env.proc.join("4242")).unwrap();
        std::fs::write(env.proc.join("4242").join("status"), "Name:\tllama-server\nVmRSS:\t 6258688 kB\nRssAnon:\t 3575808 kB\nRssFile:\t 2681856 kB\n").unwrap();
        assert_eq!(machine_memory(&env), Some(MachineMemory { total: 7_700, free: 3_000 + 3_492 }));
        // Never more than the machine has.
        memory(&env, 7_700, 7_000);
        assert_eq!(machine_memory(&env), Some(MachineMemory { total: 7_700, free: 7_700 }));
        // A server embrad no longer runs holds nothing, whatever pid its record still names.
        let mut halted = status(ServiceState::Halted, Some(four()), "ended three times");
        halted.pid = Some(4_242);
        contract::write_status(&env.status, &halted).unwrap();
        assert_eq!(machine_memory(&env), Some(MachineMemory { total: 7_700, free: 7_000 }));
        // A record that is not the kernel's says nothing.
        for broken in ["", "MemTotal: many kB\nMemAvailable: 12 kB\n", "MemTotal:  8000000 kB\n", "MemAvailable:  8000000 kB\n"] {
            std::fs::write(env.proc.join("meminfo"), broken).unwrap();
            assert_eq!(machine_memory(&env), None, "{broken:?}");
        }
        assert_eq!((gb(6_112), gb(1_024), gb(51)), ("6.0".to_string(), "1.0".to_string(), "0.0".to_string()));
    }

    /// The note of memory: at the load when the model itself does not fit
    /// what is free, of its growth when it fits and what its server may
    /// grow to does not, and none with the room or without the kernel's
    /// figures. Before a session it names no command that is not answered
    /// there.
    #[test]
    fn a_machine_short_of_memory_for_a_model_is_told_at_its_load_or_of_its_growth() {
        let (four, nine) = (contract::by_id("qwen3.5-4b").unwrap(), contract::by_id("qwen3.5-9b").unwrap());
        let machine = |total, free| Some(MachineMemory { total, free });
        // The 9B on 12.6 GB, as it was run: it loads, and its server may grow past what is free.
        let note = memory_note(nine, 65_536, true, machine(12_900, 11_800), true).expect("a note");
        assert!(!note.at_the_load);
        assert_eq!(
            note.text,
            "Memory: Qwen3.5 9B with a context window of 65536 tokens takes about 7.8 GB when it is loaded, its image file included, as measured, and its model server can grow by up to 5.6 GB as a session goes on: it keeps what it read of earlier prompts. For both this machine has 11.5 GB free for it (12.6 GB in all). When memory runs short the answers come slowly, or the kernel ends the model server and embrad starts it again. /native off and /native use free what it kept. A smaller window lowers both figures (/native context <tokens>)."
        );
        // The same on 8 GB: the load itself is more than is free.
        let note = memory_note(nine, 65_536, true, machine(7_700, 7_000), true).expect("a note");
        assert!(note.at_the_load);
        assert_eq!(
            note.text,
            "Memory: Qwen3.5 9B with a context window of 65536 tokens takes about 7.8 GB when it is loaded, its image file included, as measured, and this machine has 6.8 GB free for it (7.5 GB in all). Without the room its answers come very slowly, or the kernel ends the model server. A smaller window takes less, about 32 MB for every 1,024 tokens (/native context <tokens>)."
        );
        // Before a session the window's command is for later, and off and use are not named.
        let early = memory_note(nine, 65_536, true, machine(7_700, 7_000), false).unwrap().text;
        assert!(early.ends_with("about 32 MB for every 1,024 tokens (/native context <tokens>, once the setup is done)."), "{early}");
        let early = memory_note(four, 65_536, false, machine(7_700, 7_000), false).unwrap().text;
        assert!(!early.contains("/native off") && early.ends_with("A smaller window lowers both figures (/native context <tokens>, once the setup is done)."), "{early}");
        // The smallest window has no smaller one to name.
        let smallest = memory_note(four, contract::SMALLEST_CONTEXT, false, machine(3_900, 2_500), true).unwrap();
        assert!(smallest.at_the_load && smallest.text.ends_with("or the kernel ends the model server."), "{}", smallest.text);
        // The edges: what is free is exactly the load, then exactly the load and its growth.
        let (loaded, growth) = (four.loaded_mib(65_536, false), four.growth_mib(65_536, false));
        assert!(memory_note(four, 65_536, false, machine(64_000, loaded - 1), true).unwrap().at_the_load);
        assert!(!memory_note(four, 65_536, false, machine(64_000, loaded), true).unwrap().at_the_load);
        assert!(memory_note(four, 65_536, false, machine(64_000, loaded + growth - 1), true).is_some());
        assert_eq!(memory_note(four, 65_536, false, machine(64_000, loaded + growth), true), None);
        // No figures of the machine, nothing said.
        assert_eq!(memory_note(nine, 65_536, true, None, true), None);
    }

    /// An install brings both files, so a model that is not installed is
    /// counted with its image file, and one that stands without it is not.
    #[test]
    fn a_model_is_counted_with_its_image_file_when_it_has_it_or_an_install_brings_it() {
        let (_root, env) = machine("memory-images");
        let model = contract::by_id("qwen3.5-4b").unwrap();
        assert!(would_read_images(model, &env), "not installed: the install brings both");
        install(model, &env);
        assert!(!would_read_images(model, &env), "the model file alone");
        std::fs::File::create(model.path_of(contract::Part::Images, &env.models)).unwrap().set_len(model.images.bytes).unwrap();
        let meta = std::fs::metadata(model.path_of(contract::Part::Images, &env.models)).unwrap();
        contract::write_verified(&model.verified_of(contract::Part::Images, &env.models), &contract::Verified::of_part(model, contract::Part::Images, &meta)).unwrap();
        assert!(would_read_images(model, &env));
    }

    #[test]
    fn the_model_server_is_said_in_a_few_words_for_a_status_line() {
        let (_root, env) = machine("server-words");
        assert_eq!(server_words(&env), None, "no embrad on this machine");
        let words = |said: Status| {
            contract::write_status(&env.status, &said).unwrap();
            server_words(&env).unwrap()
        };
        assert_eq!(words(status(ServiceState::Off, None, "")), "off");
        contract::write_settings(&env.settings, Some(&four())).unwrap();
        assert_eq!(words(status(ServiceState::Off, None, "")), "embrad is about to start it for qwen3.5-4b");
        assert_eq!(words(status(ServiceState::Ready, Some(nine()), "")), "embrad is about to start it for qwen3.5-4b");
        assert_eq!(words(status(ServiceState::Loading, Some(four()), "")), "loading qwen3.5-4b, context window 65536");
        assert_eq!(
            words(status(ServiceState::Ready, Some(four()), "")),
            "ready, context window 65536, reads text alone (its image file is not installed)"
        );
        let mut seeing = status(ServiceState::Ready, Some(four()), "");
        seeing.images = true;
        assert_eq!(words(seeing.clone()), "ready, context window 65536, reads images");

        // The backend its processor is given, once embrad's status names it:
        // said by `/native`'s words, and by the provider's line never.
        assert_eq!(server_words_with_backend(&env).as_deref(), Some("ready, context window 65536, reads images"), "nobody has named it yet");
        let named = |state: ServiceState, wanted: Option<Wanted>, images: bool| {
            let mut said = status(state, wanted, "");
            (said.images, said.backend) = (images, Some("armv8.2_2".to_string()));
            contract::write_status(&env.status, &said).unwrap();
            (server_words(&env).unwrap(), server_words_with_backend(&env).unwrap())
        };
        assert_eq!(
            named(ServiceState::Ready, Some(four()), true),
            ("ready, context window 65536, reads images".to_string(), "ready, context window 65536, reads images, processor backend armv8.2_2".to_string())
        );
        assert_eq!(
            named(ServiceState::Ready, Some(four()), false).1,
            "ready, context window 65536, reads text alone (its image file is not installed), processor backend armv8.2_2"
        );
        assert_eq!(named(ServiceState::Loading, Some(four()), false).1, "loading qwen3.5-4b, context window 65536, processor backend armv8.2_2");
        // A status about another model, and a state without a server: the
        // words say nothing of a backend.
        assert_eq!(named(ServiceState::Ready, Some(nine()), true).1, "embrad is about to start it for qwen3.5-4b");
        assert_eq!(named(ServiceState::Halted, Some(four()), false).1, "not loaded: the model server is halted");
        assert_eq!(words(status(ServiceState::Unavailable, Some(four()), "qwen3.5-4b is not installed")), "not loaded: qwen3.5-4b is not installed");
        assert_eq!(words(status(ServiceState::Halted, Some(four()), "")), "not loaded: the model server is halted");
        assert_eq!(
            words(status(ServiceState::Disabled, None, "native models are switched off on the kernel line (embra.native=0)")),
            "native models are switched off on the kernel line (embra.native=0)"
        );
    }

    #[test]
    fn a_server_that_does_not_answer_is_starting_not_loaded_or_unknown_by_the_records() {
        let (_root, env) = machine("no-answer");
        // No status: the records say nothing.
        assert_eq!(no_answer(&env, "qwen3.5-4b"), NoAnswer::Unknown);
        contract::write_settings(&env.settings, Some(&four())).unwrap();
        for (said, expected) in [
            // embrad has not looked yet.
            (status(ServiceState::Off, None, ""), NoAnswer::Starting),
            // It started the server, which does not listen yet.
            (status(ServiceState::Loading, Some(four()), ""), NoAnswer::Starting),
            // It says the server answers, and it does not: the records do not say.
            (status(ServiceState::Ready, Some(four()), ""), NoAnswer::Unknown),
            (
                status(ServiceState::Halted, Some(four()), "the model server ended 3 times before it answered"),
                // The reason alone: the probe's notice names `/native` itself.
                NoAnswer::NotLoaded("the native model is not loaded: the model server ended 3 times before it answered".into()),
            ),
        ] {
            contract::write_status(&env.status, &said).unwrap();
            assert_eq!(no_answer(&env, "qwen3.5-4b"), expected, "{said:?}");
        }
        // A status nobody can read is no status.
        std::fs::write(&env.status, "state=asleep\n").unwrap();
        assert_eq!(no_answer(&env, "qwen3.5-4b"), NoAnswer::Unknown);
    }
}
