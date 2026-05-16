//! CLI/env configuration for embra-web.
//!
//! Manual arg parsing in the same style as `embra-apid`
//! (`crates/embra-apid/src/config.rs`) — no clap dependency, to keep the
//! crate lean and consistent with the rest of the workspace.

#[derive(Clone, Debug)]
pub struct WebConfig {
    /// HTTPS listen port.
    pub port: u16,
    /// embra-apid gRPC address — passed through to the PTY-hosted
    /// `embra-console` so it reaches the brain via the existing proxy.
    pub apid_addr: String,
    /// embra-trustd gRPC address — used at boot to obtain the serving cert.
    pub trust_addr: String,
    /// Path to the `embra-console` binary the PTY bridge spawns.
    pub console_bin: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            port: 3345,
            apid_addr: "http://127.0.0.1:50000".to_string(),
            trust_addr: "http://127.0.0.1:50001".to_string(),
            console_bin: "/usr/bin/embra-console".to_string(),
        }
    }
}

impl WebConfig {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut cfg = WebConfig::default();

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--port" => {
                    cfg.port = args[i + 1].parse().expect("Invalid --port");
                    i += 2;
                }
                "--apid-addr" => {
                    cfg.apid_addr = args[i + 1].clone();
                    i += 2;
                }
                "--trust-addr" => {
                    cfg.trust_addr = args[i + 1].clone();
                    i += 2;
                }
                "--console-bin" => {
                    cfg.console_bin = args[i + 1].clone();
                    i += 2;
                }
                _ => {
                    i += 1;
                }
            }
        }

        cfg
    }
}
