//! embra-update — the OS's own backups and updates: snapshots of STATE and
//! DATA and their restore; the signed release checked, written into the
//! inactive slot and staged for GRUB; the status and the rollback. Run by
//! embrad in a maintenance boot from the GRUB menu and by the brain for the
//! operator's slash commands; never a tool of the model.
//!
//! Exit codes, read by both callers: 0 done; 1 failed, the last stderr line
//! is the reason; 2 refused by a precondition, nothing was changed; 3 nothing
//! to do. Progress goes to stdout, one line per phase.

mod cli;
mod console;
mod db;
mod fetch;
mod manifest;
mod paths;
mod pubkey;
mod slot;
mod snapshot;
mod source;
mod update;

use std::io::Write;
use std::process::ExitCode;

use cli::Command;
use snapshot::Roots;

pub const EXIT_DONE: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_REFUSED: u8 = 2;
pub const EXIT_NOTHING: u8 = 3;

/// What a command came to.
pub enum Outcome {
    Done,
    /// The precondition that stopped it; nothing was changed.
    Refused(String),
    /// Nothing to do, and why.
    Nothing(String),
    /// It failed on the way; the reason is the last line.
    Failed(String),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("{}", cli::USAGE);
            return ExitCode::from(EXIT_REFUSED);
        }
    };
    match run(cmd) {
        Ok(Outcome::Done) => ExitCode::from(EXIT_DONE),
        Ok(Outcome::Refused(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(EXIT_REFUSED)
        }
        Ok(Outcome::Nothing(msg)) => {
            println!("{msg}");
            ExitCode::from(EXIT_NOTHING)
        }
        Ok(Outcome::Failed(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(EXIT_FAILED)
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

fn run(cmd: Command) -> anyhow::Result<Outcome> {
    let roots = Roots::os();
    match cmd {
        Command::Snapshot { label, taken_by, once, force } => {
            if !force && db::database_is_running() {
                return Ok(Outcome::Refused(
                    "refused: the database is running, so a copy of its files would be inconsistent; a snapshot runs before the services start (the boot menu's entry, or /backup create for the next boot)".into(),
                ));
            }
            if once && snapshot::list(&roots).iter().any(|e| e.complete && e.label() == Some(label.as_str())) {
                return Ok(Outcome::Nothing(format!("snapshot \"{label}\" exists already; nothing to do")));
            }
            let release = embra_boot::Release::read().ok();
            let record = match snapshot::create(&roots, &label, &taken_by, release.as_ref(), &mut std::io::stdout())? {
                snapshot::CreateOutcome::Created(r) => *r,
                snapshot::CreateOutcome::Refused(msg) => return Ok(Outcome::Refused(msg)),
            };
            let rotated = snapshot::rotate(&roots)?;
            if !rotated.is_empty() {
                println!("rotation: removed {}", rotated.join(", "));
            }
            println!(
                "snapshot: {} files, {} MB → {}",
                record.files,
                record.bytes_archive / (1024 * 1024),
                roots.backups.join(&record.name).display()
            );
            Ok(Outcome::Done)
        }
        Command::Restore { name, yes, console } => {
            if db::database_is_running() {
                return Ok(Outcome::Refused(
                    "refused: the database is running; a restore runs before the services start (the boot menu's entry, or /backup restore for the next boot)".into(),
                ));
            }
            let entries = snapshot::list(&roots);
            let name = match name {
                Some(n) => n,
                None if console => {
                    let stdin = std::io::stdin();
                    let mut input = stdin.lock();
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    match console::choose(&entries, &mut input, &mut out)? {
                        Some(i) => entries[i].name.clone(),
                        None => return Ok(Outcome::Refused("restore: nothing chosen; nothing was changed".into())),
                    }
                }
                None => return Ok(Outcome::Refused("restore: name the backup (embra-update list), or --console to choose".into())),
            };
            if !yes && !console {
                return Ok(Outcome::Refused(format!(
                    "restore: this replaces STATE and DATA with \"{name}\"; add --yes to proceed"
                )));
            }
            let release = embra_boot::Release::read().ok();
            match snapshot::restore(&roots, &name, release.as_ref(), &mut std::io::stdout())? {
                snapshot::RestoreOutcome::Restored => Ok(Outcome::Done),
                snapshot::RestoreOutcome::Refused(msg) => Ok(Outcome::Refused(msg)),
            }
        }
        Command::List { names_only } => {
            let entries = snapshot::list(&roots);
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if entries.is_empty() {
                writeln!(out, "no backups under {}", roots.backups.display())?;
                return Ok(Outcome::Done);
            }
            for e in &entries {
                if names_only {
                    if e.complete {
                        writeln!(out, "{}", e.name)?;
                    }
                } else {
                    writeln!(out, "{}", e.line())?;
                }
            }
            Ok(Outcome::Done)
        }
        Command::Delete { name } => match snapshot::delete(&roots, &name)? {
            true => {
                println!("deleted {name}");
                Ok(Outcome::Done)
            }
            false => Ok(Outcome::Refused(format!("delete: no backup named \"{name}\""))),
        },
        Command::Check { source, tag } => Ok(update::check_command(source.as_deref(), tag.as_deref(), &mut std::io::stdout())),
        Command::Apply { source, tag, yes, console, allow_downgrade } => {
            Ok(update::apply_command(source.as_deref(), tag.as_deref(), yes, console, allow_downgrade, &mut std::io::stdout()))
        }
        Command::Status => Ok(update::status_command(&mut std::io::stdout())),
        Command::Rollback => Ok(update::rollback_command(&mut std::io::stdout())),
    }
}
