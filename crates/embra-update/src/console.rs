//! The restore prompt of a maintenance boot: a numbered list on the serial
//! console, the number asked twice, anything else reboots without
//! restoring. No timeout: an operator is at the console.

use std::io::{BufRead, Write};

use crate::snapshot::Listed;

/// The index chosen and confirmed, or `None`.
pub fn choose(entries: &[Listed], input: &mut impl BufRead, out: &mut impl Write) -> std::io::Result<Option<usize>> {
    writeln!(out, "embraOS — restore a backup")?;
    if entries.is_empty() {
        writeln!(out, "  no backups under the backups directory; nothing to restore")?;
        return Ok(None);
    }
    let restorable: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.complete).map(|(i, _)| i).collect();
    for (n, (i, e)) in entries.iter().enumerate().enumerate() {
        let _ = i;
        let mark = if e.complete { "" } else { "   (incomplete — not restorable)" };
        writeln!(out, "  [{}] {}{}", n + 1, e.line(), mark)?;
    }
    if restorable.is_empty() {
        writeln!(out, "  no complete backup to restore")?;
        return Ok(None);
    }
    write!(out, "Restore which backup? [1-{}, q = reboot without restoring]: ", entries.len())?;
    out.flush()?;
    let Some(first) = read_line(input) else { return Ok(None) };
    let Some(index) = parse_choice(&first, entries.len()) else { return Ok(None) };
    if !entries[index].complete {
        writeln!(out, "backup {} is incomplete and cannot be restored", first)?;
        return Ok(None);
    }
    writeln!(
        out,
        "This replaces STATE and DATA (the knowledge graph, the sessions, the workspace) with backup {} and keeps the other backups.",
        first
    )?;
    write!(out, "Type {} again to confirm: ", first)?;
    out.flush()?;
    let Some(second) = read_line(input) else { return Ok(None) };
    if parse_choice(&second, entries.len()) == Some(index) { Ok(Some(index)) } else { Ok(None) }
}

fn parse_choice(line: &str, count: usize) -> Option<usize> {
    let n: usize = line.trim().parse().ok()?;
    (1..=count).contains(&n).then_some(n - 1)
}

fn read_line(input: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn entries() -> Vec<Listed> {
        vec![
            Listed { name: "2026-11-05_101200_pre-v0.24.0".into(), complete: true, archive_bytes: 298_000_000, record: None },
            Listed { name: "2026-11-01_193000_manual".into(), complete: true, archive_bytes: 240_000_000, record: None },
            Listed { name: "2026-10-30_080000_weekly".into(), complete: false, archive_bytes: 0, record: None },
        ]
    }

    #[test]
    fn the_restore_prompt_takes_a_number_twice_and_anything_else_reboots_without_restoring() {
        let mut out = Vec::new();
        assert_eq!(choose(&entries(), &mut Cursor::new("2\n2\n"), &mut out).unwrap(), Some(1));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[1] 2026-11-05_101200_pre-v0.24.0"));
        assert!(text.contains("[3] 2026-10-30_080000_weekly") && text.contains("not restorable"));
        assert_eq!(choose(&entries(), &mut Cursor::new("2\n1\n"), &mut Vec::new()).unwrap(), None, "a different second answer");
        assert_eq!(choose(&entries(), &mut Cursor::new("q\n"), &mut Vec::new()).unwrap(), None);
        assert_eq!(choose(&entries(), &mut Cursor::new("\n"), &mut Vec::new()).unwrap(), None);
        assert_eq!(choose(&entries(), &mut Cursor::new("9\n9\n"), &mut Vec::new()).unwrap(), None, "out of range");
        assert_eq!(choose(&entries(), &mut Cursor::new("3\n3\n"), &mut Vec::new()).unwrap(), None, "an incomplete entry");
        assert_eq!(choose(&entries(), &mut Cursor::new(""), &mut Vec::new()).unwrap(), None, "end of input");
        assert_eq!(choose(&[], &mut Cursor::new("1\n1\n"), &mut Vec::new()).unwrap(), None);
    }
}
