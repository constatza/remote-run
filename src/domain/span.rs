//! Spans of time written as a count and one unit (`90s`, `30m`, `8h`, `2d`), shared by the options
//! that take one so they read and show them alike.

const UNITS: [(&str, u64); 4] = [("s", 1), ("m", 60), ("h", 3600), ("d", 86_400)];

/// The seconds in `text`, or `None` when it is not a count of ASCII digits followed by one of
/// `s`, `m`, `h` or `d`. The count saturates; the caller bounds the result.
pub fn parse_seconds(text: &str) -> Option<u64> {
    let (digits, unit) = UNITS
        .into_iter()
        .find_map(|(suffix, unit)| Some((text.strip_suffix(suffix)?, unit)))?;
    // digits only: `u64::from_str` would also take a leading `+`
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.parse::<u64>().ok()?.saturating_mul(unit))
}

/// `seconds` in the largest unit that divides it, so [`parse_seconds`] reads it back.
pub fn show_seconds(seconds: u64) -> String {
    match () {
        () if seconds.is_multiple_of(86_400) => format!("{}d", seconds / 86_400),
        () if seconds.is_multiple_of(3600) => format!("{}h", seconds / 3600),
        () if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
        () => format!("{seconds}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_is_digits_and_one_unit_and_reads_back() {
        for (text, seconds) in [("90s", 90), ("30m", 1800), ("8h", 28_800), ("2d", 172_800)] {
            assert_eq!(parse_seconds(text), Some(seconds), "{text}");
            assert_eq!(show_seconds(seconds), text);
        }
        for text in [
            "",
            "8",
            "h",
            "8x",
            "-1h",
            "+5m",
            "1.5h",
            "8 h",
            "8H",
            "8d1h",
            "8é",
            "8\u{661}h",
            "99999999999999999999h",
        ] {
            assert_eq!(parse_seconds(text), None, "{text:?}");
        }
        assert_eq!(parse_seconds("18446744073709551615d"), Some(u64::MAX));
    }
}
