//! The licence the OS is released under, and the operator's acceptance of it
//! on this machine.
//!
//! The text is the image's own (`embra_boot::license::PATH`, which the image
//! build holds to the repository's `LICENSE` byte for byte). An acceptance is
//! of one text: its record on STATE names the text by its version line and
//! its SHA-256, with when and where it was given. A record of another text,
//! or none, leaves the licence open: the first boot asks before the name
//! (`config::run_config_wizard_with`), and a configured machine asks the
//! operator at a console (`grpc_service`'s operational loop) and holds the
//! operator's input until the answer. An update that brings another text is
//! asked about once the same way.
//!
//! The question holds what a console sends, and puts off the briefing of a
//! session that is attached meanwhile; nothing else. No service's health, no
//! confirmation of a trial boot, no reconcile loop, no scheduled job and no
//! `/continue` reads this module: an update that waited for a person would
//! fall back to the slot before it.
//!
//! A machine whose image carries no text of the licence's shape (a build
//! from before the text travelled with the image) is asked nothing: there
//! is no text to show, and what is accepted is a text that was shown.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

/// Where the operator's acceptance is recorded, on STATE: in its snapshots,
/// and read by no tool (STATE's read jail allows its drop-ins alone).
pub(crate) const RECORD_PATH: &str = "/embra/state/license-acceptance";

/// The answer that accepts. It is the question's last option: a console's
/// selector stands on its first, and Enter alone accepts nothing.
pub(crate) const ACCEPT: &str = "I accept the license";
/// The answer that accepts nothing, and the first option.
pub(crate) const DECLINE: &str = "I do not accept";
/// The answer that shows the whole text and asks again. A console that
/// shows a list of choices takes no typed command until one is chosen, so
/// `/license` cannot be the way to the text from the question itself.
pub(crate) const SHOW: &str = "Show the full text";

/// Where an acceptance was given, as its record says: the first boot's
/// question.
pub(crate) const AT_THE_WIZARD: &str = "wizard";
/// The question a configured machine asks at a console.
pub(crate) const AT_A_SESSION: &str = "session";

/// What the operator is shown at the first boot and after an update. The
/// wording is the operator's licence review's (2026-11-26) and is used as
/// written: changed there first, and here as it is given. `{version}` is
/// where the text's version line goes.
const NOTICE: &str = "\
embraOS is licensed, not sold, under the embraOS Proprietary License,
{version}. It is for personal and academic
non-commercial use; commercial use needs a separate license.

embraOS is experimental and is provided as is, without warranty. It runs
an AI system that can act on its own with the access you give it. By
accepting you take on the risks of running it on this machine, and you
release Ward Software Defined Systems LLC from claims, as Sections 7 to
12 of the license describe. /license shows the full text.";

/// The question's own line, after the notice.
const QUESTION: &str = "Do you accept the embraOS Proprietary License?";

/// How many lines of the text a message of `/license` carries.
const LINES_A_MESSAGE: usize = 150;

/// The licence's text as the image carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Text {
    pub(crate) text: String,
    /// Its version line ("Version 2.0, November 26, 2026").
    pub(crate) version: String,
    /// The SHA-256 of its bytes, in hex.
    pub(crate) sha256: String,
}

impl Text {
    /// The text at `path`: `Err` says why there is none to show.
    fn read(path: &Path) -> Result<Text, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{} could not be read ({e})", path.display()))?;
        let version = embra_boot::license::version_line(&text)
            .ok_or_else(|| format!("{} is not the embraOS license's text", path.display()))?
            .to_string();
        let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
        Ok(Text { text, version, sha256 })
    }
}

/// An acceptance, as its record holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Accepted {
    pub(crate) version: String,
    pub(crate) sha256: String,
    /// Seconds since 1970.
    pub(crate) at: u64,
    pub(crate) by: String,
}

impl Accepted {
    fn to_text(&self) -> String {
        format!("version={}\nsha256={}\nat={}\nby={}\n", self.version, self.sha256, self.at, self.by)
    }

    /// When it was given, for a line that shows it.
    pub(crate) fn when(&self) -> String {
        match chrono::DateTime::from_timestamp(i64::try_from(self.at).unwrap_or(i64::MAX), 0) {
            Some(at) => at.format("%Y-%m-%d %H:%M UTC").to_string(),
            None => format!("{} s after 1970", self.at),
        }
    }

    /// Where it was given, in words.
    pub(crate) fn where_given(&self) -> String {
        match self.by.as_str() {
            AT_THE_WIZARD => "at the first boot".to_string(),
            AT_A_SESSION => "at a console".to_string(),
            other => format!("by {other}"),
        }
    }
}

/// A value a record's line carries: printable ASCII on one line.
fn value_ok(text: &str) -> bool {
    !text.is_empty() && text.len() <= 120 && text.trim() == text && text.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
}

/// Parses an acceptance's record: `key=value` a line, its four fields each
/// once, and no line the parser does not know. A record is taken whole or
/// not at all.
pub(crate) fn parse_accepted(text: &str) -> Result<Accepted, String> {
    let (mut version, mut sha256, mut at, mut by) = (None, None, None, None);
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let shown: String = line.chars().take(120).collect();
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{shown}: not a line of the record"));
        };
        let (key, value) = (key.trim(), value.trim());
        let slot = match key {
            "version" | "by" if !value_ok(value) => return Err(format!("{shown}: not a value of the record")),
            "sha256" if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) => {
                return Err(format!("{shown}: not a sha256"));
            }
            "at" if value.parse::<u64>().is_err() => return Err(format!("{shown}: not a time")),
            "version" => &mut version,
            "sha256" => &mut sha256,
            "at" => &mut at,
            "by" => &mut by,
            _ => return Err(format!("{shown}: not a field this release knows")),
        };
        if slot.replace(value.to_string()).is_some() {
            return Err(format!("{shown}: named twice"));
        }
    }
    match (version, sha256, at, by) {
        (Some(version), Some(sha256), Some(at), Some(by)) => Ok(Accepted { version, sha256, at: at.parse().unwrap_or(0), by }),
        _ => Err("the record lacks one of version, sha256, at and by".to_string()),
    }
}

/// How the licence stands on a machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The image carries no text to show: nothing is asked. The sentence
    /// says why.
    NoText(String),
    /// The operator accepted the text this release carries.
    Accepted(Accepted),
    /// The text this release carries is not accepted here. The sentence
    /// says how the record stands: none, one of another text, or one that
    /// is not valid.
    Open(String),
}

/// The licence on a machine: where its text and its acceptance are.
#[derive(Debug)]
pub(crate) struct License {
    text_path: PathBuf,
    record_path: PathBuf,
    /// The text, read once: the root filesystem does not change under a
    /// running OS.
    text: OnceLock<Result<Text, String>>,
    /// Set once nothing is open: an accepted text, or no text to show. A
    /// record does not go away while the OS runs, so the operator's every
    /// message does not read the file again.
    settled: AtomicBool,
}

impl License {
    pub(crate) fn at(text_path: impl Into<PathBuf>, record_path: impl Into<PathBuf>) -> License {
        License { text_path: text_path.into(), record_path: record_path.into(), text: OnceLock::new(), settled: AtomicBool::new(false) }
    }

    /// The machine's own: one for the whole brain, so that what one console
    /// settles is settled for every other at once.
    pub(crate) fn machine() -> Arc<License> {
        static MACHINE: OnceLock<Arc<License>> = OnceLock::new();
        MACHINE.get_or_init(|| Arc::new(License::at(embra_boot::license::PATH, RECORD_PATH))).clone()
    }

    /// The text the image carries, or why there is none to show.
    pub(crate) fn text(&self) -> Result<&Text, String> {
        self.text.get_or_init(|| Text::read(&self.text_path)).as_ref().map_err(String::clone)
    }

    /// How the licence stands here, read from the record.
    pub(crate) fn standing(&self) -> Standing {
        let text = match self.text() {
            Ok(text) => text,
            Err(why) => return Standing::NoText(why),
        };
        let record = match std::fs::read_to_string(&self.record_path) {
            Ok(record) => parse_accepted(&record),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Standing::Open("no acceptance is recorded".to_string()),
            Err(e) => Err(format!("it could not be read: {e}")),
        };
        match record {
            Ok(accepted) if accepted.sha256 == text.sha256 => Standing::Accepted(accepted),
            Ok(accepted) => Standing::Open(format!("the acceptance recorded is of another text of the license ({}), and this release carries {}", accepted.version, text.version)),
            Err(why) => Standing::Open(format!("the acceptance's record is not valid ({why})")),
        }
    }

    /// Whether the operator has yet to accept the text this release carries.
    /// False where there is no text to show.
    pub(crate) fn open(&self) -> bool {
        if self.settled.load(Ordering::Relaxed) {
            return false;
        }
        let open = matches!(self.standing(), Standing::Open(_));
        if !open {
            self.settled.store(true, Ordering::Relaxed);
        }
        open
    }

    /// The notice and the question, with its answers, the refusing one
    /// first: what the first boot and a console are asked while the licence
    /// is open. `None` when nothing is open.
    pub(crate) fn question(&self) -> Option<(String, Vec<String>)> {
        if !self.open() {
            return None;
        }
        let text = self.text().ok()?;
        Some((format!("{}\n\n{QUESTION}", NOTICE.replace("{version}", &text.version)), vec![DECLINE.to_string(), SHOW.to_string(), ACCEPT.to_string()]))
    }

    /// Record the operator's acceptance of the text this release carries,
    /// given now on the surface `by`.
    pub(crate) fn accept(&self, by: &str) -> Result<Accepted, String> {
        let text = self.text()?;
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let accepted = Accepted { version: text.version.clone(), sha256: text.sha256.clone(), at, by: if value_ok(by) { by.to_string() } else { "unnamed".to_string() } };
        write_whole(&self.record_path, &accepted.to_text())
            .map_err(|e| format!("the acceptance could not be written to {} ({e})", self.record_path.display()))?;
        self.settled.store(true, Ordering::Relaxed);
        Ok(accepted)
    }

    /// What is said once an acceptance is recorded.
    pub(crate) fn accepted_words(&self, accepted: &Accepted) -> String {
        format!("Accepted on this machine and recorded: the embraOS Proprietary License, {}. /license shows the full text at any time.", accepted.version)
    }

    /// What a console that still shows the question is told when its answer
    /// arrives after the licence was accepted at another one.
    pub(crate) fn settled_words(&self) -> String {
        match self.standing() {
            Standing::Accepted(accepted) => format!(
                "The embraOS Proprietary License, {}, is accepted on this machine already ({}, {}): nothing is left to answer here.",
                accepted.version,
                accepted.when(),
                accepted.where_given()
            ),
            Standing::NoText(_) | Standing::Open(_) => "Nothing of the embraOS license waits for an answer here.".to_string(),
        }
    }

    /// `/license`: how the licence stands on this machine, then its whole
    /// text, in messages of a console's length.
    pub(crate) fn show(&self) -> Vec<String> {
        let text = match self.text() {
            Ok(text) => text,
            Err(why) => return vec![format!("This image carries no text of the embraOS license to show: {why}. The text is the file LICENSE of the release.")],
        };
        let stands = match self.standing() {
            Standing::Accepted(accepted) => format!("accepted on this machine ({}, {})", accepted.when(), accepted.where_given()),
            Standing::Open(why) => format!("not accepted on this machine yet ({why}): the answer \"{ACCEPT}\" to its question accepts it"),
            Standing::NoText(why) => format!("not shown ({why})"),
        };
        let lines: Vec<&str> = text.text.lines().collect();
        let mut messages = vec![format!(
            "The embraOS Proprietary License, {}: {stands}. Its text, {} lines (sha256 {}):",
            text.version,
            lines.len(),
            text.sha256
        )];
        messages.extend(lines.chunks(LINES_A_MESSAGE).map(|chunk| chunk.join("\n")));
        messages
    }
}

/// What an answer to the question is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The accepting option's own words, and nothing else.
    Accept,
    /// The option that shows the whole text.
    Show,
    /// The refusing option's own words.
    Decline,
    /// No answer to the question: an empty line, or any other word.
    Other,
}

pub(crate) fn answer(text: &str) -> Answer {
    let text = text.trim();
    if text.eq_ignore_ascii_case(ACCEPT) {
        Answer::Accept
    } else if text.eq_ignore_ascii_case(SHOW) {
        Answer::Show
    } else if text.eq_ignore_ascii_case(DECLINE) {
        Answer::Decline
    } else {
        Answer::Other
    }
}

/// What a console of a configured machine is told when the answer accepts
/// nothing. No list of choices follows it: a console that shows one takes
/// no typed command, and this one is left free for the commands that are
/// answered meanwhile (`ANSWERED_WHILE_OPEN`), the way back to the release
/// before an update among them. The next message brings the question back.
pub(crate) const NOT_ACCEPTED: &str = "Not accepted: nothing was changed, and the embraOS license still waits for its answer on this machine. Your next message asks again; /license shows the full text, and /reboot and /update are answered meanwhile.";

/// The same at the first boot, where the setup waits at the question and
/// the commands answered are the ones answered before a session.
pub(crate) const NOT_ACCEPTED_AT_FIRST_BOOT: &str = "Not accepted: nothing was changed, and the setup waits at this question. Your next answer asks again; /license shows the full text, and /reboot and /backup are answered meanwhile.";

/// The commands a console is answered while the licence is open: the text
/// itself, the ways to stop or leave, and the ways back to the release
/// before an update. `/continue` is among them: a turn a failed call cut is
/// the brain's own work, and the question holds the operator's input alone.
pub(crate) const ANSWERED_WHILE_OPEN: [&str; 7] = ["/license", "/help", "/continue", "/stop", "/reboot", "/update", "/backup"];

/// What any other command is told while the licence is open.
pub(crate) fn command_waits(command: &str) -> String {
    let shown: String = command.trim().chars().take(40).collect();
    format!(
        "Not run: {shown}. The embraOS license is not accepted on this machine yet: answer the question first (/license shows the full text). {} are answered meanwhile.",
        ANSWERED_WHILE_OPEN.join(", ")
    )
}

/// Writes a record whole: a temporary file beside it, synced, then renamed
/// over it.
fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// The repository's own licence.
    pub(crate) const REPOSITORY_TEXT: &str = include_str!("../../../LICENSE");

    /// A machine in a directory of its own: the repository's text in its
    /// image (or none, with `text: false`), and no acceptance recorded.
    pub(crate) fn machine(tag: &str, text: bool) -> (PathBuf, License) {
        let root = std::env::temp_dir().join(format!("embra-brain-license-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("image")).unwrap();
        if text {
            std::fs::write(root.join("image/LICENSE"), REPOSITORY_TEXT).unwrap();
        }
        let license = License::at(root.join("image/LICENSE"), root.join("state/license-acceptance"));
        (root, license)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    /// The notice as it is written, with the text's version line in its one
    /// place: plain text of a console's width, naming the sections and the
    /// command that shows the text.
    #[test]
    fn the_notice_is_the_operators_wording_with_the_texts_version_line() {
        let (root, license) = machine("notice", true);
        let (prompt, options) = license.question().expect("the licence is open on a new machine");
        assert_eq!(
            prompt,
            "embraOS is licensed, not sold, under the embraOS Proprietary License,\n\
             Version 2.0, November 26, 2026. It is for personal and academic\n\
             non-commercial use; commercial use needs a separate license.\n\
             \n\
             embraOS is experimental and is provided as is, without warranty. It runs\n\
             an AI system that can act on its own with the access you give it. By\n\
             accepting you take on the risks of running it on this machine, and you\n\
             release Ward Software Defined Systems LLC from claims, as Sections 7 to\n\
             12 of the license describe. /license shows the full text.\n\
             \n\
             Do you accept the embraOS Proprietary License?"
                .replace("Version 2.0, November 26, 2026", &license.text().unwrap().version)
        );
        assert_eq!(options, ["I do not accept", "Show the full text", "I accept the license"], "the refusing answer first: Enter alone accepts nothing");
        assert_eq!(NOTICE.matches("{version}").count(), 1);
        assert!(NOTICE.is_ascii() && NOTICE.lines().all(|line| line.len() <= 74 && line.trim_end() == line), "{NOTICE}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The terminal console names the first boot's step by the question's
    /// own words (`infer_setup_step`): a question that is worded anew is
    /// worded anew there.
    #[test]
    fn the_terminal_console_knows_the_question_by_its_words() {
        let console = Path::new(env!("CARGO_MANIFEST_DIR")).join("../embra-console/src/terminal/state.rs");
        let source = std::fs::read_to_string(&console).unwrap();
        let words = QUESTION.trim_end_matches('?').to_lowercase();
        assert!(source.contains(&format!("lower.contains(\"{words}\")")), "{} does not look for {words:?}", console.display());
    }

    /// The licence is open until the operator's acceptance of this
    /// release's text is recorded: its version line, its sha256, when and
    /// where. Only the accepting option's own words accept. A record of
    /// another text, and one that is not whole, leave it open.
    #[test]
    fn the_license_is_open_until_this_releases_text_is_accepted() {
        let (root, license) = machine("standing", true);
        let text = license.text().unwrap().clone();
        assert_eq!(text.sha256, hex::encode(Sha256::digest(REPOSITORY_TEXT.as_bytes())));
        assert_eq!(text.sha256.len(), 64);
        assert_eq!(license.standing(), Standing::Open("no acceptance is recorded".to_string()));
        assert!(license.open());
        for (said, is) in [
            ("I accept the license", Answer::Accept),
            (" i accept the license ", Answer::Accept),
            ("Show the full text", Answer::Show),
            ("I do not accept", Answer::Decline),
            (" i do not accept\n", Answer::Decline),
            ("I do not accept the license", Answer::Other),
            ("", Answer::Other),
            ("yes", Answer::Other),
            ("accept", Answer::Other),
            ("I accept", Answer::Other),
            ("I accept the license.", Answer::Other),
        ] {
            assert_eq!(answer(said), is, "{said:?}");
        }

        let accepted = license.accept(AT_THE_WIZARD).unwrap();
        assert_eq!((accepted.version.as_str(), accepted.sha256.as_str(), accepted.by.as_str()), (text.version.as_str(), text.sha256.as_str(), "wizard"));
        assert!(accepted.at > 1_700_000_000);
        let record = std::fs::read_to_string(root.join("state/license-acceptance")).unwrap();
        assert_eq!(record, format!("version={}\nsha256={}\nat={}\nby=wizard\n", text.version, text.sha256, accepted.at));
        assert_eq!(parse_accepted(&record), Ok(accepted.clone()));
        assert_eq!(std::fs::read_dir(root.join("state")).unwrap().count(), 1, "the record alone, no temporary file");
        assert_eq!(license.accepted_words(&accepted), format!("Accepted on this machine and recorded: the embraOS Proprietary License, {}. /license shows the full text at any time.", text.version));

        // Another reader of the same machine (the brain after a restart) finds it accepted, and asks nothing.
        let again = License::at(root.join("image/LICENSE"), root.join("state/license-acceptance"));
        assert_eq!(again.standing(), Standing::Accepted(accepted.clone()));
        assert!(!again.open() && again.question().is_none());
        assert_eq!(
            again.settled_words(),
            format!("The embraOS Proprietary License, {}, is accepted on this machine already ({}, at the first boot): nothing is left to answer here.", text.version, accepted.when())
        );

        // An update that brings another text: the record is of the one before, and the licence is open again.
        std::fs::write(root.join("image/LICENSE"), REPOSITORY_TEXT.replacen("Version 2.0, November 26, 2026", "Version 2.1, January 5, 2027", 1)).unwrap();
        let updated = License::at(root.join("image/LICENSE"), root.join("state/license-acceptance"));
        assert_eq!(
            updated.standing(),
            Standing::Open(format!("the acceptance recorded is of another text of the license ({}), and this release carries Version 2.1, January 5, 2027", text.version))
        );
        assert!(updated.open());
        assert!(updated.question().unwrap().0.contains("\nVersion 2.1, January 5, 2027. It is for personal and academic\n"));
        let second = updated.accept(AT_A_SESSION).unwrap();
        assert_eq!((second.version.as_str(), second.by.as_str()), ("Version 2.1, January 5, 2027", "session"));
        assert!(!updated.open());

        // A record that is not whole, or carries what the parser does not know, settles nothing.
        for (record, part) in [
            ("version=Version 2.0\nsha256=abc\nat=1\nby=wizard\n", "not a sha256"),
            (&format!("version=V\nsha256={}\nat=soon\nby=wizard\n", "a".repeat(64)), "not a time"),
            (&format!("version=V\nsha256={}\nat=1\n", "a".repeat(64)), "lacks one of version, sha256, at and by"),
            (&format!("version=V\nsha256={}\nat=1\nby=wizard\nby=session\n", "a".repeat(64)), "named twice"),
            (&format!("version=V\nsha256={}\nat=1\nby=wizard\nsigned=yes\n", "a".repeat(64)), "not a field this release knows"),
            (&format!("version=V\nsha256={}\nat=1\nby=wizard\n", "A".repeat(64)), "not a sha256"),
            ("accepted\n", "not a line of the record"),
            ("", "lacks one of version, sha256, at and by"),
        ] {
            std::fs::write(root.join("state/license-acceptance"), record).unwrap();
            let fresh = License::at(root.join("image/LICENSE"), root.join("state/license-acceptance"));
            let Standing::Open(why) = fresh.standing() else { panic!("{record:?} settled the licence") };
            assert!(why.starts_with("the acceptance's record is not valid (") && why.contains(part), "{record:?}: {why}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An image without a text of the licence's shape is asked nothing:
    /// there is no text to show, and `/license` says so.
    #[test]
    fn an_image_without_the_text_is_asked_nothing() {
        let (root, license) = machine("no-text", false);
        assert!(matches!(license.standing(), Standing::NoText(why) if why.contains("could not be read")));
        assert!(!license.open() && license.question().is_none());
        assert!(license.accept(AT_THE_WIZARD).is_err(), "nothing is recorded of a text that was not shown");
        assert!(!root.join("state").exists());
        let shown = license.show();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].starts_with("This image carries no text of the embraOS license to show: "), "{}", shown[0]);
        // Another file in its place is no licence either.
        std::fs::write(root.join("image/LICENSE"), "Some other text\n\nVersion 9\n").unwrap();
        let other = License::at(root.join("image/LICENSE"), root.join("state/license-acceptance"));
        assert!(matches!(other.standing(), Standing::NoText(why) if why.ends_with("is not the embraOS license's text")));
        assert!(!other.open());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `/license` says how the licence stands and shows the whole text, in
    /// messages a console can hold: every line, in order.
    #[test]
    fn license_shows_how_it_stands_and_the_whole_text() {
        let (root, license) = machine("show", true);
        let text = license.text().unwrap().clone();
        let lines = REPOSITORY_TEXT.lines().count();
        let shown = license.show();
        assert_eq!(
            shown[0],
            format!(
                "The embraOS Proprietary License, {}: not accepted on this machine yet (no acceptance is recorded): the answer \"I accept the license\" to its question accepts it. Its text, {lines} lines (sha256 {}):",
                text.version, text.sha256
            )
        );
        assert_eq!(shown.len(), 1 + lines.div_ceil(LINES_A_MESSAGE));
        assert!(shown[1..].iter().all(|message| message.lines().count() <= LINES_A_MESSAGE));
        assert_eq!(shown[1..].join("\n"), REPOSITORY_TEXT.trim_end_matches('\n'), "every line of the text, in order");
        license.accept(AT_A_SESSION).unwrap();
        let after = license.show();
        assert!(after[0].contains(": accepted on this machine (20") && after[0].contains(" UTC, at a console). Its text, "), "{}", after[0]);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// While the licence is open a command is told to wait, but the ones
    /// that show the text, stop or leave, go back to another release, or
    /// take up a cut turn.
    #[test]
    fn the_commands_answered_while_the_license_is_open_are_named() {
        assert_eq!(ANSWERED_WHILE_OPEN, ["/license", "/help", "/continue", "/stop", "/reboot", "/update", "/backup"]);
        assert_eq!(
            command_waits("/provider"),
            "Not run: /provider. The embraOS license is not accepted on this machine yet: answer the question first (/license shows the full text). /license, /help, /continue, /stop, /reboot, /update, /backup are answered meanwhile."
        );
        assert!(command_waits(&"x".repeat(500)).len() < 320);
    }
}
