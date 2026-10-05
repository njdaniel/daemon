//! Today's local date for the prompts (#101): `YYYY-MM-DD`, which `dmn-llm` puts under `## Today`
//! at the top of every fast and slow request so the model can tell how old what it remembers is.
//! `dmn-llm` never reads a clock; the suggester calls [`local`] and passes the date in.
//!
//! The workspace has no date crate, and a calendar date is not worth one: the date is the civil
//! date of the wall clock shifted by the machine's offset from UTC. The offset is read once per
//! run from `date +%z` (coreutils, so it honours `TZ` and `/etc/localtime` as every other
//! program on the machine does), on the suggester's thread before the model is warmed up, never
//! on a question's path. Should `date` be missing or say something unexpected, the offset is 0
//! and the date is UTC's, which is wrong by a day for at most a few hours around midnight; the
//! log says so. A daylight-saving change during a run moves the offset by an hour, which the
//! date only notices within that hour of midnight.

use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_S: i64 = 86_400;

/// The machine's offset from UTC in seconds, read on first use.
static OFFSET: OnceLock<i64> = OnceLock::new();

/// Today's date on this machine, `YYYY-MM-DD`.
pub fn local() -> String {
    let offset = *OFFSET.get_or_init(utc_offset);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    date(now.saturating_add(offset))
}

/// `date +%z`, in seconds east of UTC; 0 (and a warning) when it cannot be read.
fn utc_offset() -> i64 {
    let out = Command::new("date").arg("+%z").output();
    let text = out
        .as_ref()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    match parse_offset(&text) {
        Some(offset) => offset,
        None => {
            tracing::warn!(
                output = %text,
                "cannot read the UTC offset from `date +%z`; the prompts carry UTC's date"
            );
            0
        }
    }
}

/// `+hhmm` or `-hhmm` (what `date +%z` prints) in seconds.
fn parse_offset(s: &str) -> Option<i64> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

/// The civil date of `secs` seconds after 1970-01-01T00:00 (already shifted to local time).
/// Howard Hinnant's `civil_from_days`, proleptic Gregorian.
fn date(secs: i64) -> String {
    let z = secs.div_euclid(DAY_S) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_from_the_epoch_match_the_calendar() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(DAY_S - 1), "1970-01-01");
        assert_eq!(date(-1), "1969-12-31");
        // `date -u -d @1790200000 +%F`
        assert_eq!(date(1_790_200_000), "2026-09-23");
        assert_eq!(
            date(951_782_400),
            "2000-02-29",
            "a leap day in a 400-year leap year"
        );
        assert_eq!(date(4_107_542_400), "2100-03-01", "2100 is not a leap year");
    }

    #[test]
    fn the_offset_moves_the_date_across_midnight() {
        // 2026-10-05T02:30Z is still the 4th in New York (-0400) and already the 5th in Tokyo.
        let utc = 1_791_167_400;
        assert_eq!(date(utc), "2026-10-05");
        assert_eq!(date(utc + parse_offset("-0400").unwrap()), "2026-10-04");
        assert_eq!(date(utc + parse_offset("+0900").unwrap()), "2026-10-05");
    }

    #[test]
    fn offsets_parse_as_date_prints_them_and_nothing_else() {
        assert_eq!(parse_offset("+0000"), Some(0));
        assert_eq!(parse_offset("-0400"), Some(-4 * 3600));
        assert_eq!(parse_offset("+0530"), Some(5 * 3600 + 30 * 60));
        assert_eq!(parse_offset("+1345"), Some(13 * 3600 + 45 * 60));
        for bad in ["", "0400", "+04", "+04:00", "+0460", "UTC", "+2500"] {
            assert_eq!(parse_offset(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_local_date_has_the_prompt_shape() {
        let today = local();
        assert_eq!(today.len(), 10, "{today}");
        assert!(
            today.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }),
            "{today}"
        );
    }
}
