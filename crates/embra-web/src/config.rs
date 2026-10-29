//! CLI/env configuration for embra-web.
//!
//! The flags come through the one parser of the services
//! (`embra_common::args`, #41) — no clap dependency, to keep the crate
//! lean and consistent with the rest of the workspace.

use embra_common::args::Flags;

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
    /// This boot's access token, from `EMBRA_WEB_TOKEN` (embrad mints it
    /// and prints the URL that carries it); `None` on the host, where
    /// embra-web mints its own. An environment variable, never an
    /// argument: `ps` shows arguments.
    pub token: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            port: 3345,
            apid_addr: "http://127.0.0.1:50000".to_string(),
            trust_addr: "http://127.0.0.1:50001".to_string(),
            console_bin: "/usr/bin/embra-console".to_string(),
            token: None,
        }
    }
}

impl WebConfig {
    pub fn from_args() -> Self {
        let token = std::env::var(crate::access::TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        Self::from_args_iter(std::env::args().skip(1), token)
    }

    /// The parser behind [`from_args`], over any argument list (the program
    /// name already skipped) and the token the environment gave. A flag
    /// without its value stops the service.
    pub fn from_args_iter<I: IntoIterator<Item = String>>(args: I, token: Option<String>) -> Self {
        let flags = Flags::parse(args, &["--port", "--apid-addr", "--trust-addr", "--console-bin"])
            .unwrap_or_else(|e| panic!("{e}"));
        let mut cfg = WebConfig { token, ..WebConfig::default() };
        if let Some(port) = flags.value("--port") {
            cfg.port = port.parse().expect("Invalid --port");
        }
        if let Some(addr) = flags.value("--apid-addr") {
            cfg.apid_addr = addr.to_string();
        }
        if let Some(addr) = flags.value("--trust-addr") {
            cfg.trust_addr = addr.to_string();
        }
        if let Some(bin) = flags.value("--console-bin") {
            cfg.console_bin = bin.to_string();
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::WebConfig;

    fn parse(args: &[&str]) -> WebConfig {
        WebConfig::from_args_iter(args.iter().map(|s| s.to_string()), Some("tok".to_string()))
    }

    #[test]
    fn the_defaults_are_the_contract_and_every_flag_overrides_its_own() {
        let cfg = parse(&[]);
        assert_eq!(cfg.port, 3345);
        assert_eq!(cfg.apid_addr, "http://127.0.0.1:50000");
        assert_eq!(cfg.trust_addr, "http://127.0.0.1:50001");
        assert_eq!(cfg.console_bin, "/usr/bin/embra-console");
        assert_eq!(cfg.token.as_deref(), Some("tok"), "the token comes from the environment, never a flag");
        let cfg = parse(&["--console-bin", "/x/console", "--port", "4", "--trust-addr", "http://t:1", "--apid-addr", "http://a:2"]);
        assert_eq!((cfg.port, cfg.apid_addr.as_str(), cfg.trust_addr.as_str(), cfg.console_bin.as_str()), (4, "http://a:2", "http://t:1", "/x/console"));
    }

    #[test]
    #[should_panic(expected = "--port needs a value")]
    fn a_flag_without_its_value_stops_the_service() {
        parse(&["--port"]);
    }
}
