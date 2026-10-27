//! Parses the compact human-readable values used by node and hub TOML.
//!
//! This module owns syntax conversion only; callers remain responsible for
//! policy bounds such as a node's maximum idle threshold.

use std::time::Duration;

use thiserror::Error;

use crate::HourRange;

/// A human-readable duration, byte size, or hour range could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HumanValueError {
    /// The value does not use a supported integer and unit form.
    #[error("invalid {kind} {value:?}; expected an integer with a supported unit")]
    Invalid { kind: &'static str, value: String },
    /// Multiplication to the canonical unit exceeded its integer range.
    #[error("{kind} {value:?} is too large")]
    Overflow { kind: &'static str, value: String },
}

/// Parses an integer duration with `s`, `m`, `h`, `d`, or `w` units.
///
/// Units are case-insensitive and whitespace between the number and unit is
/// accepted. Fractional values and unitless strings are rejected.
pub fn parse_duration(value: &str) -> Result<Duration, HumanValueError> {
    let (amount, unit) = integer_and_unit(value, "duration")?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return Err(invalid("duration", value)),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| overflow("duration", value))?;
    Ok(Duration::from_secs(seconds))
}

/// Parses an integer byte size with binary or decimal K/M/G/T units.
///
/// Bare prefixes and `iB` suffixes use powers of 1024; `B` suffixes use powers
/// of 1000. Decimal quantities are rejected so every accepted value has an
/// unambiguous exact byte count.
pub fn parse_bytes(value: &str) -> Result<u64, HumanValueError> {
    let (amount, unit) = integer_and_unit(value, "byte size")?;
    let normalized = unit.to_ascii_lowercase();
    let (power, base) = match normalized.as_str() {
        "b" => (0, 1_u64),
        "k" | "kib" => (1, 1024),
        "m" | "mib" => (2, 1024),
        "g" | "gib" => (3, 1024),
        "t" | "tib" => (4, 1024),
        "kb" => (1, 1000),
        "mb" => (2, 1000),
        "gb" => (3, 1000),
        "tb" => (4, 1000),
        _ => return Err(invalid("byte size", value)),
    };
    let multiplier = (0..power).try_fold(1_u64, |result, _| result.checked_mul(base));
    let multiplier = multiplier.ok_or_else(|| overflow("byte size", value))?;
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| overflow("byte size", value))
}

/// Parses a clock interval such as `22:00-07:00` into local minutes of day.
pub fn parse_hour_range(value: &str) -> Result<HourRange, HumanValueError> {
    let value = value.trim();
    let Some((start, end)) = value.split_once('-') else {
        return Err(invalid("hour range", value));
    };
    let Some(start_minutes) = parse_clock(start.trim()) else {
        return Err(invalid("hour range", value));
    };
    let Some(end_minutes) = parse_clock(end.trim()) else {
        return Err(invalid("hour range", value));
    };
    Ok(HourRange {
        start_minutes,
        end_minutes,
    })
}

fn integer_and_unit<'a>(
    value: &'a str,
    kind: &'static str,
) -> Result<(u64, &'a str), HumanValueError> {
    let value = value.trim();
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(invalid(kind, value));
    }
    let (amount, unit) = value.split_at(digits);
    let amount = amount.parse::<u64>().map_err(|_| overflow(kind, value))?;
    let unit = unit.trim();
    if unit.is_empty() {
        return Err(invalid(kind, value));
    }
    Ok((amount, unit))
}

fn parse_clock(value: &str) -> Option<u16> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 2 || byte.is_ascii_digit())
    {
        return None;
    }
    let hour = value[..2].parse::<u16>().ok()?;
    let minute = value[3..].parse::<u16>().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    hour.checked_mul(60)?.checked_add(minute)
}

fn invalid(kind: &'static str, value: &str) -> HumanValueError {
    HumanValueError::Invalid {
        kind,
        value: value.to_owned(),
    }
}

fn overflow(kind: &'static str, value: &str) -> HumanValueError {
    HumanValueError::Overflow {
        kind,
        value: value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_friendly_durations_and_optional_whitespace() {
        assert_eq!(
            parse_duration("45m").expect("duration"),
            Duration::from_secs(2700)
        );
        assert_eq!(
            parse_duration(" 2 H ").expect("duration"),
            Duration::from_secs(7200)
        );
        assert_eq!(
            parse_duration("1W").expect("duration"),
            Duration::from_secs(604_800)
        );
    }

    #[test]
    fn rejects_invalid_and_overflowing_durations() {
        assert!(parse_duration("1.5h").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("18446744073709551615w").is_err());
    }

    #[test]
    fn parses_binary_and_decimal_byte_units_exactly() {
        assert_eq!(parse_bytes("12G").expect("bytes"), 12 * 1024_u64.pow(3));
        assert_eq!(parse_bytes("1 GiB").expect("bytes"), 1024_u64.pow(3));
        assert_eq!(parse_bytes("2MB").expect("bytes"), 2_000_000);
        assert_eq!(parse_bytes("8B").expect("bytes"), 8);
    }

    #[test]
    fn rejects_ambiguous_and_overflowing_byte_units() {
        assert!(parse_bytes("12").is_err());
        assert!(parse_bytes("1.5G").is_err());
        assert!(parse_bytes("18446744073709551615T").is_err());
        assert!(parse_bytes("2PB").is_err());
    }

    #[test]
    fn parses_local_hour_ranges_and_rejects_invalid_clocks() {
        assert_eq!(
            parse_hour_range("22:00-07:00").expect("hours"),
            HourRange {
                start_minutes: 1320,
                end_minutes: 420,
            }
        );
        assert!(parse_hour_range("24:00-07:00").is_err());
        assert!(parse_hour_range("22:00").is_err());
    }
}
