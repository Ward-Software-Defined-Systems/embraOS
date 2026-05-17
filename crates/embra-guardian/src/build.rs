//! In-OS build runner. Invokes the prebaked `/opt/rust` toolchain's
//! `cargo` to compile a scaffolded project to `wasm32-unknown-unknown`,
//! fully offline (zero deps ⇒ no crates.io, deterministic), with a hard
//! timeout. The brain runs this on a background task (validate is sync,
//! build is async) so a cold ~10–25 s build never stalls the model loop.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::GuardianError;
use crate::scaffold::ScaffoldPaths;

pub const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(90);
const LOG_TAIL_CAP: usize = 8 * 1024;

/// Locations of the prebaked toolchain + the shared, writable cargo dirs
/// on DATA (`/embra/workspace/.guardian/{cargo-home,target}`).
#[derive(Debug, Clone)]
pub struct BuildEnv {
    /// `/opt/rust/bin` (contains `cargo`, `rustc`, `rust-lld`).
    pub toolchain_bin: PathBuf,
    pub cargo_home: PathBuf,
    pub target_dir: PathBuf,
}

pub struct BuildArtifact {
    pub wasm: Vec<u8>,
    pub log_tail: String,
}

pub fn build_args(manifest: &Path) -> Vec<String> {
    vec![
        "build".into(),
        "--release".into(),
        "--offline".into(),
        "--target".into(),
        "wasm32-unknown-unknown".into(),
        "--manifest-path".into(),
        manifest.display().to_string(),
    ]
}

pub fn artifact_path(env: &BuildEnv, paths: &ScaffoldPaths) -> PathBuf {
    env.target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(&paths.artifact_name)
}

fn tail(s: &str) -> String {
    if s.len() <= LOG_TAIL_CAP {
        return s.to_string();
    }
    let mut i = s.len() - LOG_TAIL_CAP;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    format!("…[{} earlier bytes truncated]\n{}", i, &s[i..])
}

/// Compile the scaffolded project. `Err(BuildFailed(tail))` carries the
/// cargo output tail so the intelligence can self-correct.
pub async fn build(
    paths: &ScaffoldPaths,
    env: &BuildEnv,
    timeout: Duration,
) -> Result<BuildArtifact, GuardianError> {
    use tokio::process::Command;

    let path_env = {
        let cur = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", env.toolchain_bin.display(), cur)
    };

    let child = Command::new(env.toolchain_bin.join("cargo"))
        .args(build_args(&paths.manifest))
        .env("CARGO_HOME", &env.cargo_home)
        .env("CARGO_TARGET_DIR", &env.target_dir)
        .env("PATH", path_env)
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTFLAGS")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| GuardianError::BuildFailed(format!("spawn cargo: {e}")))?;

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(GuardianError::BuildFailed(e.to_string())),
        Err(_) => {
            return Err(GuardianError::BuildFailed(format!(
                "build exceeded {timeout:?} (killed)"
            )));
        }
    };

    let mut log = String::from_utf8_lossy(&out.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&out.stderr));
    let log_tail = tail(&log);

    if !out.status.success() {
        return Err(GuardianError::BuildFailed(format!(
            "cargo build failed:\n{log_tail}"
        )));
    }

    let art = artifact_path(env, paths);
    let wasm = std::fs::read(&art)
        .map_err(|e| GuardianError::BuildFailed(format!("artifact {}: {e}", art.display())))?;
    Ok(BuildArtifact { wasm, log_tail })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_are_offline_release_wasm() {
        let a = build_args(Path::new("/p/Cargo.toml"));
        assert!(a.contains(&"--offline".to_string()));
        assert!(a.contains(&"--release".to_string()));
        assert_eq!(a[a.iter().position(|x| x == "--target").unwrap() + 1], "wasm32-unknown-unknown");
        assert_eq!(a.last().unwrap(), "/p/Cargo.toml");
    }

    #[test]
    fn artifact_path_layout() {
        let env = BuildEnv {
            toolchain_bin: "/opt/rust/bin".into(),
            cargo_home: "/embra/workspace/.guardian/cargo-home".into(),
            target_dir: "/embra/workspace/.guardian/target".into(),
        };
        let paths = ScaffoldPaths {
            project: "/x".into(),
            manifest: "/x/Cargo.toml".into(),
            lib_rs: "/x/src/lib.rs".into(),
            artifact_name: "web_search.wasm".into(),
        };
        assert_eq!(
            artifact_path(&env, &paths),
            Path::new("/embra/workspace/.guardian/target/wasm32-unknown-unknown/release/web_search.wasm")
        );
    }

    #[test]
    fn tail_truncates_long_logs() {
        let big = "x".repeat(LOG_TAIL_CAP + 500);
        let t = tail(&big);
        assert!(t.starts_with("…["));
        assert!(t.len() < big.len());
        assert!(tail("short").starts_with("short"));
    }
}
