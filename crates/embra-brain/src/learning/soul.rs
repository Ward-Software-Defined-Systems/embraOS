use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::db::WardsonDbClient;

/// Where the seal's hash lives on STATE. `embra-trustd` reads it at every
/// boot, and a stored hash with no soul document halts the boot.
pub(crate) const SOUL_HASH_PATH: &str = "/embra/state/soul.sha256";

pub async fn seal_soul(db: &WardsonDbClient, soul: &serde_json::Value) -> Result<()> {
    if !db.collection_exists("soul.invariant").await? {
        db.create_collection("soul.invariant").await?;
    }

    // The hash embra-trustd computes again at every boot. Both sides have to
    // serialize the value the same way, byte for byte, or the next boot
    // halts: `compute_soul_hash` is the one place on this side, and its
    // bytes are pinned (`canonical_meridian_seal_hash_is_frozen`).
    let hash = compute_soul_hash(soul)?;

    let sealed_doc = serde_json::json!({
        "_id": "soul",
        "soul": soul,
        "sha256": hash,
        "sealed_at": chrono::Utc::now().to_rfc3339(),
        "sealed": true,
    });

    db.write("soul.invariant", &sealed_doc).await?;

    // Write hash to STATE partition for boot-time verification by embra-trustd
    write_soul_hash(Path::new(SOUL_HASH_PATH), &hash).await?;
    tracing::info!("Soul hash written to {}", SOUL_HASH_PATH);

    Ok(())
}

/// The hash file, written atomically (`file_patch::write_atomic_create`: a
/// same-directory temp, fsync, rename), so a crash mid-write leaves the
/// old file or none and never a truncated hash that halts every boot.
pub(crate) async fn write_soul_hash(hash_path: &Path, hash: &str) -> Result<()> {
    if let Some(parent) = hash_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::tools::file_patch::write_atomic_create(hash_path, hash.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write soul hash to STATE: {}", e))
}

const SEAL_UNKNOWN: u8 = 0;
const SEAL_ABSENT: u8 = 1;
const SEAL_PRESENT: u8 = 2;

/// The seal as last read, for [`is_soul_sealed`].
static LAST_KNOWN_SEAL: AtomicU8 = AtomicU8::new(SEAL_UNKNOWN);

/// Whether a soul is sealed: the `soul.invariant` collection exists. The
/// answer is remembered, and a read that fails answers the last known one
/// with a warning — never "unsealed", which every caller would act on as
/// Learning Mode (the verification of 2026-10-24: twelve callers read the
/// error as `false`). Before the first successful read the error is
/// returned. A seal or a ceremony changes the collection, and the next
/// successful read follows it.
pub async fn is_soul_sealed(db: &WardsonDbClient) -> Result<bool> {
    is_soul_sealed_with(db, &LAST_KNOWN_SEAL).await
}

/// [`is_soul_sealed`] with the memory as a parameter, for a test.
pub(crate) async fn is_soul_sealed_with(db: &WardsonDbClient, last_known: &AtomicU8) -> Result<bool> {
    match db.collection_exists("soul.invariant").await {
        Ok(sealed) => {
            last_known.store(if sealed { SEAL_PRESENT } else { SEAL_ABSENT }, Ordering::Relaxed);
            Ok(sealed)
        }
        Err(e) => match last_known.load(Ordering::Relaxed) {
            SEAL_UNKNOWN => Err(e),
            known => {
                let sealed = known == SEAL_PRESENT;
                tracing::warn!(
                    "soul seal read failed; answering the last known state ({}): {e}",
                    if sealed { "sealed" } else { "unsealed" }
                );
                Ok(sealed)
            }
        },
    }
}

/// Whether a seal hash is stored at `hash_path`: `Ok(true)` for a file of any
/// content, `Ok(false)` only when nothing is there, and the error when that
/// cannot be told (a permission or I/O failure). Presence, not readability,
/// is the question a missing soul document raises: a hash file beside a
/// missing document is the state trustd halts on, whatever the file holds.
pub(crate) fn soul_hash_is_stored(hash_path: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(hash_path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// What the seal looks like now (#15): no soul; a soul whose hash matches
/// the stored one; or a soul present with a different stored hash or none.
/// trustd verifies at boot and halts on a mismatch, so the third state is
/// never a boot that was let through — it is a soul or a hash file changed
/// since. The status surfaces print it, which is what the issue asked the
/// display to be able to reveal; the mode still follows the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealState {
    Unsealed,
    Sealed,
    SealedUnverified,
}

impl SealState {
    /// The word the status surfaces print.
    pub fn status_word(self) -> &'static str {
        match self {
            SealState::Unsealed => "unsealed",
            SealState::Sealed => "sealed",
            SealState::SealedUnverified => "sealed, unverified",
        }
    }
}

/// The seal re-verified against `SOUL_HASH_PATH`.
pub async fn seal_state(db: &WardsonDbClient) -> SealState {
    seal_state_at(db, Path::new(SOUL_HASH_PATH)).await
}

/// `seal_state` against a given hash file (a test's directory). A soul
/// that cannot be read reads as unsealed here, in the status word;
/// `is_soul_sealed`, which the mode follows, answers its last known state.
pub(crate) async fn seal_state_at(db: &WardsonDbClient, hash_path: &Path) -> SealState {
    let Some(soul) = load_soul(db).await.ok().flatten() else {
        return SealState::Unsealed;
    };
    seal_of(&soul, hash_path)
}

/// The seal of a soul already loaded, against the hash file: `Sealed` when
/// the hash computed now is the stored one, `SealedUnverified` otherwise.
pub(crate) fn seal_of(soul: &serde_json::Value, hash_path: &Path) -> SealState {
    let stored = std::fs::read_to_string(hash_path).ok().map(|s| s.trim().to_string());
    match (compute_soul_hash(soul), stored) {
        (Ok(computed), Some(stored)) if computed == stored => SealState::Sealed,
        _ => SealState::SealedUnverified,
    }
}

pub async fn load_soul(db: &WardsonDbClient) -> Result<Option<serde_json::Value>> {
    Ok(load_soul_doc(db)
        .await?
        .and_then(|doc| doc.get("soul").cloned()))
}

/// Load the soul document, trying direct GET first, falling back to query.
async fn load_soul_doc(db: &WardsonDbClient) -> Result<Option<serde_json::Value>> {
    if !db.collection_exists("soul.invariant").await? {
        return Ok(None);
    }
    // Try direct GET by well-known ID. Only a document that is not there
    // falls through to the query (pre-migration data under another id); a
    // failing server is an error, never an absence.
    match db.read("soul.invariant", "soul").await {
        Ok(doc) => return Ok(Some(doc)),
        Err(e) if crate::db::error::is_not_found(&e) => {}
        Err(e) => return Err(e),
    }
    // Fallback: query (pre-migration data)
    let results = db
        .query("soul.invariant", &crate::db::client::first_doc_query_body())
        .await?;
    Ok(results.into_iter().next())
}

fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Compute the SHA-256 hash of a soul value, using the same serialization
/// as `seal_soul` — and as embra-trustd, which verifies it at every boot.
/// Shared by startup validation and migrations.
pub fn compute_soul_hash(soul: &serde_json::Value) -> Result<String> {
    let soul_json = serde_json::to_string_pretty(soul)?;
    Ok(compute_sha256(&soul_json))
}

#[cfg(test)]
mod parity_tests {
    use super::compute_soul_hash;

    /// A soul value with its keys out of order, nested, with empty
    /// containers, text outside ASCII and a line break.
    ///
    /// The same value and the same hash stand in embra-trustd's tests
    /// (`embra-trustd/src/soul.rs`). trustd computes the hash again at every
    /// boot and halts on a difference, so the two sides have to serialize a
    /// value the same way, byte for byte. If one of them changes, its test
    /// fails.
    const PARITY_SOUL: &str = r#"{"name":"Parity","format":"graph.v1","nodes":[{"id":"self","type":"self","text":"Ünïcode — “quoted”\nsecond line","weight":1},{"id":"a","type":"value","text":"t","tags":[],"meta":{}}],"edges":[{"src":"self","dst":"a","relation":"holds"}],"n":42,"neg":-7,"flag":true,"nothing":null}"#;
    const PARITY_HASH: &str = "2e825f06286345aa0e369f9107d34b5d9e77ddd4bdd693408f7a8a997a170f51";

    #[test]
    fn the_seal_hash_is_the_one_trustd_computes() {
        let soul: serde_json::Value = serde_json::from_str(PARITY_SOUL).unwrap();
        assert_eq!(compute_soul_hash(&soul).unwrap(), PARITY_HASH);
    }
}

#[cfg(test)]
mod hash_file_tests {
    //! The seal's hash file (#53 b6): whole or absent, never truncated.
    use super::write_soul_hash;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "embra-soul-hash-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn the_hash_file_is_written_atomically() {
        let dir = temp("write");
        let path = dir.join("soul.sha256");
        write_soul_hash(&path, "abc123").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc123");
        // A re-seal replaces it, and no temp is left beside it.
        write_soul_hash(&path, "def456").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "def456");
        assert_eq!(names(&dir), vec!["soul.sha256".to_string()]);
        // A missing parent is created.
        let nested = dir.join("state").join("soul.sha256");
        write_soul_hash(&nested, "789").await.unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "789");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_hash_write_leaves_no_temp() {
        // The parent is a file, so no temp can be created in it.
        let dir = temp("fail");
        std::fs::write(dir.join("state"), "a file where the directory should be").unwrap();
        let err = write_soul_hash(&dir.join("state").join("soul.sha256"), "abc").await.unwrap_err();
        assert!(err.to_string().starts_with("Failed to write soul hash to STATE"), "{err}");
        assert_eq!(names(&dir), vec!["state".to_string()], "nothing else was created");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod seal_memory_tests {
    use super::*;
    use crate::db::WardsonDbClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
        server
    }

    /// A seal read that fails answers the last known state, sealed or
    /// unsealed, and is an error only before anything is known: a transient
    /// database error never flips the callers to Learning Mode.
    #[tokio::test]
    async fn a_failed_seal_read_answers_the_last_known_state_and_never_unsealed() {
        let known = AtomicU8::new(SEAL_UNKNOWN);
        let failing = server(500).await;
        let db_failing = WardsonDbClient::from_url(&failing.uri());
        assert!(is_soul_sealed_with(&db_failing, &known).await.is_err(), "nothing known yet: the error is returned");
        let sealed = server(200).await;
        assert!(is_soul_sealed_with(&WardsonDbClient::from_url(&sealed.uri()), &known).await.unwrap());
        assert!(is_soul_sealed_with(&db_failing, &known).await.unwrap(), "the last known state is sealed");
        let unsealed = server(404).await;
        assert!(!is_soul_sealed_with(&WardsonDbClient::from_url(&unsealed.uri()), &known).await.unwrap());
        assert!(!is_soul_sealed_with(&db_failing, &known).await.unwrap(), "the last known state is unsealed");
    }
}

#[cfg(test)]
mod seal_state_tests {
    use super::{SealState, compute_soul_hash, seal_state_at};
    use crate::db::WardsonDbClient;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn envelope(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": v, "meta": {}}))
    }

    async fn server_with_soul(soul: &serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/soul.invariant/docs/soul"))
            .respond_with(envelope(json!({"_id": "soul", "soul": soul})))
            .mount(&server)
            .await;
        server
    }

    fn hash_file(tag: &str, contents: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-seal-state-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("soul.sha256");
        if let Some(c) = contents {
            std::fs::write(&file, c).unwrap();
        }
        file
    }

    /// #15: the word follows the hash as stored now — a match is sealed; a
    /// different or missing hash is sealed and unverified; no soul is
    /// unsealed. trustd halts a mismatch at boot, so the middle state is a
    /// soul or a file changed since, which the status surfaces now show.
    #[tokio::test]
    async fn the_seal_is_re_verified_against_the_stored_hash() {
        let soul = json!({"name": "Embra", "values": ["candor"]});
        let hash = compute_soul_hash(&soul).unwrap();
        let server = server_with_soul(&soul).await;
        let db = WardsonDbClient::from_url(&server.uri());
        assert_eq!(seal_state_at(&db, &hash_file("match", Some(&hash))).await, SealState::Sealed);
        assert_eq!(seal_state_at(&db, &hash_file("other", Some("0000"))).await, SealState::SealedUnverified);
        assert_eq!(seal_state_at(&db, &hash_file("none", None)).await, SealState::SealedUnverified);

        let empty = MockServer::start().await;
        Mock::given(method("GET")).and(path("/soul.invariant")).respond_with(ResponseTemplate::new(404)).mount(&empty).await;
        let db = WardsonDbClient::from_url(&empty.uri());
        assert_eq!(seal_state_at(&db, &hash_file("unsealed", Some(&hash))).await, SealState::Unsealed);

        assert_eq!(SealState::Sealed.status_word(), "sealed");
        assert_eq!(SealState::SealedUnverified.status_word(), "sealed, unverified");
        assert_eq!(SealState::Unsealed.status_word(), "unsealed");
    }
}
