//! The one flag parser for the services' command lines (#41).
//!
//! Every service used to carry its own `while i < args.len()` loop over
//! `--name value` pairs, each with the same panic-prone `args[i + 1]`.
//! This is that loop once: a known flag takes the token after it, a word
//! that is not a known flag is skipped, and a known flag at the end of the
//! line with nothing after it is an error naming the flag, so a service
//! stops on a bad command line instead of guessing. No `clap`: the
//! services are static musl binaries with a handful of flags each, and
//! embrad writes the lines they read.

/// The `--name value` pairs of a command line, by the names a service
/// knows. The last value given for a name wins, as the loops' overwrites
/// did.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pairs: Vec<(String, String)>,
}

impl Flags {
    /// Parses `args`, the program name already skipped. `known` names the
    /// flags that take a value; every other token is skipped one at a
    /// time, so an unknown word or flag never swallows a known one after
    /// it. A known flag with no token after it is `Err("--name needs a
    /// value")`.
    pub fn parse(args: impl IntoIterator<Item = String>, known: &[&str]) -> Result<Flags, String> {
        let args: Vec<String> = args.into_iter().collect();
        let mut pairs = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if known.contains(&args[i].as_str()) {
                let value = args.get(i + 1).ok_or_else(|| format!("{} needs a value", args[i]))?;
                pairs.push((args[i].clone(), value.clone()));
                i += 2;
            } else {
                i += 1;
            }
        }
        Ok(Flags { pairs })
    }

    /// The last value given for `flag`, or `None` when the line did not
    /// carry it.
    pub fn value(&self, flag: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(name, _)| name == flag)
            .map(|(_, value)| value.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Flags;

    fn parse(line: &[&str], known: &[&str]) -> Result<Flags, String> {
        Flags::parse(line.iter().map(|s| s.to_string()), known)
    }

    #[test]
    fn a_known_flag_takes_the_token_after_it_and_the_last_one_wins() {
        let flags = parse(&["--port", "7", "--addr", "http://a", "--port", "8"], &["--port", "--addr"]).unwrap();
        assert_eq!(flags.value("--port"), Some("8"));
        assert_eq!(flags.value("--addr"), Some("http://a"));
        assert_eq!(flags.value("--other"), None);
    }

    /// A word that is not a known flag is skipped on its own, so it never
    /// takes the token after it: `ignored --state-dir /y` still reads the
    /// directory, as every service's loop did.
    #[test]
    fn unknown_words_are_skipped_one_at_a_time() {
        let flags = parse(&["ignored", "--state-dir", "/y", "--unknown", "--port", "8"], &["--state-dir", "--port"]).unwrap();
        assert_eq!(flags.value("--state-dir"), Some("/y"));
        assert_eq!(flags.value("--port"), Some("8"));
        assert_eq!(flags.value("--unknown"), None);
    }

    #[test]
    fn a_known_flag_at_the_end_without_its_value_is_an_error_naming_it() {
        let err = parse(&["--addr", "x", "--port"], &["--port", "--addr"]).unwrap_err();
        assert_eq!(err, "--port needs a value");
        assert!(parse(&[], &["--port"]).unwrap().value("--port").is_none());
    }
}
