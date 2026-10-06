//! Small helpers over keyvalues-parser's loosely typed tree.
//!
//! Steam isn't consistent about key case, so lookups ignore it.

use keyvalues_parser::{Obj, Parser, Vdf};

/// Parse text KeyValues. Steam usually escapes backslashes, but not always
/// (Windows paths in older app info), so fall back to reading them
/// literally when the escaped parse fails.
pub fn parse(text: &str) -> Option<Vdf<'_>> {
    let partial = match Parser::new().parse(text) {
        Ok(partial) => partial,
        Err(_) => Parser::new().literal_special_chars(true).parse(text).ok()?,
    };
    Some(partial.into_vdf())
}

pub fn get_str<'a>(obj: &'a Obj, key: &str) -> Option<&'a str> {
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, values)| values.first())
        .and_then(|v| v.get_str())
}

pub fn get_obj<'a>(obj: &'a Obj<'a>, key: &str) -> Option<&'a Obj<'a>> {
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, values)| values.first())
        .and_then(|v| v.get_obj())
}

pub fn get_num<T: std::str::FromStr>(obj: &Obj, key: &str) -> Option<T> {
    get_str(obj, key).and_then(|s| s.trim().parse().ok())
}

/// Comma-separated lists like `oslist "windows,macos"`.
pub fn get_list(obj: &Obj, key: &str) -> Vec<String> {
    get_str(obj, key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}
