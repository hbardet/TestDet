//! Hand-rolled `.env` parser (§7.11, ruling 3 — no new dependency).
//!
//! Grammar: `KEY=VALUE` lines; `#` comments (whole-line, and trailing when
//! preceded by whitespace); optional surrounding single or double quotes
//! stripped; `\n`/`\t`/`\"`/`\\` escapes inside double quotes only; no
//! variable expansion; last definition wins; malformed lines skipped
//! silently; `\r\n` tolerated.

use std::collections::BTreeMap;
use std::path::Path;

/// Parse `.env` text into a name → value map.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = parse_line(line) {
            out.insert(key, value);
        }
    }
    out
}

/// Read and parse a dotenv file. A missing or unreadable file contributes
/// nothing (ruling 1).
pub fn parse_file(path: &Path) -> BTreeMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => BTreeMap::new(),
    }
}

/// Parse one line, returning `(key, value)` or `None` for a comment, blank or
/// malformed line. `str::lines` already strips `\r` from `\r\n` endings.
fn parse_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let eq = line.find('=')?;
    let key = line[..eq].trim_end();
    if key.is_empty() {
        return None;
    }
    let raw_value = &line[eq + 1..];
    let value = unquote(strip_comment(raw_value).trim());
    Some((key.to_string(), value))
}

/// Cut a trailing `#` comment, honouring quotes so a `#` inside single or
/// double quotes (or preceded by a non-whitespace character) is literal.
fn strip_comment(value: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut prev: Option<char> = None;

    for (index, c) in value.char_indices() {
        if escaped {
            escaped = false;
            prev = Some(c);
            continue;
        }
        if in_double {
            if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
            }
            prev = Some(c);
            continue;
        }
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            prev = Some(c);
            continue;
        }
        match c {
            '"' => in_double = true,
            '\'' => in_single = true,
            '#' if prev.is_some_and(char::is_whitespace) => {
                return &value[..index];
            }
            _ => {}
        }
        prev = Some(c);
    }
    value
}

/// Strip surrounding single or double quotes. Double-quoted values process
/// `\n`/`\t`/`\"`/`\\`; single-quoted values are verbatim.
fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let first = value.chars().next().expect("len >= 2");
        let last = value.chars().last().expect("len >= 2");
        if first == '"' && last == '"' {
            return unescape_double(&value[1..value.len() - 1]);
        }
        if first == '\'' && last == '\'' {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Resolve the four escapes defined inside double quotes. An unknown escape
/// keeps its backslash so `\x` survives as-is rather than dropping a byte.
fn unescape_double(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn basic_key_value() {
        assert_eq!(parse("A=1\nB=two\n"), map(&[("A", "1"), ("B", "two")]));
    }

    #[test]
    fn whole_line_comments_are_skipped() {
        assert_eq!(parse("# comment\nA=1\n"), map(&[("A", "1")]));
        assert_eq!(parse("   # indented comment\nA=1\n"), map(&[("A", "1")]));
    }

    #[test]
    fn trailing_comments_require_preceding_whitespace() {
        let out = parse("A=1 # comment\nB=two#not-comment\nC=three\n");
        assert_eq!(out.get("A"), Some(&"1".to_string()));
        assert_eq!(out.get("B"), Some(&"two#not-comment".to_string()));
        assert_eq!(out.get("C"), Some(&"three".to_string()));
    }

    #[test]
    fn single_quotes_are_stripped_verbatim() {
        assert_eq!(parse("A='hello'\n").get("A"), Some(&"hello".to_string()));
        // No escape processing inside single quotes.
        assert_eq!(parse(r#"A='a\nb'"#).get("A"), Some(&"a\\nb".to_string()));
    }

    #[test]
    fn double_quotes_are_stripped_and_escapes_processed() {
        let out = parse(r#"A="a\nb\t\"c\\d""#);
        assert_eq!(out.get("A"), Some(&"a\nb\t\"c\\d".to_string()));
    }

    #[test]
    fn no_variable_expansion() {
        let out = parse("A=1\nB=$A\nC=${A}\n");
        assert_eq!(out.get("B"), Some(&"$A".to_string()));
        assert_eq!(out.get("C"), Some(&"${A}".to_string()));
    }

    #[test]
    fn last_definition_wins() {
        assert_eq!(parse("A=1\nA=2\n").get("A"), Some(&"2".to_string()));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let out = parse("no-equals-sign\n=value\nA=1\n\n");
        assert_eq!(out, map(&[("A", "1")]));
    }

    #[test]
    fn crlf_is_tolerated() {
        let out = parse("A=1\r\nB=2\r\n");
        assert_eq!(out.get("A"), Some(&"1".to_string()));
        assert_eq!(out.get("B"), Some(&"2".to_string()));
    }

    #[test]
    fn hash_inside_quotes_is_literal() {
        let out = parse(r#"A="x # y" # tail"#);
        assert_eq!(out.get("A"), Some(&"x # y".to_string()));
    }
}
