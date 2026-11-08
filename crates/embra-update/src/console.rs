//! Where the updater speaks and listens. In a maintenance boot (`--console`)
//! that is the boot consoles: the serial line and the display, the prompt on
//! both, the first line typed on either answering (`BootConsole`). Otherwise
//! stdio: the brain's child, a developer's terminal. A write never fails — a
//! console that refuses one (a UART that is not there) is skipped, never a
//! panic of a print macro — and `ask` is `None` when nobody answers.
//!
//! The restore prompt lives here too: a numbered list, the number asked
//! twice, anything else reboots without restoring. No timeout: an operator
//! is at a console.

#[cfg(test)]
use std::collections::VecDeque;
use std::io::{self, Write};

use embra_boot::console::BootConsole;

use crate::snapshot::Listed;

pub enum Operator {
    Boot(Box<BootConsole>),
    Stdio,
    /// A scripted operator for the tests: the answers in order, every byte
    /// written kept.
    #[cfg(test)]
    Scripted { answers: VecDeque<String>, transcript: Vec<u8> },
}

impl Operator {
    /// The boot consoles under `--console`, stdio otherwise.
    pub fn for_console(console: bool) -> Operator {
        if console { Operator::Boot(Box::new(BootConsole::open_interactive())) } else { Operator::Stdio }
    }

    #[cfg(test)]
    pub fn scripted(answers: &[&str]) -> Operator {
        Operator::Scripted { answers: answers.iter().map(|s| s.to_string()).collect(), transcript: Vec::new() }
    }

    #[cfg(test)]
    pub fn transcript(&self) -> String {
        match self {
            Operator::Scripted { transcript, .. } => String::from_utf8_lossy(transcript).into_owned(),
            _ => String::new(),
        }
    }

    /// One line.
    pub fn line(&mut self, text: &str) {
        let _ = self.write_all(text.as_bytes());
        let _ = self.write_all(b"\n");
        let _ = self.flush();
    }

    /// A prompt and the line that answers it, trimmed; `None` when nobody can.
    pub fn ask(&mut self, prompt: &str) -> Option<String> {
        match self {
            Operator::Boot(console) => console.prompt_line(prompt).ok().flatten(),
            Operator::Stdio => {
                let mut out = io::stdout();
                let _ = out.write_all(prompt.as_bytes());
                let _ = out.flush();
                let mut line = String::new();
                match io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line.trim().to_string()),
                }
            }
            #[cfg(test)]
            Operator::Scripted { answers, transcript } => {
                transcript.extend_from_slice(prompt.as_bytes());
                answers.pop_front()
            }
        }
    }
}

impl Write for Operator {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Operator::Boot(console) => {
                let _ = console.write_all(buf);
            }
            Operator::Stdio => {
                let _ = io::stdout().write_all(buf);
            }
            #[cfg(test)]
            Operator::Scripted { transcript, .. } => transcript.extend_from_slice(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Operator::Boot(console) => {
                let _ = console.flush();
            }
            Operator::Stdio => {
                let _ = io::stdout().flush();
            }
            #[cfg(test)]
            Operator::Scripted { .. } => {}
        }
        Ok(())
    }
}

/// `y`, `Y` or `yes` confirms; anything else, or no answer, does not.
pub fn confirmed(answer: Option<&str>) -> bool {
    matches!(answer.map(str::trim), Some("y" | "Y" | "yes"))
}

/// The index chosen and confirmed, or `None`.
pub fn choose(entries: &[Listed], op: &mut Operator) -> std::io::Result<Option<usize>> {
    op.line("embraOS — restore a backup");
    if entries.is_empty() {
        op.line("  no backups under the backups directory; nothing to restore");
        return Ok(None);
    }
    let restorable: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.complete).map(|(i, _)| i).collect();
    for (n, e) in entries.iter().enumerate() {
        let mark = if e.complete { "" } else { "   (incomplete — not restorable)" };
        op.line(&format!("  [{}] {}{}", n + 1, e.line(), mark));
    }
    if restorable.is_empty() {
        op.line("  no complete backup to restore");
        return Ok(None);
    }
    let Some(first) = op.ask(&format!("Restore which backup? [1-{}, q = reboot without restoring]: ", entries.len())) else {
        return Ok(None);
    };
    let Some(index) = parse_choice(&first, entries.len()) else { return Ok(None) };
    if !entries[index].complete {
        op.line(&format!("backup {} is incomplete and cannot be restored", first));
        return Ok(None);
    }
    op.line(&format!(
        "This replaces STATE and DATA (the knowledge graph, the sessions, the workspace) with backup {} and keeps the other backups.",
        first
    ));
    let Some(second) = op.ask(&format!("Type {} again to confirm: ", first)) else { return Ok(None) };
    if parse_choice(&second, entries.len()) == Some(index) { Ok(Some(index)) } else { Ok(None) }
}

fn parse_choice(line: &str, count: usize) -> Option<usize> {
    let n: usize = line.trim().parse().ok()?;
    (1..=count).contains(&n).then_some(n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Listed> {
        vec![
            Listed { name: "2026-11-05_101200_pre-v0.24.0".into(), complete: true, archive_bytes: 298_000_000, record: None },
            Listed { name: "2026-11-01_193000_manual".into(), complete: true, archive_bytes: 240_000_000, record: None },
            Listed { name: "2026-10-30_080000_weekly".into(), complete: false, archive_bytes: 0, record: None },
        ]
    }

    #[test]
    fn the_restore_prompt_takes_a_number_twice_and_anything_else_reboots_without_restoring() {
        let mut op = Operator::scripted(&["2", "2"]);
        assert_eq!(choose(&entries(), &mut op).unwrap(), Some(1));
        let text = op.transcript();
        assert!(text.contains("[1] 2026-11-05_101200_pre-v0.24.0"));
        assert!(text.contains("[3] 2026-10-30_080000_weekly") && text.contains("not restorable"));
        assert!(text.contains("Restore which backup? [1-3, q = reboot without restoring]: "));
        assert_eq!(choose(&entries(), &mut Operator::scripted(&["2", "1"])).unwrap(), None, "a different second answer");
        assert_eq!(choose(&entries(), &mut Operator::scripted(&["q"])).unwrap(), None);
        assert_eq!(choose(&entries(), &mut Operator::scripted(&[""])).unwrap(), None);
        assert_eq!(choose(&entries(), &mut Operator::scripted(&["3", "3"])).unwrap(), None, "an incomplete backup");
        assert_eq!(choose(&entries(), &mut Operator::scripted(&[])).unwrap(), None, "nobody answers");
        assert_eq!(choose(&[], &mut Operator::scripted(&["1"])).unwrap(), None);
    }

    #[test]
    fn the_apply_confirmation_takes_y_and_nothing_else() {
        assert!(confirmed(Some("y")));
        assert!(confirmed(Some("Y")));
        assert!(confirmed(Some(" yes ")));
        assert!(!confirmed(Some("n")));
        assert!(!confirmed(Some("")));
        assert!(!confirmed(Some("yes please")));
        assert!(!confirmed(None));
    }

    /// A scripted or stdio operator never fails a write; the point is that a
    /// dead console cannot end the updater through a print macro.
    #[test]
    fn a_write_to_the_operator_never_fails() {
        let mut op = Operator::scripted(&[]);
        assert!(writeln!(op, "download: 10/491 MB").is_ok());
        assert!(op.flush().is_ok());
        assert_eq!(op.transcript(), "download: 10/491 MB\n");
    }
}
