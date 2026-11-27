//! The update: the signed manifest checked against this release, the slot
//! image streamed into the inactive partition and verified, the slot
//! staged in grubenv for GRUB to try; the status; the rollback.

use std::io::Write;
use std::path::Path;

use embra_boot::{BootState, BootStatus, Release, Slot, SlotStatus, esp, partitions, slot};

use crate::fetch;
use crate::manifest::{self, Manifest, Verdict};
use crate::pubkey;
use crate::slot as slot_io;
use crate::source::{self, Source};
use crate::Outcome;

fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// What `check` establishes before anything is written.
pub struct Checked {
    pub manifest: Manifest,
    pub verdict: Verdict,
    pub source: Source,
    /// The release this machine runs, as the verdict compared it.
    pub release: Release,
}

pub fn keys() -> Vec<[u8; 32]> {
    pubkey::load_keys(&[Path::new(pubkey::SHIPPED_KEYS), Path::new(pubkey::STATE_KEYS)])
}

fn data_engine() -> Option<String> {
    std::fs::read_to_string("/embra/data/wardsondb/.engine").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Fetch and verify the manifest at the source; print what was found.
pub fn check(source_flag: Option<&str>, tag: Option<&str>, out: &mut impl Write) -> Result<Checked, Outcome> {
    let refused = |msg: String| Outcome::Refused(msg);
    let keys = keys();
    if keys.is_empty() {
        return Err(refused("refused: no release signing key is installed; the image ships one at /usr/share/embra/update-signing.pub, or the operator's goes to /embra/state/update-signing.pub".into()));
    }
    let release = Release::read().map_err(|e| refused(format!("refused: this is not an embraOS slot ({e})")))?;
    let source = source::resolve(source_flag, tag, Path::new(source::STATE_SOURCE_FILE)).map_err(|e| refused(format!("refused: {e}")))?;
    let engine = data_engine();
    let client = fetch::client().map_err(|e| refused(format!("refused: {e}")))?;
    let manifest_url = source.manifest_url(&release.arch, engine.as_deref().unwrap_or(&release.engine));
    let _ = writeln!(out, "source: {}", source.base);
    let bytes = fetch::get_small(&client, &manifest_url, fetch::SMALL_MAX)
        .map_err(|e| refused(format!("update check failed: {e}; no manifest for {}-{} is published at this source", release.arch, engine.as_deref().unwrap_or(&release.engine))))?;
    let sig = fetch::get_small(&client, &source.signature_url(&release.arch, engine.as_deref().unwrap_or(&release.engine)), fetch::SMALL_MAX)
        .map_err(|e| refused(format!("update check failed: {e}")))?;
    let sig = String::from_utf8_lossy(&sig).to_string();
    let manifest = manifest::verify(&bytes, &sig, &keys).map_err(|e| refused(format!("refused: {e}")))?;
    let verdict = manifest::compare(&release, engine.as_deref(), &manifest);
    let _ = writeln!(out, "running: {} ({}) {} {}{}", release.version, release.build_id, release.arch, release.engine, engine.as_ref().filter(|e| **e != release.engine).map(|e| format!(" (DATA holds a {e} database)")).unwrap_or_default());
    let _ = writeln!(
        out,
        "available: {} ({}) schema {} kernel {}, {} MB, built {}; signature verified",
        manifest.version, manifest.build_id, manifest.schema_version, manifest.kernel, mb(manifest.rootfs.size), manifest.built
    );
    if !manifest.notes_url.is_empty() {
        let _ = writeln!(out, "notes: {}", manifest.notes_url);
    }
    Ok(Checked { manifest, verdict, source, release })
}

/// The check as one object, the form the brain reads (`check --json`): the
/// verdict by a word, the release this machine runs, the one the source
/// publishes, and the source without any credentials it carries. It exists
/// only for a manifest whose signature was verified: a check that fails
/// before that prints no object, and its reason is the last stderr line.
pub fn check_json(checked: &Checked) -> serde_json::Value {
    let (verdict, reason) = match &checked.verdict {
        Verdict::Newer => ("newer", None),
        Verdict::SameBuild => ("same_build", None),
        Verdict::SameVersionOtherBuild => ("same_version_other_build", None),
        Verdict::Older => ("older", None),
        Verdict::Mismatch(why) => ("mismatch", Some(why.as_str())),
    };
    serde_json::json!({
        "verdict": verdict,
        "reason": reason,
        "running": {
            "version": checked.release.version,
            "build_id": checked.release.build_id,
            "arch": checked.release.arch,
            "engine": checked.release.engine,
        },
        "available": {
            "version": checked.manifest.version,
            "build_id": checked.manifest.build_id,
            "built": checked.manifest.built,
            "notes_url": checked.manifest.notes_url,
        },
        "source": checked.source.shown(),
    })
}

/// `check` as a command: the verdict and the next action, and its exit code.
/// With `json` the object is all that is printed, under the same exit codes:
/// 0 for a newer release, 3 for nothing to do, 2 for a release that is not
/// this machine's.
pub fn check_command(source_flag: Option<&str>, tag: Option<&str>, json: bool, out: &mut impl Write) -> Outcome {
    if json {
        let checked = match check(source_flag, tag, &mut std::io::sink()) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let _ = writeln!(out, "{}", check_json(&checked));
        return match checked.verdict {
            Verdict::Newer => Outcome::Done,
            Verdict::Mismatch(why) => Outcome::Refused(format!("refused: {why}")),
            Verdict::SameBuild | Verdict::SameVersionOtherBuild | Verdict::Older => Outcome::Nothing(String::new()),
        };
    }
    let checked = match check(source_flag, tag, out) {
        Ok(c) => c,
        Err(o) => return o,
    };
    match checked.verdict {
        Verdict::Newer => {
            let _ = writeln!(out, "verdict: an update is available — embra-update apply (or /update apply) stages it into the inactive slot");
            Outcome::Done
        }
        Verdict::SameBuild => Outcome::Nothing("verdict: this build is running; nothing to do".into()),
        Verdict::SameVersionOtherBuild => Outcome::Nothing(format!("verdict: the same version, another build ({}); apply only if you meant to", checked.manifest.build_id)),
        Verdict::Older => Outcome::Nothing("verdict: the source's release is older than this one; apply takes --allow-downgrade".into()),
        Verdict::Mismatch(why) => Outcome::Refused(format!("refused: {why}")),
    }
}

/// Where the inactive slot is and whether it may be written over.
struct Target {
    booted: Slot,
    other: Slot,
    dev: std::path::PathBuf,
    esp: std::path::PathBuf,
}

fn target(status: &BootStatus) -> Result<Target, String> {
    let booted = status.slot.ok_or("this boot has no slot on its kernel line (the direct-kernel path), so there is no inactive slot to write")?;
    let other = booted.other();
    if status.pending == Some(other) || status.status_of(other) == Some(SlotStatus::Pending) {
        return Err(format!("the slot you would overwrite ({other}) is staged and GRUB may fall back to it; confirm this boot first (every service healthy) or embra-update rollback"));
    }
    if status.trial || status.pending == Some(booted) {
        return Err("this boot is a trial of a staged slot; let it confirm (every service healthy) before staging another release".into());
    }
    let dev = partitions::find_partition_beside_root(other.partition_name()).ok_or_else(|| format!("no partition named {}", other.partition_name()))?;
    let esp = partitions::find_partition_beside_root(slot::ESP_PARTITION).ok_or_else(|| format!("no partition named {}", slot::ESP_PARTITION))?;
    Ok(Target { booted, other, dev, esp })
}

/// `apply`: check, the preconditions, the write, the staging.
pub fn apply_command(
    source_flag: Option<&str>,
    tag: Option<&str>,
    yes: bool,
    console: bool,
    allow_downgrade: bool,
    out: &mut crate::console::Operator,
) -> Outcome {
    let checked = match check(source_flag, tag, out) {
        Ok(c) => c,
        Err(o) => return o,
    };
    match &checked.verdict {
        Verdict::Newer => {}
        Verdict::Older if allow_downgrade => {
            let _ = writeln!(out, "verdict: older than this release; applied on --allow-downgrade");
        }
        Verdict::Older => return Outcome::Refused("refused: the source's release is older than this one (--allow-downgrade overrides)".into()),
        Verdict::SameBuild => return Outcome::Nothing("this build is running; nothing to do".into()),
        Verdict::SameVersionOtherBuild if allow_downgrade => {
            let _ = writeln!(out, "verdict: the same version, another build; applied on --allow-downgrade");
        }
        Verdict::SameVersionOtherBuild => {
            return Outcome::Refused(format!("refused: the same version, another build ({}); --allow-downgrade applies it anyway", checked.manifest.build_id));
        }
        Verdict::Mismatch(why) => return Outcome::Refused(format!("refused: {why}")),
    }
    let Some(status) = BootStatus::read() else {
        return Outcome::Refused("refused: no boot status (/run/embra/boot-status); is this embraOS, booted through GRUB?".into());
    };
    let target = match target(&status) {
        Ok(t) => t,
        Err(e) => return Outcome::Refused(format!("refused: {e}")),
    };
    let slot_bytes = match partitions::size_bytes(&target.dev) {
        Ok(b) => b,
        Err(e) => return Outcome::Refused(format!("refused: the size of {} could not be read: {e}", target.dev.display())),
    };
    if checked.manifest.rootfs.size > slot_bytes {
        return Outcome::Refused(format!(
            "refused: the release's image is {} bytes and slot {} is {slot_bytes}; this disk's slots are too small for it",
            checked.manifest.rootfs.size, target.other
        ));
    }
    if !yes && !console {
        return Outcome::Refused(format!(
            "refused: --yes to write {} ({}) into slot {} ({}); the slot's previous content is gone from the first byte",
            checked.manifest.version, checked.manifest.build_id, target.other, target.dev.display()
        ));
    }
    if console && !yes {
        let answer = out.ask(&format!(
            "Apply {} to slot {} ({})? The slot's previous content is gone from the first byte. [y/N] ",
            checked.manifest.version, target.other, target.dev.display()
        ));
        if !crate::console::confirmed(answer.as_deref()) {
            return Outcome::Refused("apply: not confirmed; nothing was changed".into());
        }
    }
    let client = match fetch::client() {
        Ok(c) => c,
        Err(e) => return Outcome::Refused(format!("refused: {e}")),
    };
    let url = checked.source.rootfs_url(&checked.manifest.rootfs.file);
    let _ = writeln!(out, "download: {} ({} MB) → slot {} ({})", checked.manifest.rootfs.file, mb(checked.manifest.rootfs.size), target.other, target.dev.display());
    let mut body = match fetch::open(&client, &url) {
        Ok(r) => r,
        Err(e) => return Outcome::Refused(format!("download failed before anything was written: {e}")),
    };
    let size = checked.manifest.rootfs.size;
    let step = (size / 10).max(1);
    let mut next_mark = step;
    let total_mb = mb(size);
    let written = slot_io::write_image(&mut body, &target.dev, size, &checked.manifest.rootfs.sha256, |done| {
        if done >= next_mark {
            let _ = writeln!(out, "download: {}/{} MB", mb(done), total_mb);
            let _ = out.flush();
            next_mark += step;
        }
    });
    if let Err(e) = written {
        return Outcome::Failed(format!("{e}; GRUB will not boot slot {} (it is not staged)", target.other));
    }
    let _ = writeln!(out, "verify: sha256 matches the signed manifest (streamed and read back)");
    let other = target.other;
    let staged = esp::with_env_mut(&target.esp, |env| {
        let mut st = BootState::from_env(env);
        st.stage(other);
        st.apply(env).map(|()| st)
    });
    match staged {
        Ok(Ok(st)) => {
            let _ = writeln!(
                out,
                "staged: slot {other} holds {} ({}), pending with {} tries; slot {} stays the default until it confirms. Reboot to apply; GRUB falls back to slot {} if the new release does not come up",
                checked.manifest.version, checked.manifest.build_id, st.tries_left, target.booted, target.booted
            );
            Outcome::Done
        }
        Ok(Err(e)) => Outcome::Failed(format!("the image is in slot {other} and verified, but grubenv could not be written: {e}")),
        Err(e) => Outcome::Failed(format!("the image is in slot {other} and verified, but the ESP could not be written: {e}")),
    }
}

/// `status`: the boot status and what each slot holds.
pub fn status_command(out: &mut impl Write) -> Outcome {
    let Some(status) = BootStatus::read() else {
        return Outcome::Refused("no boot status (/run/embra/boot-status); is this embraOS, booted through GRUB?".into());
    };
    let _ = writeln!(out, "slots: {}", status.summary());
    let _ = writeln!(out, "confirmed: {}", status.confirmed);
    for s in [Slot::A, Slot::B] {
        let line = if Some(s) == status.slot {
            match Release::read() {
                Ok(r) => format!("{} ({}, {}, {})", r.version, r.build_id, r.arch, r.engine),
                Err(e) => format!("unreadable: {e}"),
            }
        } else {
            match partitions::find_partition_beside_root(s.partition_name()) {
                Some(dev) => match slot_io::release_of(s, &dev) {
                    Ok(Some(r)) => format!("{} ({}, {}, {})", r.version, r.build_id, r.arch, r.engine),
                    Ok(None) => "empty or unreadable".to_string(),
                    Err(e) => format!("unreadable: {e}"),
                },
                None => "no partition".to_string(),
            }
        };
        let mark = status.status_of(s).map(|st| st.as_str()).filter(|t| !t.is_empty()).map(|t| format!(" [{t}]")).unwrap_or_default();
        let booted = if Some(s) == status.slot { " (booted)" } else { "" };
        let _ = writeln!(out, "slot {s}: {line}{mark}{booted}");
    }
    let _ = writeln!(out, "boot menu: {}", crate::boot_menu::standing_words(&crate::boot_menu::standing()));
    Outcome::Done
}

/// `rollback`: the other slot becomes the default again; the reboot is separate.
pub fn rollback_command(out: &mut impl Write) -> Outcome {
    let Some(status) = BootStatus::read() else {
        return Outcome::Refused("no boot status (/run/embra/boot-status); is this embraOS, booted through GRUB?".into());
    };
    let Some(booted) = status.slot else {
        return Outcome::Refused("refused: this boot has no slot on its kernel line, so there is no other slot".into());
    };
    let other = booted.other();
    let Some(dev) = partitions::find_partition_beside_root(other.partition_name()) else {
        return Outcome::Refused(format!("refused: no partition named {}", other.partition_name()));
    };
    let release = match slot_io::release_of(other, &dev) {
        Ok(Some(r)) => r,
        Ok(None) => return Outcome::Refused(format!("refused: slot {other} holds no release to roll back to")),
        Err(e) => return Outcome::Refused(format!("refused: slot {other} could not be read: {e}")),
    };
    if status.status_of(other) == Some(SlotStatus::Pending) || status.pending == Some(other) {
        return Outcome::Refused(format!("refused: slot {other} is staged and untried; reboot to try it, or apply again"));
    }
    let Some(esp_dev) = partitions::find_partition_beside_root(slot::ESP_PARTITION) else {
        return Outcome::Refused(format!("refused: no partition named {}", slot::ESP_PARTITION));
    };
    match esp::with_env_mut(&esp_dev, |env| {
        let mut st = BootState::from_env(env);
        st.activate(other);
        st.apply(env)
    }) {
        Ok(Ok(())) => {
            let _ = writeln!(out, "slot {other} ({} {}) is the default again; reboot to boot it", release.version, release.build_id);
            Outcome::Done
        }
        Ok(Err(e)) => Outcome::Failed(format!("grubenv could not be written: {e}")),
        Err(e) => Outcome::Failed(format!("the ESP could not be written: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::test_support::{key_pair, sample, sign_b64};

    fn checked(running: (&str, &str), published: (&str, &str), base: &str) -> Checked {
        let (kp, public) = key_pair();
        let bytes = sample(published.0, published.1, "x86_64", "fjall").into_bytes();
        let manifest = manifest::verify(&bytes, &sign_b64(&kp, &bytes), &[public]).unwrap();
        let release = Release::parse(&format!("VERSION={}\nBUILD_ID={}\nARCH=x86_64\nENGINE=fjall\nBUILT=now\n", running.0, running.1)).unwrap();
        let verdict = manifest::compare(&release, None, &manifest);
        Checked { manifest, verdict, source: Source { base: base.to_string() }, release }
    }

    /// The brain reads this object and nothing else of a check: each verdict
    /// has its word, and both releases are named by version and build.
    #[test]
    fn the_object_names_the_verdict_by_a_word_and_both_releases() {
        let base = "https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download";
        let newer = check_json(&checked(("0.24.0-phase2", "aaa"), ("0.25.0-phase2", "bbb"), base));
        assert_eq!(newer["verdict"], "newer");
        assert_eq!(newer["reason"], serde_json::Value::Null);
        assert_eq!(newer["running"], serde_json::json!({"version": "0.24.0-phase2", "build_id": "aaa", "arch": "x86_64", "engine": "fjall"}));
        assert_eq!(newer["available"]["version"], "0.25.0-phase2");
        assert_eq!(newer["available"]["build_id"], "bbb");
        assert_eq!(newer["available"]["built"], "2026-11-05T10:12:00Z");
        assert_eq!(newer["available"]["notes_url"], "https://example/notes");
        assert_eq!(newer["source"], base);
        for (running, published, word) in [
            (("0.25.0-phase2", "bbb"), ("0.25.0-phase2", "bbb"), "same_build"),
            (("0.25.0-phase2", "ccc"), ("0.25.0-phase2", "bbb"), "same_version_other_build"),
            (("0.26.0-phase2", "ddd"), ("0.25.0-phase2", "bbb"), "older"),
        ] {
            assert_eq!(check_json(&checked(running, published, base))["verdict"], word, "{running:?} against {published:?}");
        }
        // A release for another machine: the word, and why.
        let mut other = checked(("0.24.0-phase2", "aaa"), ("0.25.0-phase2", "bbb"), base);
        other.verdict = manifest::compare(&Release::parse("VERSION=0.24.0-phase2\nBUILD_ID=aaa\nARCH=aarch64\nENGINE=fjall\nBUILT=now\n").unwrap(), None, &other.manifest);
        let object = check_json(&other);
        assert_eq!(object["verdict"], "mismatch");
        assert!(object["reason"].as_str().unwrap().contains("aarch64"), "{object}");
    }

    /// The object goes to the model through the brain's status and its tool:
    /// an operator's own source may carry credentials before its host, and
    /// none of them is in it.
    #[test]
    fn the_object_shows_the_source_without_its_credentials() {
        let object = check_json(&checked(("0.24.0-phase2", "aaa"), ("0.25.0-phase2", "bbb"), "https://deploy:s3cret@mirror.example:8443/embraos/latest"));
        assert_eq!(object["source"], "https://mirror.example:8443/embraos/latest");
        assert!(!object.to_string().contains("s3cret"), "{object}");
    }
}
