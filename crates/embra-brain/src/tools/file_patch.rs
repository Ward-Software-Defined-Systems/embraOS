//! `file_patch` — exact-string batch file editing.
//!
//! Consumer spec: `embraOS-Phase1-Implementation/Sprint 6/file_patch_spec.md`
//! (local, written by the Intelligence as the tool's consumer after the F-19
//! full-rewrite truncation). The five hard requirements (spec §15):
//!
//! 1. Exact-string anchoring — no line ranges, no regex, no fuzzy matching.
//! 2. Validate ALL edits, then write once — or write nothing.
//! 3. Every edit matches against the ORIGINAL buffer (never a prior edit's
//!    output — a sequential chain can manufacture anchors that exist in no
//!    version of the file the caller ever read; pinned by `t07_`).
//! 4. Temp file + fsync + atomic rename in the target's directory.
//! 5. No normalization of any kind (trailing whitespace, BOM, CRLF, Unicode
//!    folds, indentation — all preserved byte-for-byte).
//!
//! Deviations from the spec, agreed at plan review: the §8.1 "NFC/NFD"
//! near-match relaxation is a typographic-confusables fold instead (no
//! Unicode normalization form equates U+2019 with U+0027 — the spec's own
//! T15 is unimplementable as written); a 64 MiB target backstop bounds the
//! otherwise-unbounded read-into-RAM; and zero-match diagnostics also probe
//! the opposite escape mode ("would match with raw: true").
//!
//! Matching operates on BYTES: non-UTF-8 files are legal (spec §10), line
//! numbers are 1-based LF counts. Errors are prose in the success channel,
//! matching the ten-tool file_* family convention.

use std::path::Path;

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use super::engineering::{expand_escapes, resolve_workspace_path, WORKSPACE_ROOT};
use super::sessions::truncate_str;
use crate::tools::registry::DispatchContext;

/// Backstop against reading a pathological target into RAM twice on a 4 GiB
/// guest. Deliberately far above every realistic file (spec §11 wants edit
/// cost decoupled from file size, and it stays decoupled below this): T20's
/// 10 MB case passes with two orders of magnitude of headroom.
const FILE_PATCH_MAX_TARGET: u64 = 64 * 1024 * 1024;

/// Ambiguous-match enumeration cap (spec §8.3) and per-edit span-list cap in
/// the success report.
const ENUM_CAP: usize = 10;

/// Max bytes of a context line rendered in diagnostics.
const CONTEXT_LINE_MAX: usize = 100;

/// Bytes of divergence context shown on each side in the §8.2 report.
const DIVERGE_CONTEXT: usize = 24;

// ---------------------------------------------------------------------------
// Pure planning core
// ---------------------------------------------------------------------------

/// One normalized edit: needles are post-escape-handling bytes.
#[derive(Debug, Clone)]
pub(crate) struct Edit {
    pub(crate) old: Vec<u8>,
    pub(crate) new: Vec<u8>,
    /// The needle the OPPOSITE escape mode would have produced, when it
    /// differs, paired with the `raw` value to suggest. Diagnostic-only.
    pub(crate) alt_old: Option<(Vec<u8>, bool)>,
    pub(crate) replace_all: bool,
    pub(crate) expect_count: Option<u64>,
}

impl Edit {
    #[cfg(test)]
    fn simple(old: &str, new: &str) -> Self {
        Edit {
            old: old.as_bytes().to_vec(),
            new: new.as_bytes().to_vec(),
            alt_old: None,
            replace_all: false,
            expect_count: None,
        }
    }
}

/// Per-edit outcome for the success report.
pub(crate) struct EditReport {
    /// Match-span start offsets in the ORIGINAL buffer.
    spans: Vec<usize>,
    old_len: usize,
    new_len: usize,
}

pub(crate) struct PatchPlan {
    pub(crate) output: Vec<u8>,
    reports: Vec<EditReport>,
    total_replacements: usize,
}

/// Find all non-overlapping occurrences, left to right. Byte-exact.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    let mut i = 0usize;
    while i <= last_start {
        match haystack[i..=last_start].iter().position(|&b| b == first) {
            None => break,
            Some(off) => {
                let s = i + off;
                if &haystack[s..s + needle.len()] == needle {
                    out.push(s);
                    i = s + needle.len();
                } else {
                    i = s + 1;
                }
            }
        }
    }
    out
}

/// 1-based line number of a byte offset (LF count; CR is not a line break).
fn line_of(buffer: &[u8], pos: usize) -> usize {
    1 + buffer[..pos.min(buffer.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// The full line containing `pos`, lossily rendered and display-truncated.
fn context_line(buffer: &[u8], pos: usize) -> String {
    let pos = pos.min(buffer.len());
    let start = buffer[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = buffer[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| pos + i)
        .unwrap_or(buffer.len());
    let line = String::from_utf8_lossy(&buffer[start..end]);
    let t = truncate_str(&line, CONTEXT_LINE_MAX);
    if t.len() < line.len() {
        format!("{t}…")
    } else {
        t.to_string()
    }
}

/// Lossy one-line rendering of raw bytes for diagnostics.
fn display_snip(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

/// Validate every edit against the original buffer and produce the patched
/// output — or a diagnostic error with the buffer untouched (spec §3, §4).
pub(crate) fn plan_patch(buffer: &[u8], edits: &[Edit]) -> Result<PatchPlan, String> {
    if edits.is_empty() {
        return Err("edits is empty — supply at least one edit".to_string());
    }

    // Phase 1: match every edit against the ORIGINAL buffer (spec §4.1).
    let mut kept: Vec<Vec<usize>> = Vec::with_capacity(edits.len());
    for (idx, edit) in edits.iter().enumerate() {
        if edit.old.is_empty() {
            return Err(format!(
                "edits[{idx}].old_string is empty — empty match anchors are meaningless"
            ));
        }
        let spans = find_all(buffer, &edit.old);
        let count = spans.len();

        if count == 0 {
            return Err(zero_match_report(buffer, edit, idx));
        }
        if let Some(expected) = edit.expect_count.filter(|&e| count as u64 != e) {
            return Err(format!(
                "edits[{idx}] expect_count is {expected} but old_string matches {count} time(s)"
            ));
        }
        if count > 1 && !edit.replace_all {
            return Err(ambiguous_report(buffer, &spans, idx, count));
        }
        kept.push(spans);
    }

    // Phase 2: cross-edit overlap check (spec §4.2). Adjacency is legal.
    let mut all_spans: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, edit_idx)
    for (idx, spans) in kept.iter().enumerate() {
        let len = edits[idx].old.len();
        for &s in spans {
            all_spans.push((s, s + len, idx));
        }
    }
    all_spans.sort_unstable();
    for pair in all_spans.windows(2) {
        let (s1, e1, i1) = pair[0];
        let (s2, e2, i2) = pair[1];
        if e1 > s2 {
            return Err(format!(
                "edits[{i1}] and edits[{i2}] have overlapping matches\n  edits[{i1}]: bytes {s1}..{e1} (line {})\n  edits[{i2}]: bytes {s2}..{e2} (line {})",
                line_of(buffer, s1),
                line_of(buffer, s2),
            ));
        }
    }

    // Phase 3: apply all replacements against the original spans (spec §4 step 5).
    let mut output = Vec::with_capacity(buffer.len());
    let mut cursor = 0usize;
    for &(start, end, idx) in &all_spans {
        output.extend_from_slice(&buffer[cursor..start]);
        output.extend_from_slice(&edits[idx].new);
        cursor = end;
    }
    output.extend_from_slice(&buffer[cursor..]);

    let total_replacements = all_spans.len();
    let reports = kept
        .iter()
        .enumerate()
        .map(|(idx, spans)| EditReport {
            spans: spans.clone(),
            old_len: edits[idx].old.len(),
            new_len: edits[idx].new.len(),
        })
        .collect();

    Ok(PatchPlan {
        output,
        reports,
        total_replacements,
    })
}

// ---------------------------------------------------------------------------
// Zero-match diagnostics (spec §8.1 / §8.2, report-only — never auto-applied)
// ---------------------------------------------------------------------------

/// Strip trailing spaces/tabs from every line (before each LF and at EOF).
/// Preserves LF count, so line numbers computed on the transformed text are
/// valid for the original.
fn strip_trailing_ws_per_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t']));
    }
    out
}

/// Typographic-confusables fold. Substitutes for the spec's "NFC/NFD fold"
/// relaxation, which cannot do what its own acceptance test (T15) expects:
/// no Unicode normalization form equates U+2019 with U+0027. This fold
/// catches the mismatches that actually occur — smart quotes, NBSP,
/// typographic dashes, ellipsis. Never touches LF, so line numbers hold.
fn fold_confusables(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{00A0}' => out.push(' '),
            '\u{2013}' | '\u{2014}' | '\u{2212}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            other => out.push(other),
        }
    }
    out
}

fn line_of_str(s: &str, pos: usize) -> usize {
    1 + s.as_bytes()[..pos.min(s.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

fn zero_match_report(buffer: &[u8], edit: &Edit, idx: usize) -> String {
    let mut lines = vec![format!("no exact match for edits[{idx}].old_string")];

    let hay = String::from_utf8_lossy(buffer);
    let needle = String::from_utf8_lossy(&edit.old);

    // Each relaxation probes independently (spec §8.1).
    let probes: [(&str, String, String); 4] = [
        (
            "trailing-whitespace",
            strip_trailing_ws_per_line(&hay),
            strip_trailing_ws_per_line(&needle),
        ),
        (
            "whole-string-trim",
            hay.to_string(),
            needle.trim().to_string(),
        ),
        (
            "crlf-to-lf",
            hay.replace("\r\n", "\n"),
            needle.replace("\r\n", "\n"),
        ),
        (
            "typographic-confusables",
            fold_confusables(&hay),
            fold_confusables(&needle),
        ),
    ];

    let mut near_hit = false;
    for (name, h, n) in &probes {
        if n.is_empty() {
            continue;
        }
        let hits = find_all(h.as_bytes(), n.as_bytes());
        if let Some(&pos) = hits.first() {
            near_hit = true;
            lines.push(format!(
                "  near-match at line {} under relaxation: {name}",
                line_of_str(h, pos)
            ));
        }
    }

    // Opposite-escape-mode probe (plan-review addition, same report-only rule).
    if let Some((alt, suggest_raw)) = &edit.alt_old {
        let hits = find_all(buffer, alt);
        if !hits.is_empty() {
            near_hit = true;
            lines.push(format!(
                "  would match with raw: {suggest_raw} ({} occurrence(s), first at line {})",
                hits.len(),
                line_of(buffer, hits[0])
            ));
        }
    }

    if near_hit {
        lines.push("  (not applied — extend or correct old_string and retry)".to_string());
    } else {
        lines.push(prefix_divergence(buffer, &edit.old));
    }

    lines.join("\n")
}

/// Longest prefix of the needle that occurs anywhere in the buffer, and where
/// the needle diverges from the file at that point (spec §8.2). Binary search
/// is valid because occurrence is monotone in prefix length.
fn prefix_divergence(buffer: &[u8], needle: &[u8]) -> String {
    let occurs = |len: usize| -> Option<usize> { find_all(buffer, &needle[..len]).first().copied() };

    // Invariant: occurs(lo) known-Some (lo=0 treated as trivially occurring),
    // occurs(hi) known-None (hi = full needle: caller established 0 matches).
    let (mut lo, mut hi) = (0usize, needle.len());
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if occurs(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    if lo == 0 {
        return "  no near-match under any relaxation\n  no prefix of old_string occurs in the file"
            .to_string();
    }

    let pos = occurs(lo).expect("binary-search invariant: occurs(lo)");
    let ctx_start = lo.saturating_sub(12);
    let file_end = (pos + lo + DIVERGE_CONTEXT).min(buffer.len());
    let given_end = (lo + DIVERGE_CONTEXT).min(needle.len());

    format!(
        "  no near-match under any relaxation\n  longest matching prefix: {lo} of {} bytes, at line {}\n  diverges at old_string byte {lo}:\n    file has: \"…{}\"\n    given:    \"…{}\"",
        needle.len(),
        line_of(buffer, pos),
        display_snip(&buffer[pos + ctx_start..file_end]),
        display_snip(&needle[ctx_start..given_end]),
    )
}

fn ambiguous_report(buffer: &[u8], spans: &[usize], idx: usize, count: usize) -> String {
    let mut lines = vec![format!(
        "edits[{idx}].old_string matches {count} times — extend old_string with more context or set replace_all"
    )];
    for &pos in spans.iter().take(ENUM_CAP) {
        lines.push(format!(
            "  line {:>5}: {}",
            line_of(buffer, pos),
            context_line(buffer, pos)
        ));
    }
    if count > ENUM_CAP {
        lines.push(format!("  … {} more", count - ENUM_CAP));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Success report (spec §9)
// ---------------------------------------------------------------------------

fn render_report(path: &str, plan: &PatchPlan, buffer: &[u8], before: usize, dry_run: bool) -> String {
    let after = plan.output.len();
    let mut out = String::new();
    if dry_run {
        out.push_str("DRY RUN — no write\n");
    }
    out.push_str(&format!(
        "Patched {path}: {} edit(s), {} replacement(s)\n",
        plan.reports.len(),
        plan.total_replacements
    ));
    for (idx, rep) in plan.reports.iter().enumerate() {
        let n = rep.spans.len();
        let removed = rep.old_len * n;
        let added = rep.new_len * n;
        if n == 1 {
            out.push_str(&format!(
                "  edits[{idx}]: line {} (-{removed}/+{added} bytes)\n",
                line_of(buffer, rep.spans[0])
            ));
        } else {
            let mut shown: Vec<String> = rep
                .spans
                .iter()
                .take(ENUM_CAP)
                .map(|&p| line_of(buffer, p).to_string())
                .collect();
            if n > ENUM_CAP {
                shown.push("…".to_string());
            }
            out.push_str(&format!(
                "  edits[{idx}]: lines {} ({n} replacements, -{removed}/+{added} bytes)\n",
                shown.join(", ")
            ));
        }
    }
    let delta = after as i64 - before as i64;
    out.push_str(&format!(
        "size: {before} -> {after} bytes ({}{delta}){}",
        if delta >= 0 { "+" } else { "" },
        if dry_run { " (projected)" } else { "" }
    ));
    out
}

// ---------------------------------------------------------------------------
// Atomic writer (spec §5)
// ---------------------------------------------------------------------------

/// Same-directory temp + full write + fsync + metadata restore + atomic
/// rename + directory fsync. Any pre-rename failure removes the temp and
/// leaves the target byte-identical. Rename is last, so the target is never
/// observable half-written — the failure the tool exists to prevent.
pub(crate) async fn apply_atomic(target: &Path, bytes: &[u8]) -> Result<(), String> {
    apply_atomic_inner(target, bytes, false).await
}

async fn apply_atomic_inner(target: &Path, bytes: &[u8], fail_before_rename: bool) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "target has no parent directory".to_string())?;
    let fname = target
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| "target has no file name".to_string())?;
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".{fname}.embra-patch.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let result: Result<(), String> = async {
        let meta = tokio::fs::metadata(target)
            .await
            .map_err(|e| format!("failed to stat target: {e}"))?;

        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| format!("failed to create temp file: {e}"))?;
        f.write_all(bytes)
            .await
            .map_err(|e| format!("failed to write temp file: {e}"))?;
        f.sync_all()
            .await
            .map_err(|e| format!("failed to fsync temp file: {e}"))?;
        drop(f);

        // Restore mode + ownership on the temp BEFORE the rename, so the
        // replacement lands with its metadata already correct (spec §5.6).
        tokio::fs::set_permissions(&tmp, meta.permissions())
            .await
            .map_err(|e| format!("failed to set permissions: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()))
                .map_err(|e| format!("failed to set ownership: {e}"))?;
        }

        if fail_before_rename {
            return Err("injected failure (test)".to_string());
        }

        tokio::fs::rename(&tmp, target)
            .await
            .map_err(|e| format!("failed to rename temp over target: {e}"))?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return result;
    }

    // Directory fsync makes the rename durable. Best-effort by design: the
    // rename has already happened, so reporting failure here would claim
    // "File unchanged." about a file that changed.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument normalization + handler
// ---------------------------------------------------------------------------

/// One edit in the batch form.
///
/// `deny_unknown_fields` (shakedown D-1): a misspelled or misplaced field —
/// `expect_kount`, or a per-edit `raw` (spec §6: raw is per-call, mixed modes
/// are an error) — must refuse loudly, never silently void a guardrail.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PatchEdit {
    /// Exact text to find in the file. Must be non-empty, and must match
    /// exactly once unless replace_all is true.
    pub old_string: String,
    /// Replacement text. An empty string deletes the match.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
    /// Optional assertion: the call fails unless the match count equals this.
    #[serde(default)]
    pub expect_count: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_patch",
    is_side_effectful = true,
    description = "Edit an EXISTING file in place by exact string replacement — the surgical alternative to a full file_write rewrite. The file's contents never enter the conversation, so a small edit to a large file costs only the edit itself (targets up to a 64 MiB backstop). Each edit's old_string must match the file exactly and uniquely (unless replace_all: true; expect_count asserts the match count; new_string empty = deletion). Multiple edits in the edits array form ONE transaction: every edit is validated against the original file, then all apply in a single atomic temp-file-and-rename write — or nothing is written. A single edit may instead be given flat as old_string/new_string/replace_all/expect_count. Unknown or misplaced fields are rejected, never ignored. Never creates files — use file_write for that. Like file_write, \\n and \\t in old_string/new_string are expanded before use; set raw: true (top-level — it applies to every edit) when the file contains literal backslash sequences (e.g. source code with \"\\n\" inside string literals). dry_run: true validates and reports without writing. Failed matches return near-match and divergence diagnostics. path may be absolute (/embra/workspace/...) or workspace-relative; workspace restricted."
)]
#[serde(deny_unknown_fields)]
pub struct FilePatchArgs {
    /// File to edit. Must already exist — file_patch never creates files.
    pub path: String,
    /// The edits, validated together and applied as one all-or-nothing
    /// atomic transaction against the original file.
    #[serde(default)]
    pub edits: Option<Vec<PatchEdit>>,
    /// Single-edit shorthand for edits: [{...}]. Do not combine with edits.
    #[serde(default)]
    pub old_string: Option<String>,
    /// Single-edit shorthand. Empty string deletes the match.
    #[serde(default)]
    pub new_string: Option<String>,
    /// Single-edit shorthand for the per-edit replace_all.
    #[serde(default)]
    pub replace_all: Option<bool>,
    /// Single-edit shorthand for the per-edit expect_count.
    #[serde(default)]
    pub expect_count: Option<u64>,
    /// Validate and report without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Disable \n/\t escape expansion in old_string/new_string (both fields,
    /// every edit) so literal backslash sequences can be matched.
    #[serde(default)]
    pub raw: bool,
}

/// Resolve the flat-vs-batch forms and apply escape handling symmetrically
/// to both fields of every edit (spec §6). The opposite-mode needle is kept
/// for diagnostics when it differs.
///
/// Field-parity rule (shakedown, shared root cause of D-1/D-2/D-3): every
/// per-edit field is available in the flat shorthand, `raw`/`dry_run` are
/// per-call for both forms, and anything unrecognized is a deserialization
/// error — the two forms must never differ in what they can express.
fn effective_edits(args: &FilePatchArgs) -> Result<Vec<Edit>, String> {
    let flat_given = args.old_string.is_some()
        || args.new_string.is_some()
        || args.replace_all.is_some()
        || args.expect_count.is_some();

    let raw_edits: Vec<PatchEdit> = match (&args.edits, flat_given) {
        (Some(_), true) => {
            return Err(
                "supply either the edits array or the flat old_string/new_string form, not both"
                    .to_string(),
            )
        }
        (Some(list), false) => {
            if list.is_empty() {
                return Err("edits is empty — supply at least one edit".to_string());
            }
            list.clone()
        }
        (None, _) => {
            let old = args
                .old_string
                .clone()
                .ok_or_else(|| "old_string is required (or supply the edits array)".to_string())?;
            let new = args
                .new_string
                .clone()
                .ok_or_else(|| "new_string is required (empty string = deletion)".to_string())?;
            vec![PatchEdit {
                old_string: old,
                new_string: new,
                replace_all: args.replace_all.unwrap_or(false),
                expect_count: args.expect_count,
            }]
        }
    };

    let mut out = Vec::with_capacity(raw_edits.len());
    for (idx, e) in raw_edits.iter().enumerate() {
        if e.old_string.is_empty() {
            return Err(format!(
                "edits[{idx}].old_string is empty — empty match anchors are meaningless"
            ));
        }
        let expanded_old = expand_escapes(&e.old_string);
        let expanded_new = expand_escapes(&e.new_string);
        let (old, new, alt_old) = if args.raw {
            let alt = (expanded_old != e.old_string)
                .then(|| (expanded_old.into_bytes(), false));
            (e.old_string.clone().into_bytes(), e.new_string.clone().into_bytes(), alt)
        } else {
            let alt = (expanded_old != e.old_string)
                .then(|| (e.old_string.clone().into_bytes(), true));
            (expanded_old.into_bytes(), expanded_new.into_bytes(), alt)
        };
        out.push(Edit {
            old,
            new,
            alt_old,
            replace_all: e.replace_all,
            expect_count: e.expect_count,
        });
    }
    Ok(out)
}

fn not_found_msg(path: &str) -> String {
    format!(
        "Error: file not found: {path} — file_patch never creates files; use file_write to create. No file created."
    )
}

/// Post-jail core: everything after path resolution. Split out so tests can
/// exercise the full pipeline against temp files (positive-path tests cannot
/// touch /embra/workspace on dev hosts).
pub(crate) async fn patch_at(target: &Path, edits: Vec<Edit>, dry_run: bool) -> String {
    let display = target.display().to_string();

    let meta = match tokio::fs::metadata(target).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return not_found_msg(&display),
        Err(e) => return format!("Error: failed to stat {display}: {e}\nFile unchanged."),
    };
    if meta.is_dir() {
        return format!("Error: {display} is a directory\nFile unchanged.");
    }
    if meta.len() > FILE_PATCH_MAX_TARGET {
        return format!(
            "Error: {display} is {} bytes — over the {} MiB file_patch backstop\nFile unchanged.",
            meta.len(),
            FILE_PATCH_MAX_TARGET / (1024 * 1024)
        );
    }

    let buffer = match tokio::fs::read(target).await {
        Ok(b) => b,
        Err(e) => return format!("Error: failed to read {display}: {e}\nFile unchanged."),
    };

    let plan = match plan_patch(&buffer, &edits) {
        Ok(p) => p,
        Err(msg) => return format!("Error: {msg}\nFile unchanged."),
    };

    if dry_run {
        return render_report(&display, &plan, &buffer, buffer.len(), true);
    }

    if let Err(e) = apply_atomic(target, &plan.output).await {
        return format!("Error: atomic write failed: {e}\nFile unchanged.");
    }

    render_report(&display, &plan, &buffer, buffer.len(), false)
}

impl FilePatchArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_patch_impl(self).await)
    }
}

async fn file_patch_impl(args: FilePatchArgs) -> String {
    // Writer-family jail (resolve_workspace_path), NOT file_read's permissive
    // resolution — a read-modify-write tool with unrestricted paths would be
    // an arbitrary-file-write escape hatch.
    let resolved = match resolve_workspace_path(&args.path) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // §5 symlink rule: resolve, then re-verify the REAL path is still inside
    // the workspace, and operate on the resolved target so a symlink is never
    // silently replaced by a regular file. Component-wise starts_with, so
    // /embra/workspace-evil does not pass. Deliberately stronger than the
    // family's string-prefix jail.
    let canonical = match tokio::fs::canonicalize(&resolved).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return not_found_msg(&resolved),
        Err(e) => return format!("Error: failed to resolve {resolved}: {e}\nFile unchanged."),
    };
    if !canonical.starts_with(WORKSPACE_ROOT) {
        return format!(
            "Denied: path '{resolved}' resolves outside {WORKSPACE_ROOT} (symlink target {})",
            canonical.display()
        );
    }

    let edits = match effective_edits(&args) {
        Ok(e) => e,
        Err(msg) => return format!("Error: {msg}\nFile unchanged."),
    };

    patch_at(&canonical, edits, args.dry_run).await
}

// ---------------------------------------------------------------------------
// Tests — T1..T21 are the acceptance tests from spec §12, numbered to match.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod file_patch_tests {
    use super::*;
    use std::path::PathBuf;

    fn apply(buffer: &[u8], edits: &[Edit]) -> Result<Vec<u8>, String> {
        plan_patch(buffer, edits).map(|p| p.output)
    }

    // -- Pure core: matching semantics ------------------------------------

    #[test]
    fn t01_simple_replace() {
        let plan = plan_patch(b"hello world", &[Edit::simple("world", "there")]).unwrap();
        assert_eq!(plan.output, b"hello there");
        assert_eq!(plan.total_replacements, 1);
        assert_eq!(line_of(b"hello world", plan.reports[0].spans[0]), 1);
    }

    #[test]
    fn t02_zero_matches_is_error() {
        let err = apply(b"hello world", &[Edit::simple("mars", "x")]).unwrap_err();
        assert!(err.contains("no exact match for edits[0].old_string"), "{err}");
    }

    #[test]
    fn t03_ambiguous_match_enumerated() {
        let err = apply(b"a a a", &[Edit::simple("a", "b")]).unwrap_err();
        assert!(err.contains("matches 3 times"), "{err}");
        assert!(err.contains("replace_all"), "{err}");
        assert_eq!(err.matches("line").count(), 3, "{err}");
    }

    #[test]
    fn t04_replace_all() {
        let mut e = Edit::simple("a", "b");
        e.replace_all = true;
        let plan = plan_patch(b"a a a", &[e]).unwrap();
        assert_eq!(plan.output, b"b b b");
        assert_eq!(plan.total_replacements, 3);
    }

    #[test]
    fn t05_expect_count_mismatch() {
        let mut e = Edit::simple("a", "b");
        e.replace_all = true;
        e.expect_count = Some(2);
        let err = apply(b"a a a", &[e]).unwrap_err();
        assert!(err.contains("expect_count is 2 but old_string matches 3"), "{err}");
    }

    #[test]
    fn t06_overlapping_spans_rejected() {
        let err = apply(b"ABC", &[Edit::simple("AB", "X"), Edit::simple("BC", "Y")]).unwrap_err();
        assert!(err.contains("overlapping matches"), "{err}");
    }

    #[test]
    fn t07_edits_match_original_buffer_never_each_other() {
        // A->X would produce "XB"; edits[1] anchors on "XB", which exists in
        // no version of the file the caller ever read. Must be a 0-match
        // error on the ORIGINAL buffer — never "Y".
        let err = apply(b"AB", &[Edit::simple("A", "X"), Edit::simple("XB", "Y")]).unwrap_err();
        assert!(err.contains("no exact match for edits[1].old_string"), "{err}");
    }

    #[test]
    fn t08_batch_applies_in_one_pass() {
        let plan = plan_patch(
            b"one\ntwo\nthree",
            &[Edit::simple("one", "1"), Edit::simple("three", "3")],
        )
        .unwrap();
        assert_eq!(plan.output, b"1\ntwo\n3");
        assert_eq!(plan.total_replacements, 2);
    }

    #[test]
    fn t09_empty_new_string_is_deletion() {
        let plan = plan_patch(b"keep me", &[Edit::simple("keep ", "")]).unwrap();
        assert_eq!(plan.output, b"me");
    }

    #[test]
    fn t10_empty_old_string_is_error() {
        let err = apply(b"anything", &[Edit::simple("", "x")]).unwrap_err();
        assert!(err.contains("old_string is empty"), "{err}");
    }

    #[test]
    fn t13_trailing_space_preserved() {
        let plan = plan_patch(b"line \n", &[Edit::simple("line", "LINE")]).unwrap();
        assert_eq!(plan.output, b"LINE \n");
    }

    #[test]
    fn t14_exact_codepoint_match() {
        let plan = plan_patch("don’t".as_bytes(), &[Edit::simple("don’t", "do not")]).unwrap();
        assert_eq!(plan.output, b"do not");
    }

    #[test]
    fn t15_confusables_near_match_reported() {
        let err = apply("don’t".as_bytes(), &[Edit::simple("don't", "x")]).unwrap_err();
        assert!(err.contains("typographic-confusables"), "{err}");
        assert!(err.contains("not applied"), "{err}");
    }

    #[test]
    fn t16_untouched_regions_byte_identical() {
        let content = "┌─┐\n│x│\n└─┘\n".as_bytes().to_vec();
        let plan = plan_patch(&content, &[Edit::simple("x", "y")]).unwrap();
        let expected = "┌─┐\n│y│\n└─┘\n".as_bytes();
        assert_eq!(plan.output, expected);
    }

    // -- Pure core: report + diagnostics details ---------------------------

    #[test]
    fn adjacency_is_legal_not_overlap() {
        let plan = plan_patch(b"ABCD", &[Edit::simple("AB", "X"), Edit::simple("CD", "Y")]).unwrap();
        assert_eq!(plan.output, b"XY");
    }

    #[test]
    fn identical_needles_in_two_edits_collide_as_overlap() {
        let err = apply(b"only one", &[Edit::simple("one", "1"), Edit::simple("one", "2")]).unwrap_err();
        assert!(err.contains("overlapping matches"), "{err}");
    }

    #[test]
    fn prefix_divergence_reported_when_no_near_match() {
        let file = b"the frame is unelemented and stays so";
        let err = apply(file, &[Edit::simple("the frame is aelemented", "x")]).unwrap_err();
        assert!(err.contains("longest matching prefix: 13 of 23 bytes"), "{err}");
        assert!(err.contains("diverges at old_string byte 13"), "{err}");
        assert!(err.contains("file has:"), "{err}");
        assert!(err.contains("given:"), "{err}");
    }

    #[test]
    fn no_prefix_at_all_reported() {
        let err = apply(b"abc", &[Edit::simple("zzz", "x")]).unwrap_err();
        assert!(err.contains("no prefix of old_string occurs"), "{err}");
    }

    #[test]
    fn ambiguity_enumeration_caps_at_ten() {
        let hay = "x ".repeat(14);
        let err = apply(hay.as_bytes(), &[Edit::simple("x", "y")]).unwrap_err();
        assert!(err.contains("matches 14 times"), "{err}");
        assert_eq!(err.matches("line").count(), 10, "{err}");
        assert!(err.contains("… 4 more"), "{err}");
    }

    #[test]
    fn replace_all_span_lines_cap_at_ten_in_report() {
        let hay = "q\n".repeat(12);
        let mut e = Edit::simple("q", "r");
        e.replace_all = true;
        let plan = plan_patch(hay.as_bytes(), &[e]).unwrap();
        let report = render_report("f", &plan, hay.as_bytes(), hay.len(), false);
        assert!(report.contains("12 replacements"), "{report}");
        assert!(report.contains("…"), "{report}");
    }

    #[test]
    fn report_shape_matches_spec() {
        let buffer = b"hello world\n".to_vec();
        let plan = plan_patch(&buffer, &[Edit::simple("world", "there")]).unwrap();
        let report = render_report("/w/f.md", &plan, &buffer, buffer.len(), false);
        assert!(report.starts_with("Patched /w/f.md: 1 edit(s), 1 replacement(s)"), "{report}");
        assert!(report.contains("edits[0]: line 1 (-5/+5 bytes)"), "{report}");
        assert!(report.contains("size: 12 -> 12 bytes (+0)"), "{report}");
    }

    #[test]
    fn line_numbers_count_lf_only() {
        let buffer = b"a\r\nb\nc target";
        let plan = plan_patch(buffer, &[Edit::simple("target", "t")]).unwrap();
        assert_eq!(line_of(buffer, plan.reports[0].spans[0]), 3);
    }

    #[test]
    fn non_utf8_buffer_patches_on_bytes() {
        let mut buffer = vec![0xFF, 0xFE, b'\n'];
        buffer.extend_from_slice(b"needle here");
        let plan = plan_patch(&buffer, &[Edit::simple("needle", "thread")]).unwrap();
        let mut expected = vec![0xFF, 0xFE, b'\n'];
        expected.extend_from_slice(b"thread here");
        assert_eq!(plan.output, expected);
        assert_eq!(line_of(&buffer, plan.reports[0].spans[0]), 2);
    }

    // -- Escape handling (T17/T18) via effective_edits ---------------------

    fn args_flat(old: &str, new: &str, raw: bool) -> FilePatchArgs {
        FilePatchArgs {
            path: "unused".into(),
            edits: None,
            old_string: Some(old.into()),
            new_string: Some(new.into()),
            replace_all: None,
            expect_count: None,
            dry_run: false,
            raw,
        }
    }

    #[test]
    fn t17_raw_mode_matches_literal_backslash_n() {
        let edits = effective_edits(&args_flat("\\n", "\\t", true)).unwrap();
        assert_eq!(edits[0].old, b"\\n");
        assert_eq!(edits[0].new, b"\\t");
        let plan = plan_patch(b"x\\ny", &edits).unwrap();
        assert_eq!(plan.output, b"x\\ty");
    }

    #[test]
    fn t18_default_mode_expands_to_real_newline() {
        let edits = effective_edits(&args_flat("x\\ny", "z", false)).unwrap();
        assert_eq!(edits[0].old, b"x\ny");
        let plan = plan_patch(b"x\ny", &edits).unwrap();
        assert_eq!(plan.output, b"z");
    }

    #[test]
    fn opposite_escape_mode_probe_suggests_raw() {
        // File holds a literal backslash-n; default mode expanded the needle
        // to a real newline, so 0 matches — the probe should point at raw: true.
        let edits = effective_edits(&args_flat("x\\ny", "z", false)).unwrap();
        let err = apply(b"x\\ny", &edits).unwrap_err();
        assert!(err.contains("would match with raw: true"), "{err}");
    }

    #[test]
    fn opposite_escape_mode_probe_suggests_expansion() {
        // raw: true kept the two-char needle, but the file has a real newline.
        let edits = effective_edits(&args_flat("x\\ny", "z", true)).unwrap();
        let err = apply(b"x\ny", &edits).unwrap_err();
        assert!(err.contains("would match with raw: false"), "{err}");
    }

    // -- Argument normalization --------------------------------------------

    #[test]
    fn flat_and_edits_together_is_error() {
        let mut args = args_flat("a", "b", false);
        args.edits = Some(vec![PatchEdit {
            old_string: "a".into(),
            new_string: "b".into(),
            replace_all: false,
            expect_count: None,
        }]);
        let err = effective_edits(&args).unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn neither_form_is_error() {
        let args = FilePatchArgs {
            path: "x".into(),
            edits: None,
            old_string: None,
            new_string: None,
            replace_all: None,
            expect_count: None,
            dry_run: false,
            raw: false,
        };
        let err = effective_edits(&args).unwrap_err();
        assert!(err.contains("old_string is required"), "{err}");
    }

    #[test]
    fn empty_edits_array_is_error() {
        let args = FilePatchArgs {
            path: "x".into(),
            edits: Some(vec![]),
            old_string: None,
            new_string: None,
            replace_all: None,
            expect_count: None,
            dry_run: false,
            raw: false,
        };
        let err = effective_edits(&args).unwrap_err();
        assert!(err.contains("edits is empty"), "{err}");
    }

    // -- I/O layer: temp files (positive paths can't touch /embra/workspace) --

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-file-patch-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn t11_missing_file_never_created() {
        let dir = TempDir::new("t11");
        let target = dir.0.join("nope.md");
        let out = patch_at(&target, vec![Edit::simple("a", "b")], false).await;
        assert!(out.contains("file not found"), "{out}");
        assert!(out.contains("No file created"), "{out}");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn t12_dry_run_writes_nothing() {
        let dir = TempDir::new("t12");
        let target = dir.0.join("f.md");
        std::fs::write(&target, "hello world").unwrap();
        let mtime_before = std::fs::metadata(&target).unwrap().modified().unwrap();

        let out = patch_at(&target, vec![Edit::simple("world", "there")], true).await;
        assert!(out.starts_with("DRY RUN — no write"), "{out}");
        assert!(out.contains("(projected)"), "{out}");
        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
        assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), mtime_before);
    }

    #[tokio::test]
    async fn t19_symlink_target_patched_link_preserved() {
        let dir = TempDir::new("t19");
        let real = dir.0.join("real.md");
        let link = dir.0.join("link.md");
        std::fs::write(&real, "hello world").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The handler canonicalizes before patching; emulate that seam.
        let canonical = tokio::fs::canonicalize(&link).await.unwrap();
        assert_eq!(canonical, tokio::fs::canonicalize(&real).await.unwrap());

        let out = patch_at(&canonical, vec![Edit::simple("world", "there")], false).await;
        assert!(out.starts_with("Patched"), "{out}");
        assert_eq!(std::fs::read(&real).unwrap(), b"hello there");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    }

    #[tokio::test]
    async fn t20_ten_megabyte_file_no_ceiling() {
        let dir = TempDir::new("t20");
        let target = dir.0.join("big.dat");
        let mut content = vec![b'a'; 10 * 1024 * 1024];
        let marker = b"THE-ONE-MARKER";
        let pos = 7 * 1024 * 1024;
        content[pos..pos + marker.len()].copy_from_slice(marker);
        std::fs::write(&target, &content).unwrap();

        let out = patch_at(&target, vec![Edit::simple("THE-ONE-MARKER", "REPLACED-OK!!!")], false).await;
        assert!(out.starts_with("Patched"), "{out}");
        let after = std::fs::read(&target).unwrap();
        assert_eq!(after.len(), content.len());
        assert_eq!(&after[pos..pos + marker.len()], b"REPLACED-OK!!!");
    }

    #[tokio::test]
    async fn t21_failed_write_leaves_target_and_no_temp() {
        let dir = TempDir::new("t21");
        let target = dir.0.join("f.md");
        std::fs::write(&target, "precious bytes").unwrap();

        let err = apply_atomic_inner(&target, b"new bytes", true).await.unwrap_err();
        assert!(err.contains("injected failure"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"precious bytes");
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("embra-patch"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[tokio::test]
    async fn atomic_write_preserves_mode_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("mode");
        let target = dir.0.join("f.md");
        std::fs::write(&target, "hello world").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        let out = patch_at(&target, vec![Edit::simple("world", "there")], false).await;
        assert!(out.starts_with("Patched"), "{out}");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn directory_target_is_error() {
        let dir = TempDir::new("isdir");
        let out = patch_at(&dir.0, vec![Edit::simple("a", "b")], false).await;
        assert!(out.contains("is a directory"), "{out}");
    }

    #[tokio::test]
    async fn failed_validation_leaves_file_byte_identical() {
        let dir = TempDir::new("valfail");
        let target = dir.0.join("f.md");
        std::fs::write(&target, "one two three").unwrap();
        let out = patch_at(
            &target,
            vec![Edit::simple("one", "1"), Edit::simple("missing", "x")],
            false,
        )
        .await;
        assert!(out.contains("no exact match for edits[1]"), "{out}");
        assert!(out.contains("File unchanged."), "{out}");
        assert_eq!(std::fs::read(&target).unwrap(), b"one two three");
    }

    // -- Jail (mirrors file_family_rejects_traversal_uniformly) ------------

    #[tokio::test]
    async fn jail_rejects_traversal_and_outside_paths() {
        let ctx_free = |args: FilePatchArgs| async move { file_patch_impl(args).await };

        let mut a = args_flat("a", "b", false);
        a.path = "../etc/passwd".into();
        let msg = ctx_free(a).await;
        assert!(msg.contains("'..'"), "expected uniform traversal rejection, got: {msg}");

        let mut b = args_flat("a", "b", false);
        b.path = "/etc/passwd".into();
        let msg = ctx_free(b).await;
        assert!(msg.starts_with("Denied:"), "expected outside-workspace denial, got: {msg}");
    }

    // -- Registration (the system_logs pattern) -----------------------------

    #[test]
    fn file_patch_registered_with_plain_object_schema() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"file_patch"), "file_patch registered");

        // Anthropic rejects top-level oneOf/allOf/anyOf in input_schema.
        let schema = schemars::schema_for!(FilePatchArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
        assert!(v.get("properties").is_some());
    }

    // -- Shakedown v1 regressions (D-1 / D-2 / D-3) -------------------------

    #[test]
    fn d1_unknown_top_level_field_rejected() {
        // A misspelled guardrail must refuse loudly, never silently void it.
        let err = serde_json::from_value::<FilePatchArgs>(serde_json::json!({
            "path": "f", "old_string": "a", "new_string": "b",
            "replace_all": true, "expect_kount": 3
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("expect_kount"), "{err}");
    }

    #[test]
    fn d1_unknown_per_edit_field_rejected() {
        // raw is per-call (spec §6) — inside an edit it must error, not vanish.
        let err = serde_json::from_value::<FilePatchArgs>(serde_json::json!({
            "path": "f",
            "edits": [{"old_string": "a", "new_string": "b", "raw": true}]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("raw"), "{err}");
    }

    #[test]
    fn d2_flat_expect_count_honored() {
        let mut args = args_flat("a", "b", false);
        args.replace_all = Some(true);
        args.expect_count = Some(3);
        let edits = effective_edits(&args).unwrap();
        let err = apply(b"a a", &edits).unwrap_err();
        assert!(err.contains("expect_count is 3 but old_string matches 2"), "{err}");
    }

    #[test]
    fn d2_flat_expect_count_counts_as_flat_form() {
        let args = FilePatchArgs {
            path: "f".into(),
            edits: Some(vec![PatchEdit {
                old_string: "a".into(),
                new_string: "b".into(),
                replace_all: false,
                expect_count: None,
            }]),
            old_string: None,
            new_string: None,
            replace_all: None,
            expect_count: Some(1),
            dry_run: false,
            raw: false,
        };
        let err = effective_edits(&args).unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[tokio::test]
    async fn d3_batch_and_flat_raw_parity_end_to_end() {
        // Same decoded input through both forms with raw: true must produce
        // byte-identical files — pins that top-level raw reaches every edit
        // in the batch form (the shakedown's D-3 concern).
        let dir = TempDir::new("d3parity");
        let fa = dir.0.join("a.txt");
        let fb = dir.0.join("b.txt");
        std::fs::write(&fa, "alpha omega tail").unwrap();
        std::fs::write(&fb, "alpha omega tail").unwrap();

        let batch: FilePatchArgs = serde_json::from_value(serde_json::json!({
            "path": "x", "raw": true,
            "edits": [{"old_string": "omega", "new_string": "omega\\nEND"}]
        }))
        .unwrap();
        let flat: FilePatchArgs = serde_json::from_value(serde_json::json!({
            "path": "x", "raw": true,
            "old_string": "omega", "new_string": "omega\\nEND"
        }))
        .unwrap();

        let out_a = patch_at(&fa, effective_edits(&batch).unwrap(), false).await;
        let out_b = patch_at(&fb, effective_edits(&flat).unwrap(), false).await;
        assert!(out_a.starts_with("Patched"), "{out_a}");
        assert!(out_b.starts_with("Patched"), "{out_b}");

        let a = std::fs::read(&fa).unwrap();
        let b = std::fs::read(&fb).unwrap();
        assert_eq!(a, b, "batch and flat diverged under raw: true");
        // raw preserved the literal backslash-n (two chars), not a real LF
        assert_eq!(a, b"alpha omega\\nEND tail".to_vec());
        assert!(!a.contains(&b'\n'));
    }
}

