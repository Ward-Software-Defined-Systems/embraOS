//! The clock as embrad left it at boot, for every reader after it.
//!
//! The kernel takes the time from the machine's hardware clock, and a board
//! whose clock battery ran down starts at its firmware's default date:
//! every certificate is then "not valid yet". embrad (`embrad/src/clock.rs`)
//! checks the clock against the image's build time before anything else,
//! asks a time server once the network is up, and records the outcome in
//! [`STATUS_PATH`], one `key=value` per line. The brain reads the record for
//! a certificate date error's hint and for `system_status`; embrad's banner
//! reads it for the one line it prints when the clock is still a guess.
//!
//! The date arithmetic lives here too (the release stamp's `BUILT`, the
//! dates in an operator's line, the hardware clock's fields): std alone, as
//! the rest of this crate.

use std::fmt;

/// Where embrad records what it made of the clock.
pub const STATUS_PATH: &str = "/run/embra/clock-status";

/// A date and a time of day in UTC (the proleptic Gregorian calendar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilTime {
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Days from 1970-01-01 to a date; a date before it counts down.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    // The year runs from March, so a leap day is its last day.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let month_from_march = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date `days` after 1970-01-01: year, month, day.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = (if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

impl CivilTime {
    /// The date and time at `secs` seconds after 1970-01-01T00:00:00Z.
    pub fn from_unix(secs: i64) -> CivilTime {
        let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
        let of_day = secs.rem_euclid(86_400) as u32;
        CivilTime { year, month, day, hour: of_day / 3_600, minute: of_day % 3_600 / 60, second: of_day % 60 }
    }

    /// Seconds after 1970-01-01T00:00:00Z.
    pub fn to_unix(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86_400
            + i64::from(self.hour) * 3_600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// The day of the week, 0 for Sunday (1970-01-01 was a Thursday).
    pub fn weekday(&self) -> u32 {
        (days_from_civil(self.year, self.month, self.day) + 4).rem_euclid(7) as u32
    }

    /// The day of the year, 0 for January 1.
    pub fn day_of_year(&self) -> u32 {
        (days_from_civil(self.year, self.month, self.day) - days_from_civil(self.year, 1, 1)) as u32
    }
}

/// `2026-11-12 02:58:42 UTC`: a time in a line for the operator.
impl fmt::Display for CivilTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }
}

/// `secs` after the epoch as a line for the operator names it.
pub fn format_utc(secs: i64) -> String {
    CivilTime::from_unix(secs).to_string()
}

/// An RFC 3339 time as seconds after the epoch: `2026-11-12T02:58:42Z`, the
/// form the image build stamps `BUILT` with (`date -u +%Y-%m-%dT%H:%M:%SZ`).
/// A fraction of a second is dropped and an offset (`+01:00`) is applied;
/// anything that is not a date and a time with a zone is `None`.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    if !text.is_ascii() || text.len() < 20 {
        return None;
    }
    let (date, rest) = text.split_at(10);
    let (sep, rest) = rest.split_at(1);
    if !matches!(sep, "T" | "t" | " ") {
        return None;
    }
    let digits = |s: &str, len: usize| -> Option<u32> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
    };
    let mut ymd = date.split('-');
    let year = i64::from(digits(ymd.next()?, 4)?);
    let month = digits(ymd.next()?, 2)?;
    let day = digits(ymd.next()?, 2)?;
    if ymd.next().is_some() || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let (time, zone) = rest.split_at(8);
    let mut hms = time.split(':');
    let hour = digits(hms.next()?, 2)?;
    let minute = digits(hms.next()?, 2)?;
    let second = digits(hms.next()?, 2)?;
    // 60 is a leap second: the second after :59, as every clock here counts it.
    if hms.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let zone = match zone.strip_prefix('.') {
        Some(fraction) => {
            let end = fraction.find(|c: char| !c.is_ascii_digit())?;
            if end == 0 {
                return None;
            }
            &fraction[end..]
        }
        None => zone,
    };
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = match zone.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h, m) = zone[1..].split_once(':')?;
            let (h, m) = (digits(h, 2)?, digits(m, 2)?);
            if h > 23 || m > 59 {
                return None;
            }
            sign * (i64::from(h) * 3_600 + i64::from(m) * 60)
        }
    };
    Some(CivilTime { year, month, day, hour, minute, second }.to_unix() - offset)
}

/// What the time fetch at boot came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Sync {
    /// A time server answered and the clock was set from it. `offset` is
    /// the correction in seconds, positive when the clock was behind.
    Synchronized { server: String, address: String, offset: f64, stratum: u8 },
    /// Nobody gave a usable answer; `reason` is a sentence for the operator.
    Failed { server: String, reason: String },
    /// `embra.ntp=off`: nobody was asked.
    Off,
}

/// What became of the hardware clock after the fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rtc {
    /// Set from the clock: the next boot starts right without a network.
    Written,
    /// Less than a second off, or no answer to set it from.
    Unchanged,
    /// The machine has no `/dev/rtc0`.
    Absent,
    Failed(String),
}

impl fmt::Display for Rtc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rtc::Written => f.write_str("written"),
            Rtc::Unchanged => f.write_str("unchanged"),
            Rtc::Absent => f.write_str("absent"),
            Rtc::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

impl Rtc {
    fn parse(text: &str) -> Rtc {
        match text {
            "written" => Rtc::Written,
            "absent" => Rtc::Absent,
            t => match t.strip_prefix("failed: ") {
                Some(e) => Rtc::Failed(e.to_string()),
                None => Rtc::Unchanged,
            },
        }
    }
}

/// The record at [`STATUS_PATH`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClockStatus {
    /// What the clock read at boot when that was earlier than the image's
    /// build time (seconds after the epoch): embrad set it to the build
    /// time then, a guess until a time server answers.
    pub floored_from: Option<i64>,
    /// The image's build time, when the release stamp gave one.
    pub built: Option<i64>,
    pub sync: Sync,
    pub rtc: Rtc,
}

impl ClockStatus {
    /// A time server set the clock this boot.
    pub fn synchronized(&self) -> bool {
        matches!(self.sync, Sync::Synchronized { .. })
    }

    /// The clock is the image's build time and nothing confirmed it: every
    /// date check runs against a clock known to be behind.
    pub fn is_a_guess(&self) -> bool {
        self.floored_from.is_some() && !self.synchronized()
    }

    /// The file's text. A reason is one line: a newline in it would start a
    /// key of its own.
    pub fn to_text(&self) -> String {
        let one_line = |s: &str| s.replace(['\n', '\r'], " ");
        let mut out = String::new();
        if let Some(was) = self.floored_from {
            out.push_str(&format!("floored_from={was}\n"));
        }
        if let Some(built) = self.built {
            out.push_str(&format!("built={built}\n"));
        }
        match &self.sync {
            Sync::Synchronized { server, address, offset, stratum } => {
                out.push_str(&format!("sync=synchronized\nserver={}\naddress={}\noffset={offset:.3}\nstratum={stratum}\n", one_line(server), one_line(address)));
            }
            Sync::Failed { server, reason } => {
                out.push_str(&format!("sync=failed\nserver={}\nreason={}\n", one_line(server), one_line(reason)));
            }
            Sync::Off => out.push_str("sync=off\n"),
        }
        out.push_str(&format!("rtc={}\n", one_line(&self.rtc.to_string())));
        out
    }

    /// `None` for a text without a `sync` line embrad writes: a foreign or
    /// half-written file says nothing about the clock.
    pub fn parse(text: &str) -> Option<ClockStatus> {
        let field = |key: &str| {
            text.lines().filter_map(|l| l.split_once('=')).filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim()).next_back()
        };
        let server = || field("server").unwrap_or("").to_string();
        let sync = match field("sync")? {
            "synchronized" => Sync::Synchronized {
                server: server(),
                address: field("address").unwrap_or("").to_string(),
                offset: field("offset").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                stratum: field("stratum").and_then(|v| v.parse().ok()).unwrap_or(0),
            },
            "failed" => Sync::Failed { server: server(), reason: field("reason").unwrap_or("").to_string() },
            "off" => Sync::Off,
            _ => return None,
        };
        Some(ClockStatus {
            floored_from: field("floored_from").and_then(|v| v.parse().ok()),
            built: field("built").and_then(|v| v.parse().ok()),
            sync,
            rtc: Rtc::parse(field("rtc").unwrap_or("")),
        })
    }

    /// `None` when the file is absent (a development host, or embrad before
    /// the fetch) or says nothing.
    pub fn read() -> Option<ClockStatus> {
        std::fs::read_to_string(STATUS_PATH).ok().and_then(|t| ClockStatus::parse(&t))
    }

    /// Written whole: a temporary file beside it, then a rename, so a
    /// reader never meets half a record.
    pub fn write(&self) -> std::io::Result<()> {
        let path = std::path::Path::new(STATUS_PATH);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, path)
    }

    /// One line for a status surface.
    pub fn summary(&self) -> String {
        let mut out = match &self.sync {
            Sync::Synchronized { server, address, offset, .. } => {
                format!("set from {server} ({address}) at boot, corrected by {offset:+.3} s")
            }
            Sync::Failed { server, reason } => format!("not confirmed: no time from {server} at boot ({reason})"),
            Sync::Off => "not confirmed: embra.ntp=off, no time server was asked".to_string(),
        };
        if let Some(was) = self.floored_from {
            let built = self.built.map(format_utc).unwrap_or_else(|| "the image's build time".into());
            if self.synchronized() {
                out.push_str(&format!("; the hardware clock read {} at boot, earlier than the image was built", format_utc(was)));
            } else {
                out.push_str(&format!("; the hardware clock read {} at boot, earlier than the image was built, so the clock is the build time, {built}, and counting", format_utc(was)));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_and_its_day_number_convert_both_ways() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // The leap days: 2000 has one, 1900 and 2100 have none.
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28), 2);
        assert_eq!(days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28), 1);
        assert_eq!(days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28), 1);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        // Every day of four centuries comes back as itself, in order.
        let mut expect = days_from_civil(1899, 12, 31);
        for year in 1900..2300 {
            for month in 1..=12 {
                for day in 1..=days_in_month(year, month) {
                    expect += 1;
                    assert_eq!(days_from_civil(year, month, day), expect, "{year}-{month}-{day}");
                    assert_eq!(civil_from_days(expect), (year, month, day));
                }
            }
        }
    }

    #[test]
    fn a_unix_time_is_a_utc_date_and_time_with_its_weekday() {
        // The two numbers of the bare-metal run of 2026-11-11: the clock the
        // board woke up with, and the first second of the gateway's certificate.
        let woke = CivilTime::from_unix(1_514_765_221);
        assert_eq!(woke.to_string(), "2018-01-01 00:07:01 UTC");
        assert_eq!(woke.weekday(), 1, "a Monday");
        assert_eq!(woke.day_of_year(), 0);
        assert_eq!(format_utc(1_793_750_400), "2026-11-04 00:00:00 UTC");
        assert_eq!(CivilTime::from_unix(1_793_750_400).weekday(), 3, "a Wednesday");
        assert_eq!(CivilTime::from_unix(0).weekday(), 4, "1970-01-01 was a Thursday");
        assert_eq!(CivilTime { year: 2024, month: 12, day: 31, hour: 0, minute: 0, second: 0 }.day_of_year(), 365, "a leap year's last day");
        assert_eq!(format_utc(-1), "1969-12-31 23:59:59 UTC");
        for secs in [0, 1, 86_399, 86_400, 951_782_400, 1_514_765_221, 1_793_750_400, 4_102_444_800, -86_401] {
            assert_eq!(CivilTime::from_unix(secs).to_unix(), secs);
        }
    }

    #[test]
    fn the_build_stamp_parses_and_what_is_not_a_time_does_not() {
        assert_eq!(parse_rfc3339("2026-11-12T02:58:42Z"), Some(1_794_452_322));
        assert_eq!(format_utc(1_794_452_322), "2026-11-12 02:58:42 UTC");
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339(" 2026-11-12T02:58:42Z\n"), Some(1_794_452_322), "the file's line end is not part of it");
        assert_eq!(parse_rfc3339("2026-11-12 02:58:42Z"), Some(1_794_452_322));
        assert_eq!(parse_rfc3339("2026-11-12T02:58:42.750Z"), Some(1_794_452_322), "a fraction is dropped");
        assert_eq!(parse_rfc3339("2026-11-12T03:58:42+01:00"), Some(1_794_452_322), "an offset is applied");
        assert_eq!(parse_rfc3339("2026-11-11T18:58:42-08:00"), Some(1_794_452_322));
        for bad in [
            "", "now", "2026-11-12", "2026-11-12T02:58:42", "2026-11-12T02:58Z", "2026-13-01T00:00:00Z", "2026-02-29T00:00:00Z",
            "2026-11-12T24:00:00Z", "2026-11-12T02:60:00Z", "2026-11-12T02:58:42.Z", "2026-11-12T02:58:42+0100", "2026-11-12X02:58:42Z",
            "26-11-12T02:58:42Z0000", "2026-11-12T02:58:42Zjunk", "2026-11-00T00:00:00Z", "２０２６-11-12T02:58:42Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z"), Some(1_709_208_000), "a leap day is a day");
    }

    #[test]
    fn the_status_file_reads_back_into_its_fields() {
        let synced = ClockStatus {
            floored_from: Some(1_514_764_803),
            built: Some(1_794_452_322),
            sync: Sync::Synchronized { server: "time.cloudflare.com".into(), address: "162.159.200.1:123".into(), offset: 279_687_519.25, stratum: 3 },
            rtc: Rtc::Written,
        };
        let text = synced.to_text();
        assert_eq!(
            text,
            "floored_from=1514764803\nbuilt=1794452322\nsync=synchronized\nserver=time.cloudflare.com\naddress=162.159.200.1:123\noffset=279687519.250\nstratum=3\nrtc=written\n"
        );
        assert_eq!(ClockStatus::parse(&text), Some(synced.clone()));
        assert!(synced.synchronized() && !synced.is_a_guess(), "an answer confirms a clock that started behind");
        assert_eq!(
            synced.summary(),
            "set from time.cloudflare.com (162.159.200.1:123) at boot, corrected by +279687519.250 s; the hardware clock read 2018-01-01 00:00:03 UTC at boot, earlier than the image was built"
        );

        let guess = ClockStatus {
            floored_from: Some(1_514_764_803),
            built: Some(1_794_452_322),
            sync: Sync::Failed { server: "time.cloudflare.com".into(), reason: "time.cloudflare.com could not be resolved:\nno answer".into() },
            rtc: Rtc::Unchanged,
        };
        let text = guess.to_text();
        assert_eq!(text.lines().count(), 6, "a reason is one line: {text}");
        let back = ClockStatus::parse(&text).unwrap();
        assert!(back.is_a_guess() && !back.synchronized());
        assert_eq!(back.sync, Sync::Failed { server: "time.cloudflare.com".into(), reason: "time.cloudflare.com could not be resolved: no answer".into() });
        assert!(back.summary().starts_with("not confirmed: no time from time.cloudflare.com at boot (time.cloudflare.com could not be resolved: no answer); the hardware clock read 2018-01-01 00:00:03 UTC"), "{}", back.summary());
        assert!(back.summary().ends_with("so the clock is the build time, 2026-11-12 02:58:42 UTC, and counting"), "{}", back.summary());

        let off = ClockStatus { floored_from: None, built: None, sync: Sync::Off, rtc: Rtc::Absent };
        assert_eq!(off.to_text(), "sync=off\nrtc=absent\n");
        assert_eq!(ClockStatus::parse("sync=off\nrtc=absent\n"), Some(off.clone()));
        assert!(!off.is_a_guess(), "a clock nobody checked is not known to be wrong");
        assert_eq!(off.summary(), "not confirmed: embra.ntp=off, no time server was asked");
        assert_eq!(ClockStatus::parse("sync=synchronized\nrtc=failed: Permission denied\n").unwrap().rtc, Rtc::Failed("Permission denied".into()));
        assert_eq!(Rtc::Failed("EIO".into()).to_string(), "failed: EIO");
        // No `sync` line, or one embrad does not write: nothing is known.
        assert_eq!(ClockStatus::parse(""), None);
        assert_eq!(ClockStatus::parse("floored_from=1\n"), None);
        assert_eq!(ClockStatus::parse("sync=maybe\n"), None);
    }
}
