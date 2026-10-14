//! One line per event, label first, so logs stay readable when grepped or tailed.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Returns the wall clock in Unix milliseconds.
pub fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Prints `HH:MM:SS.mmm label text` with the time in UTC.
pub fn log(label: &str, text: &str) {
    let day_ms = unix_ms() % 86_400_000;
    let (h, m, s, milli) = (day_ms / 3_600_000, day_ms / 60_000 % 60, day_ms / 1000 % 60, day_ms % 1000);
    println!("{h:02}:{m:02}:{s:02}.{milli:03} {label:<9} {text}");
}

/// Returns an address shortened to its first and last four characters.
pub fn short(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    if chars.len() <= 12 {
        return address.to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Returns the name an enum has on the wire (its serde name), for log lines. The serde attributes
/// are the one place these names are written.
pub fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}
