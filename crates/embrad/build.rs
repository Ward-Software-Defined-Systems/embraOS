// Forwards EMBRA_STORAGE_ENGINE to rustc so option_env! in supervisor.rs
// is invalidated when the operator picks a different backend via build-image.sh.
// rerun-if-env-changed alone would only re-run this script, not recompile
// supervisor.rs — rustc-env is what makes cargo treat the value as a rustc input.
fn main() {
    println!("cargo:rerun-if-env-changed=EMBRA_STORAGE_ENGINE");
    if let Ok(v) = std::env::var("EMBRA_STORAGE_ENGINE") {
        println!("cargo:rustc-env=EMBRA_STORAGE_ENGINE={v}");
    }
}
