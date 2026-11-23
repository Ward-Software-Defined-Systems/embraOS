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
}

impl Env {
    pub(crate) fn in_force() -> Self {
        Env {
            settings: PathBuf::from(contract::SETTINGS_PATH),
            status: PathBuf::from(contract::STATUS_PATH),
            models: PathBuf::from(contract::MODELS_DIR),
            updater: vec![OsString::from(crate::grpc_service::UPDATER)],
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

/// The question that offers the catalogue's models, and its options: the
/// ids, with `current` (the model kept loaded) first, since a console's
/// selector starts on the first option whatever the default. `/model`,
/// `/provider --setup native` and the wizard ask it.
pub(crate) fn catalogue_question(current: Option<&str>, env: &Env) -> (String, Vec<String>) {
    let mut ids: Vec<String> = contract::CATALOGUE.iter().map(|m| m.id.to_string()).collect();
    if let Some(at) = current.and_then(|current| ids.iter().position(|id| id == current)) {
        let kept = ids.remove(at);
        ids.insert(0, kept);
    }
    let stands: Vec<String> = contract::CATALOGUE
        .iter()
        .map(|m| format!("{} is {} ({}, {} MB): {}", m.id, m.label, m.quantisation, mb(m.bytes), file_words(m, env)))
        .collect();
    (format!("Select a native model (experimental). {}.", stands.join("; ")), ids)
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
    let (wanted, status) = env.records();
    let status = status?;
    let asked = wanted.as_ref().ok().and_then(|wanted| wanted.as_ref());
    // Whether the status is about what the record asks now.
    let follows = asked.is_some_and(|wanted| status.model == Some(wanted.model) && status.context == Some(wanted.context));
    Some(match (status.state, asked) {
        (ServiceState::Disabled, _) => said(&status),
        (_, None) => "off".to_string(),
        (_, Some(wanted)) if !follows => format!("embrad is about to start it for {}", wanted.model.id),
        (ServiceState::Ready, Some(wanted)) => format!("ready, context window {}", wanted.context),
        (ServiceState::Loading, Some(wanted)) => format!("loading {}, context window {}", wanted.model.id, wanted.context),
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
        };
        (root, env)
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
    use super::test_support::{machine, status};
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
        assert_eq!(words(status(ServiceState::Ready, Some(four()), "")), "ready, context window 65536");
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
