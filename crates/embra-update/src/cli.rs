//! The command line: a subcommand and `--flag [value]` pairs. A flag that
//! needs a value and has none is an error naming it, like
//! `embra_common::args::Flags` for the services (this binary is not one).

pub const USAGE: &str = "usage: embra-update <command> [flags]
  snapshot [--label <l>] [--taken-by <who>] [--once] [--force]   a tar.gz of STATE and DATA under /embra/data/backups
  restore [<name>] [--yes] [--console]                           replace STATE and DATA with a backup
  list [--names]                                                 the backups, newest first
  delete <name>                                                  remove one backup
  check [--source <url>] [--tag <v…>]                            the release at the source against this one
  apply [--yes] [--console] [--allow-downgrade] [--source <url>] [--tag <v…>]   write the release into the inactive slot and stage it
  status                                                         the booted, active and pending slots and what each holds
  rollback                                                       make the other slot the default again";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Snapshot { label: String, taken_by: String, once: bool, force: bool },
    Restore { name: Option<String>, yes: bool, console: bool },
    List { names_only: bool },
    Delete { name: String },
    Check { source: Option<String>, tag: Option<String> },
    Apply { source: Option<String>, tag: Option<String>, yes: bool, console: bool, allow_downgrade: bool },
    Status,
    Rollback,
}

/// A label: a word of letters, digits, `_`, `.` and `-`, up to 32 characters.
pub fn valid_label(s: &str) -> bool {
    valid_word(s, 32)
}

/// A backup's name: the same grammar up to 80 characters, so it is one path
/// component with no `/` and never `..`.
pub fn valid_name(s: &str) -> bool {
    valid_word(s, 80)
}

fn valid_word(s: &str, max: usize) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    s.len() <= max && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let (sub, rest) = args.split_first().ok_or_else(|| "a command is needed".to_string())?;
    let mut positionals: Vec<String> = Vec::new();
    let mut label: Option<String> = None;
    let mut taken_by: Option<String> = None;
    let mut once = false;
    let mut force = false;
    let mut yes = false;
    let mut console = false;
    let mut names = false;
    let mut source: Option<String> = None;
    let mut tag: Option<String> = None;
    let mut allow_downgrade = false;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        let mut value = |flag: &str| -> Result<String, String> {
            i += 1;
            rest.get(i).cloned().ok_or_else(|| format!("{flag} needs a value"))
        };
        match a {
            "--label" => label = Some(value("--label")?),
            "--taken-by" => taken_by = Some(value("--taken-by")?),
            "--once" => once = true,
            "--force" => force = true,
            "--yes" => yes = true,
            "--console" => console = true,
            "--names" => names = true,
            "--source" => source = Some(value("--source")?),
            "--tag" => tag = Some(value("--tag")?),
            "--allow-downgrade" => allow_downgrade = true,
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            s => positionals.push(s.to_string()),
        }
        i += 1;
    }
    match sub.as_str() {
        "snapshot" => {
            let label = label.unwrap_or_else(|| "manual".to_string());
            if !valid_label(&label) {
                return Err(format!("not a label: {label:?} (letters, digits, _ . -; 32 at most)"));
            }
            let taken_by = taken_by.unwrap_or_else(|| "manual".to_string());
            if !valid_word(&taken_by, 32) {
                return Err(format!("not a taken-by value: {taken_by:?}"));
            }
            if !positionals.is_empty() {
                return Err(format!("snapshot takes no positional: {}", positionals.join(" ")));
            }
            Ok(Command::Snapshot { label, taken_by, once, force })
        }
        "restore" => {
            if positionals.len() > 1 {
                return Err("restore takes one name".into());
            }
            let name = positionals.pop();
            if let Some(n) = &name
                && !valid_name(n)
            {
                return Err(format!("not a backup name: {n:?}"));
            }
            Ok(Command::Restore { name, yes, console })
        }
        "list" => Ok(Command::List { names_only: names }),
        "delete" => {
            let name = positionals.pop().ok_or_else(|| "delete needs a name".to_string())?;
            if !positionals.is_empty() || !valid_name(&name) {
                return Err(format!("not a backup name: {name:?}"));
            }
            Ok(Command::Delete { name })
        }
        "check" => Ok(Command::Check { source, tag }),
        "apply" => Ok(Command::Apply { source, tag, yes, console, allow_downgrade }),
        "status" => Ok(Command::Status),
        "rollback" => Ok(Command::Rollback),
        other => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, String> {
        parse(&s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn every_subcommand_and_flag_parses_and_a_flag_without_its_value_is_an_error() {
        assert_eq!(
            p("snapshot --label pre-v0.24.0 --taken-by pending-slot --once"),
            Ok(Command::Snapshot { label: "pre-v0.24.0".into(), taken_by: "pending-slot".into(), once: true, force: false })
        );
        assert_eq!(
            p("snapshot"),
            Ok(Command::Snapshot { label: "manual".into(), taken_by: "manual".into(), once: false, force: false })
        );
        assert_eq!(p("restore 2026-11-05_101200_manual --yes"), Ok(Command::Restore { name: Some("2026-11-05_101200_manual".into()), yes: true, console: false }));
        assert_eq!(p("restore --console"), Ok(Command::Restore { name: None, yes: false, console: true }));
        assert_eq!(p("list --names"), Ok(Command::List { names_only: true }));
        assert_eq!(p("delete 2026-11-05_101200_manual"), Ok(Command::Delete { name: "2026-11-05_101200_manual".into() }));
        assert_eq!(p("check --source https://m/x --tag v1.2.3"), Ok(Command::Check { source: Some("https://m/x".into()), tag: Some("v1.2.3".into()) }));
        assert_eq!(p("apply --yes --allow-downgrade"), Ok(Command::Apply { source: None, tag: None, yes: true, console: false, allow_downgrade: true }));
        assert_eq!(p("status"), Ok(Command::Status));
        assert_eq!(p("rollback"), Ok(Command::Rollback));
        assert!(p("check --source").unwrap_err().contains("--source needs a value"));
        assert!(p("snapshot --label").unwrap_err().contains("--label needs a value"));
        assert!(p("snapshot --label ../x").unwrap_err().contains("not a label"));
        assert!(p("restore ../etc").unwrap_err().contains("not a backup name"));
        assert!(p("delete").unwrap_err().contains("needs a name"));
        assert!(p("frobnicate").unwrap_err().contains("unknown command"));
        assert!(p("list --verbose").unwrap_err().contains("unknown flag"));
        assert!(p("").is_err());
    }

    #[test]
    fn a_name_is_one_path_component() {
        assert!(valid_name("2026-11-05_101200_pre-v0.24.0-phase2"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".."));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(81)));
        assert!(valid_label("nightly"));
        assert!(!valid_label(&"x".repeat(33)));
    }
}
