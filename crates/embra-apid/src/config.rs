use embra_common::args::Flags;

#[derive(Clone)]
pub struct ApidConfig {
    pub grpc_port: u16,
    pub rest_port: u16,
    pub brain_addr: String,
    pub trust_addr: String,
}

impl ApidConfig {
    pub fn from_args() -> Self {
        Self::from_args_iter(std::env::args().skip(1))
    }

    /// The parser behind [`from_args`], over any argument list (the program
    /// name already skipped), through the one flag parser of the services
    /// (`embra_common::args`, #41). The defaults are the literals embrad
    /// passes. A flag without its value stops the service.
    pub fn from_args_iter<I: IntoIterator<Item = String>>(args: I) -> Self {
        let flags = Flags::parse(args, &["--grpc-port", "--rest-port", "--brain-addr", "--trust-addr"])
            .unwrap_or_else(|e| panic!("{e}"));
        Self {
            grpc_port: flags.value("--grpc-port").map_or(50000, |v| v.parse().expect("Invalid gRPC port")),
            rest_port: flags.value("--rest-port").map_or(8443, |v| v.parse().expect("Invalid REST port")),
            brain_addr: flags.value("--brain-addr").unwrap_or("http://127.0.0.1:50002").to_string(),
            trust_addr: flags.value("--trust-addr").unwrap_or("http://127.0.0.1:50001").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApidConfig;

    fn parse(args: &[&str]) -> ApidConfig {
        ApidConfig::from_args_iter(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_defaults_are_the_contract_and_every_flag_overrides_its_own() {
        let cfg = parse(&[]);
        assert_eq!((cfg.grpc_port, cfg.rest_port), (50000, 8443));
        assert_eq!((cfg.brain_addr.as_str(), cfg.trust_addr.as_str()), ("http://127.0.0.1:50002", "http://127.0.0.1:50001"));
        let cfg = parse(&["--trust-addr", "http://t:1", "--rest-port", "9", "--grpc-port", "8", "--brain-addr", "http://b:2"]);
        assert_eq!((cfg.grpc_port, cfg.rest_port), (8, 9));
        assert_eq!((cfg.brain_addr.as_str(), cfg.trust_addr.as_str()), ("http://b:2", "http://t:1"));
    }

    #[test]
    #[should_panic(expected = "--grpc-port needs a value")]
    fn a_flag_without_its_value_stops_the_service() {
        parse(&["--grpc-port"]);
    }
}
