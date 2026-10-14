//! One line per event, label first, so logs stay readable when grepped or tailed.

use std::time::{SystemTime, UNIX_EPOCH};

/// Prints `HH:MM:SS.mmm label text` with the time in UTC.
pub fn log(label: &str, text: &str) {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let day_ms = ms % 86_400_000;
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
