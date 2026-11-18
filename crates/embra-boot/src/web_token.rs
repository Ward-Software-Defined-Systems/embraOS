//! The web console's access token, kept on STATE.
//!
//! Every request to the web console carries the token (embra-web's
//! `access.rs`). embrad makes it once, stores it here and hands it to
//! embra-web and to the display console at every boot, so the console's URL
//! stays the same across boots and a bookmark keeps working. The operator
//! asks for a new one with `/web-token rotate`: the brain writes the request
//! beside the token and never reads the token; embrad makes the new one,
//! stores it and starts the web console and the display console again.
//!
//! The directory belongs to the machine, as its saved clock does: a snapshot
//! leaves it out and a restore keeps the one in place (embra-update's
//! `STATE_EXCLUDED` and `STATE_KEPT_BY_RESTORE`), so no restored backup
//! brings back a token that was replaced. The brain's read jail refuses the
//! directory with the rest of STATE.

use std::io;
use std::path::Path;

/// The directory's name under STATE.
pub const DIR_NAME: &str = "web-console";
/// The directory.
pub const DIR: &str = "/embra/state/web-console";
/// The token's file in it: one line, `TOKEN_LEN` lowercase hex characters.
pub const TOKEN_NAME: &str = "token";
/// The operator's request for a new token: an empty file. It stays until a
/// new token is stored, so a request the power cut short is answered by the
/// next boot.
pub const ROTATE_NAME: &str = "rotate";
/// A token's length: 16 bytes of `/dev/urandom` in hex.
pub const TOKEN_LEN: usize = 32;

/// Whether a text is a token as `mint` makes one.
pub fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A new token: 16 bytes of `/dev/urandom` as lowercase hex. A failed read
/// is an error, never a weaker source.
pub fn mint() -> io::Result<String> {
    use io::Read;
    let mut bytes = [0u8; TOKEN_LEN / 2];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The stored token, when the file holds a well-formed one. Anything else
/// (no file, an empty or a cut one) is no token, and a new one is made.
pub fn read(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(TOKEN_NAME)).ok()?;
    let token = text.strip_suffix('\n').unwrap_or(&text);
    well_formed(token).then(|| token.to_string())
}

/// The directory, private to root from its creation.
fn make_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A file in the directory, written whole and private: a temporary file
/// beside it, created 0600, synced and renamed over, and the directory
/// synced, so a power cut leaves the old file or the new one.
fn write_private(dir: &Path, name: &str, text: &str) -> io::Result<()> {
    use io::Write;
    make_dir(dir)?;
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, &path)?;
    // The rename is in the directory: synced too, where the platform lets a
    // directory be opened and synced.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Stores a token. One that is not well formed is refused: what is stored
/// is what `read` takes.
pub fn store(dir: &Path, token: &str) -> io::Result<()> {
    if !well_formed(token) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not an access token"));
    }
    write_private(dir, TOKEN_NAME, &format!("{token}\n"))
}

/// The operator's request for a new token (the brain, for `/web-token
/// rotate`).
pub fn ask_rotation(dir: &Path) -> io::Result<()> {
    write_private(dir, ROTATE_NAME, "")
}

/// Whether a new token has been asked for and not made yet.
pub fn rotation_asked(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(ROTATE_NAME)).is_ok()
}

/// The request is answered: its file goes. No file is no error.
pub fn rotation_done(dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(dir.join(ROTATE_NAME)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A new token in place of the stored one, for a request: minted, stored,
/// and the request removed, in that order, so a request is gone only once
/// its token is on STATE. The error of the first step that failed; the
/// stored token is the old one then, and the request stays.
pub fn rotate(dir: &Path) -> io::Result<String> {
    rotate_with(dir, mint)
}

fn rotate_with(dir: &Path, mint: impl FnOnce() -> io::Result<String>) -> io::Result<String> {
    let token = mint()?;
    store(dir, &token)?;
    rotation_done(dir)?;
    Ok(token)
}

/// Where the token of a boot came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The stored one, as at the boot before.
    Kept,
    /// A new one, stored: a first boot, a file that held no token, or the
    /// operator's request.
    Made,
    /// A new one that could not be stored (the reason): it gates this boot,
    /// and the next boot makes another.
    NotStored(String),
}

/// The token a boot's web console is gated with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootToken {
    pub token: String,
    pub origin: Origin,
}

/// The token for this boot: the stored one, unless there is none or a new
/// one was asked for. `Err` when no token can be minted at all: there is no
/// gate then, and the web console is not started.
pub fn for_boot(dir: &Path) -> io::Result<BootToken> {
    for_boot_with(dir, mint)
}

fn for_boot_with(dir: &Path, mint: impl FnOnce() -> io::Result<String>) -> io::Result<BootToken> {
    let asked = rotation_asked(dir);
    if !asked && let Some(token) = read(dir) {
        return Ok(BootToken { token, origin: Origin::Kept });
    }
    let token = mint()?;
    let origin = match store(dir, &token) {
        Ok(()) => {
            // A request that cannot be removed is asked again at the next
            // boot, which makes one more token: nothing is lost by it.
            let _ = rotation_done(dir);
            Origin::Made
        }
        Err(e) => Origin::NotStored(e.to_string()),
    };
    Ok(BootToken { token, origin })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-web-token-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_token_is_thirty_two_lowercase_hex_characters() {
        let (a, b) = (mint().unwrap(), mint().unwrap());
        assert!(well_formed(&a) && well_formed(&b), "{a} {b}");
        assert_ne!(a, b, "two tokens differ");
        for bad in ["", "0123", &A[..31], &format!("{A}0"), &A.to_uppercase(), "0123456789abcdef0123456789abcdeg", "0123456789abcdef 123456789abcdef"] {
            assert!(!well_formed(bad), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_token_is_stored_private_and_whole_and_read_back() {
        use std::os::unix::fs::PermissionsExt;
        let state = scratch("store");
        let dir = state.join(DIR_NAME);
        assert_eq!(read(&dir), None, "no directory, no token");
        store(&dir, A).unwrap();
        assert_eq!(read(&dir).as_deref(), Some(A));
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700, "the directory is root's alone");
        assert_eq!(std::fs::metadata(dir.join(TOKEN_NAME)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(dir.join(TOKEN_NAME)).unwrap(), format!("{A}\n"));
        // Stored again over itself: the later one stands, and no temporary
        // file is left beside it.
        store(&dir, B).unwrap();
        assert_eq!(read(&dir).as_deref(), Some(B));
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [TOKEN_NAME]);
        // What is stored is what `read` takes: nothing else gets in.
        assert!(store(&dir, "not a token").is_err());
        assert_eq!(read(&dir).as_deref(), Some(B));
        // A file that holds no token is no token.
        for bad in ["", "\n", &A[..20], &format!("{A}\n\n"), &format!(" {A}\n"), &format!("{A} \n")] {
            std::fs::write(dir.join(TOKEN_NAME), bad).unwrap();
            assert_eq!(read(&dir), None, "{bad:?}");
        }
        std::fs::remove_dir_all(&state).unwrap();
        assert!(DIR.ends_with(DIR_NAME) && DIR.starts_with("/embra/state/"));
    }

    /// The first boot makes a token and stores it; every boot after it
    /// takes the stored one, which is what keeps the console's URL.
    #[test]
    fn a_boot_keeps_the_stored_token_and_makes_one_when_there_is_none() {
        let state = scratch("boot");
        let dir = state.join(DIR_NAME);
        let first = for_boot_with(&dir, || Ok(A.to_string())).unwrap();
        assert_eq!(first, BootToken { token: A.to_string(), origin: Origin::Made });
        for _ in 0..3 {
            let again = for_boot_with(&dir, || panic!("a stored token is not made again")).unwrap();
            assert_eq!(again, BootToken { token: A.to_string(), origin: Origin::Kept });
        }
        // A file that holds no token: a new one, stored.
        std::fs::write(dir.join(TOKEN_NAME), "cut sho").unwrap();
        let repaired = for_boot_with(&dir, || Ok(B.to_string())).unwrap();
        assert_eq!(repaired, BootToken { token: B.to_string(), origin: Origin::Made });
        assert_eq!(read(&dir).as_deref(), Some(B));
        // No token can be minted: no gate, and the caller starts no console.
        std::fs::remove_file(dir.join(TOKEN_NAME)).unwrap();
        assert!(for_boot_with(&dir, || Err(io::Error::other("no entropy"))).is_err());
        std::fs::remove_dir_all(&state).unwrap();
    }

    /// A new token that cannot be stored still gates the boot, and says so:
    /// the banner must not promise a URL that the next boot changes.
    #[cfg(unix)]
    #[test]
    fn a_token_that_cannot_be_stored_gates_the_boot_and_is_named_as_such() {
        let state = scratch("unstored");
        std::fs::create_dir_all(&state).unwrap();
        // A file where the directory would be: nothing can be stored under it.
        std::fs::write(state.join(DIR_NAME), "in the way").unwrap();
        let boot = for_boot_with(&state.join(DIR_NAME), || Ok(A.to_string())).unwrap();
        assert_eq!(boot.token, A);
        assert!(matches!(boot.origin, Origin::NotStored(_)), "{:?}", boot.origin);
        std::fs::remove_dir_all(&state).unwrap();
    }

    /// The operator's request: the stored token is replaced and the request
    /// goes, in that order. A request that was never answered (the power
    /// went first) is answered by the next boot.
    #[test]
    fn a_request_replaces_the_token_and_is_removed_once_the_new_one_is_stored() {
        let state = scratch("rotate");
        let dir = state.join(DIR_NAME);
        store(&dir, A).unwrap();
        assert!(!rotation_asked(&dir));
        ask_rotation(&dir).unwrap();
        ask_rotation(&dir).unwrap();
        assert!(rotation_asked(&dir), "asked twice is asked");
        assert_eq!(read(&dir).as_deref(), Some(A), "asking changes no token");

        // No token can be minted: the old one stands and the request stays.
        assert!(rotate_with(&dir, || Err(io::Error::other("no entropy"))).is_err());
        assert_eq!(read(&dir).as_deref(), Some(A));
        assert!(rotation_asked(&dir));

        assert_eq!(rotate_with(&dir, || Ok(B.to_string())).unwrap(), B);
        assert_eq!(read(&dir).as_deref(), Some(B));
        assert!(!rotation_asked(&dir), "answered");
        rotation_done(&dir).unwrap();

        // Asked, and the machine stopped before embrad looked: the boot
        // makes the new token and takes the request away.
        ask_rotation(&dir).unwrap();
        let boot = for_boot_with(&dir, || Ok(A.to_string())).unwrap();
        assert_eq!(boot, BootToken { token: A.to_string(), origin: Origin::Made });
        assert!(!rotation_asked(&dir));
        let after = for_boot_with(&dir, || panic!("kept from here on")).unwrap();
        assert_eq!(after.origin, Origin::Kept);
        std::fs::remove_dir_all(&state).unwrap();
    }
}
