use embra_common::args::Flags;
use std::path::PathBuf;

#[derive(Clone)]
pub struct TrustdConfig {
    pub port: u16,
    pub state_dir: PathBuf,
    pub wardsondb_url: String,
}

impl TrustdConfig {
    pub fn from_args() -> Self {
        Self::from_args_iter(std::env::args())
    }

    /// The parser behind [`from_args`], over any argument list (the program
    /// name first, as `std::env::args` gives it), so a test runs it; the
    /// flags come through the one parser of the services
    /// (`embra_common::args`, #41). The defaults are the literals embrad
    /// passes (`embrad/src/supervisor.rs`): port 50001, `/embra/state`,
    /// `http://127.0.0.1:8090`. A flag without its value is a configuration
    /// error, and the service stops there rather than guess.
    pub fn from_args_iter<I: IntoIterator<Item = String>>(args: I) -> Self {
        let flags = Flags::parse(args.into_iter().skip(1), &["--port", "--state-dir", "--wardsondb-url"])
            .unwrap_or_else(|e| panic!("{e}"));
        Self {
            port: flags.value("--port").map_or(50001, |v| v.parse().expect("Invalid port")),
            state_dir: flags.value("--state-dir").map_or_else(|| PathBuf::from("/embra/state"), PathBuf::from),
            wardsondb_url: flags.value("--wardsondb-url").unwrap_or("http://127.0.0.1:8090").to_string(),
        }
    }

    pub fn soul_hash_path(&self) -> PathBuf {
        self.state_dir.join("soul.sha256")
    }

    pub fn pki_dir(&self) -> PathBuf {
        self.state_dir.join("pki")
    }

}

#[cfg(test)]
mod tests {
    use super::TrustdConfig;
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> TrustdConfig {
        TrustdConfig::from_args_iter(std::iter::once("embra-trustd".to_string()).chain(args.iter().map(|s| s.to_string())))
    }

    /// The defaults are the contract embrad hard-codes against, and the
    /// two paths derive from the state directory: the soul hash and the
    /// PKI live where embrad and the brain expect them (#52).
    #[test]
    fn the_defaults_and_the_derived_paths_are_the_contract() {
        let cfg = parse(&[]);
        assert_eq!(cfg.port, 50001);
        assert_eq!(cfg.state_dir, PathBuf::from("/embra/state"));
        assert_eq!(cfg.wardsondb_url, "http://127.0.0.1:8090");
        assert_eq!(cfg.soul_hash_path(), PathBuf::from("/embra/state/soul.sha256"));
        assert_eq!(cfg.pki_dir(), PathBuf::from("/embra/state/pki"));
    }

    #[test]
    fn every_flag_overrides_its_default_in_any_order() {
        let cfg = parse(&["--wardsondb-url", "http://db:1", "--state-dir", "/x", "--port", "7"]);
        assert_eq!(cfg.port, 7);
        assert_eq!(cfg.soul_hash_path(), PathBuf::from("/x/soul.sha256"));
        assert_eq!(cfg.pki_dir(), PathBuf::from("/x/pki"));
        assert_eq!(cfg.wardsondb_url, "http://db:1");
        let cfg = parse(&["--port", "8", "ignored", "--state-dir", "/y"]);
        assert_eq!((cfg.port, cfg.state_dir), (8, PathBuf::from("/y")));
    }

    #[test]
    #[should_panic(expected = "--port needs a value")]
    fn a_flag_without_its_value_stops_the_service() {
        parse(&["--port"]);
    }
}
