//! Small helpers shared by the drivers.

use deltos_data::{Result, Row, RowBatch, Value};
use futures::stream::BoxStream;
use tokio::sync::mpsc;

/// The largest batch any driver reads at once, whatever the caller asks for, so one batch
/// can never grow past a sensible size.
pub(crate) const MAX_BATCH: usize = 50_000;

/// The batch size to use for a request, at least one row and at most [`MAX_BATCH`].
pub(crate) fn batch_size(requested: u32) -> usize {
    (requested as usize).clamp(1, MAX_BATCH)
}

/// A stream over batches sent by a producer task. The channel's capacity is the read
/// ahead: the producer waits for room before it reads more from the server.
pub(crate) fn channel_stream(
    receiver: mpsc::Receiver<Result<RowBatch>>,
) -> BoxStream<'static, Result<RowBatch>> {
    Box::pin(futures::stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|batch| (batch, receiver)) },
    ))
}

pub(crate) fn row(values: Vec<Value>) -> Row {
    values.into_boxed_slice()
}

/// Bytes as lowercase hex, appended to `out`.
pub(crate) fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    out.reserve(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
}

/// Parses hex digits, with or without a `0x` or `\x` prefix.
pub(crate) fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("\\x"))
        .unwrap_or(text)
        .as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    digits
        .chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

/// A value as text for a parameter sent in text form, or `None` for NULL.
pub(crate) fn value_text(value: &Value) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        Value::Int(i) => Some(Cow::Owned(i.to_string())),
        Value::Float(f) => Some(Cow::Owned(float_text(*f))),
        Value::Text(s) | Value::Json(s) => Some(Cow::Borrowed(s)),
        Value::Bytes(b) => {
            let mut out = String::with_capacity(2 + b.len() * 2);
            out.push_str("\\x");
            push_hex(&mut out, b);
            Some(Cow::Owned(out))
        }
    }
}

/// A float the way SQL servers read it back, including the special values.
pub(crate) fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        f.to_string()
    }
}

/// Text read as UTF-8, replacing invalid sequences rather than failing a whole row.
pub(crate) fn text_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::Text(s.into()),
        Err(_) => Value::Text(String::from_utf8_lossy(bytes).into()),
    }
}

/// Days since 1970-01-01 as a proleptic Gregorian (year, month, day). From Howard
/// Hinnant's `civil_from_days`, which is exact over the whole i64 range we feed it.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `HH:MM:SS` with `.ffffff` when there are microseconds, trailing zeros removed.
pub(crate) fn push_time(out: &mut String, hours: u64, minutes: u32, seconds: u32, micros: u32) {
    use std::fmt::Write;
    let _ = write!(out, "{hours:02}:{minutes:02}:{seconds:02}");
    if micros > 0 {
        let fraction = format!("{micros:06}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let mut out = String::new();
        push_hex(&mut out, &[0, 1, 0xab, 0xff]);
        assert_eq!(out, "0001abff");
        assert_eq!(parse_hex("\\x0001abff"), Some(vec![0, 1, 0xab, 0xff]));
        assert_eq!(parse_hex("0xAB"), Some(vec![0xab]));
        assert_eq!(parse_hex("abc"), None);
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
    }

    #[test]
    fn times() {
        let mut out = String::new();
        push_time(&mut out, 1, 2, 3, 450_000);
        assert_eq!(out, "01:02:03.45");
        out.clear();
        push_time(&mut out, 25, 0, 0, 0);
        assert_eq!(out, "25:00:00");
    }

    #[test]
    fn floats_read_back() {
        assert_eq!(float_text(f64::NAN), "NaN");
        assert_eq!(float_text(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(float_text(0.1), "0.1");
    }
}
