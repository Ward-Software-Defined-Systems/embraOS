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

/// The cargo that builds a guest: the pinned toolchain's binary, with the
/// environment cleared. A guest's compile-time read of the environment
/// (`env!`, which the validator refuses, and whatever a future rule misses)
/// then finds none of the brain's secrets — the verification of 2026-10-24
/// compiled `core::env!("GITHUB_TOKEN")` under the inherited environment
/// (#12). The child gets `PATH` (the toolchain first), `CARGO_HOME` and
/// `CARGO_TARGET_DIR`, and nothing else; cargo and rustc need no `HOME`
/// once `CARGO_HOME` is set.
fn cargo_command(env: &BuildEnv, manifest: &Path) -> tokio::process::Command {
    let path_env = {
        let cur = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", env.toolchain_bin.display(), cur)
    };
    let mut cmd = tokio::process::Command::new(env.toolchain_bin.join("cargo"));
    cmd.args(build_args(manifest))
        .env_clear()
        .env("PATH", path_env)
        .env("CARGO_HOME", &env.cargo_home)
        .env("CARGO_TARGET_DIR", &env.target_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd
}

/// Compile the scaffolded project. `Err(BuildFailed(tail))` carries the
/// cargo output tail so the intelligence can self-correct.
pub async fn build(
    paths: &ScaffoldPaths,
    env: &BuildEnv,
    timeout: Duration,
) -> Result<BuildArtifact, GuardianError> {
    let child = cargo_command(env, &paths.manifest)
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

    /// The build's environment is the three variables cargo needs and
    /// nothing the brain holds (#12): a stand-in `cargo` that prints its
    /// environment sees `PATH`, `CARGO_HOME` and `CARGO_TARGET_DIR` and no
    /// `HOME`, which every shell hands a process.
    #[tokio::test]
    async fn the_build_runs_cargo_in_a_cleared_environment() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("embra-guardian-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("cargo");
        std::fs::write(&fake, "#!/bin/sh\nenv\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BuildEnv {
            toolchain_bin: dir.clone(),
            cargo_home: dir.join("home"),
            target_dir: dir.join("target"),
        };
        let out = cargo_command(&env, Path::new("/x/Cargo.toml")).output().await.unwrap();
        let printed = String::from_utf8_lossy(&out.stdout);
        let names: Vec<&str> = printed.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
        assert!(names.contains(&"PATH") && names.contains(&"CARGO_HOME") && names.contains(&"CARGO_TARGET_DIR"), "{printed}");
        // The shell adds `PWD` and the like of its own; what must be absent
        // is what the brain's process carries.
        for inherited in ["HOME", "USER", "LOGNAME", "RUSTUP_TOOLCHAIN", "RUSTFLAGS"] {
            assert!(!names.contains(&inherited), "the inherited {inherited} reached the build: {printed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
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
