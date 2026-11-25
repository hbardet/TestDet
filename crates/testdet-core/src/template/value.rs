//! Token grammar, scanning, reference extraction and stringification.
//!
//! This module owns the pieces of templating that do not touch a resolver's
//! data: the `{{namespace.name}}` grammar, the `{{` … first `}}` scanner,
//! the walk that produces [`Reference`]s, and the embedded-value stringifier.

use serde_json::Value;

/// A parsed template expression. The namespace selects the lookup table; the
/// name may itself contain dots (`session.user.email`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Env(String),
    Var(String),
    File(String),
    Session(String),
}

/// One well-formed `{{...}}` occurrence inside a JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub token: Token,
    /// The full `{{...}}` text as it appeared.
    pub raw: String,
    /// Dotted object keys with bare-number array indices, e.g.
    /// `variables.input.items.2.name`.
    pub path: String,
}

/// One malformed `{{...}}` occurrence inside a JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    /// Dotted object keys with bare-number array indices.
    pub path: String,
    /// The raw text that failed to parse: the full `{{...}}` expression, or
    /// the unterminated `{{` up to the end of the string.
    pub raw: String,
    /// Why it is malformed.
    pub reason: String,
}

/// A segment of a scanned string: either literal text or one expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    Literal(String),
    Token { raw: String, inner: String },
}

/// Parse the text between the braces (braces already stripped, whitespace not
/// trimmed).
///
/// Grammar: `namespace.name`, namespace one of `env|var|file|session`, name
/// non-empty. The namespace is everything before the FIRST `.`; later dots
/// belong to the name.
pub fn parse_token(inner: &str) -> Result<Token, String> {
    if inner.is_empty() {
        return Err("empty expression".to_string());
    }
    if inner.chars().any(char::is_whitespace) {
        return Err("whitespace is not allowed inside an expression".to_string());
    }

    let Some((namespace, name)) = inner.split_once('.') else {
        return Err(format!(
            "expected `namespace.name`, found `{inner}` without a `.`"
        ));
    };

    if namespace.is_empty() {
        return Err(format!("empty namespace in `{inner}`"));
    }
    if name.is_empty() {
        return Err(format!("empty name in `{inner}`"));
    }

    match namespace {
        "env" => Ok(Token::Env(name.to_string())),
        "var" => Ok(Token::Var(name.to_string())),
        "file" => Ok(Token::File(name.to_string())),
        "session" => Ok(Token::Session(name.to_string())),
        other => Err(format!(
            "unknown namespace `{other}`; expected one of env, var, file, session"
        )),
    }
}

/// Scan `s` for `{{` … first-following-`}}` pairs.
///
/// Text outside tokens is literal. An unterminated `{{` returns `Err(offset)`
/// where `offset` is its byte offset. A `{{` whose token closes normally is a
/// token even when preceded by another `{` (`{{{var.x}}}` yields inner
/// `{var.x`).
pub(crate) fn scan(s: &str) -> Result<Vec<Segment>, usize> {
    let mut segments = Vec::new();
    let mut cursor = 0;

    while let Some(relative) = s[cursor..].find("{{") {
        let start = cursor + relative;
        if start > cursor {
            segments.push(Segment::Literal(s[cursor..start].to_string()));
        }

        let after_open = start + 2;
        match s[after_open..].find("}}") {
            Some(close_relative) => {
                let close = after_open + close_relative;
                segments.push(Segment::Token {
                    raw: s[start..close + 2].to_string(),
                    inner: s[after_open..close].to_string(),
                });
                cursor = close + 2;
            }
            None => return Err(start),
        }
    }

    if cursor < s.len() {
        segments.push(Segment::Literal(s[cursor..].to_string()));
    }

    Ok(segments)
}

/// Collect every well-formed expression inside `value`, in document order.
///
/// `serde_json::Map` is a `BTreeMap` unless the `preserve_order` feature is
/// on (it is not), so "document order" means key-sorted order here.
/// Malformed expressions are NOT references: this function never fails, and
/// the validator reports them separately.
pub fn references(value: &Value) -> Vec<Reference> {
    let mut out = Vec::new();
    collect_references(value, "", &mut out);
    out
}

/// Collect every malformed expression inside `value`, in document order.
///
/// A malformed occurrence is either a `{{...}}` whose inner text fails
/// [`parse_token`], or an unterminated `{{` (no closing `}}`). This is the
/// complement of [`references`]: every `{{` appears in exactly one of the two.
pub fn malformed(value: &Value) -> Vec<Malformed> {
    let mut out = Vec::new();
    collect_malformed(value, "", &mut out);
    out
}

fn collect_references(value: &Value, path: &str, out: &mut Vec<Reference>) {
    match value {
        Value::String(s) => {
            let mut cursor = 0;
            while let Some(relative) = s[cursor..].find("{{") {
                let start = cursor + relative;
                let after_open = start + 2;
                let Some(close_relative) = s[after_open..].find("}}") else {
                    break;
                };
                let close = after_open + close_relative;
                let raw = &s[start..close + 2];
                let inner = &s[after_open..close];
                if let Ok(token) = parse_token(inner) {
                    out.push(Reference {
                        token,
                        raw: raw.to_string(),
                        path: path.to_string(),
                    });
                }
                cursor = close + 2;
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_references(item, &join_path(path, &index.to_string()), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_references(item, &join_path(path, key), out);
            }
        }
        _ => {}
    }
}

fn collect_malformed(value: &Value, path: &str, out: &mut Vec<Malformed>) {
    match value {
        Value::String(s) => collect_malformed_string(s, path, out),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_malformed(item, &join_path(path, &index.to_string()), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_malformed(item, &join_path(path, key), out);
            }
        }
        _ => {}
    }
}

fn collect_malformed_string(s: &str, path: &str, out: &mut Vec<Malformed>) {
    let mut cursor = 0;
    while let Some(relative) = s[cursor..].find("{{") {
        let start = cursor + relative;
        let after_open = start + 2;
        match s[after_open..].find("}}") {
            Some(close_relative) => {
                let close = after_open + close_relative;
                let raw = &s[start..close + 2];
                let inner = &s[after_open..close];
                if let Err(reason) = parse_token(inner) {
                    out.push(Malformed {
                        path: path.to_string(),
                        raw: raw.to_string(),
                        reason,
                    });
                }
                cursor = close + 2;
            }
            None => {
                out.push(Malformed {
                    path: path.to_string(),
                    raw: s[start..].to_string(),
                    reason: "unterminated expression: missing `}}`".to_string(),
                });
                break;
            }
        }
    }
}

/// Append `segment` to a dotted path, keeping a bare root key unadorned.
pub(crate) fn join_path(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_string()
    } else {
        format!("{path}.{segment}")
    }
}

/// Stringify a value for embedding in surrounding text (§7.8).
///
/// A string is its own text; numbers, booleans, objects and arrays use their
/// compact JSON text. `null` must never reach this function: callers turn a
/// null into an `E_UNKNOWN_VAR`-family diagnostic first.
pub(crate) fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

/// Levenshtein distance over characters (no new dependency).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];

    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b.len()]
}

/// The closest key to `name`: a distance ≤ 3 always beats a larger one, and
/// otherwise the nearest key wins.
pub(crate) fn closest_match<'a>(
    name: &str,
    keys: impl Iterator<Item = &'a String>,
) -> Option<&'a String> {
    let mut best: Option<(&'a String, usize)> = None;

    for key in keys {
        let distance = levenshtein(name, key);
        let replace = match best {
            None => true,
            Some((_, best_distance)) => {
                if distance <= 3 {
                    best_distance > 3 || distance < best_distance
                } else {
                    best_distance > 3 && distance < best_distance
                }
            }
        };
        if replace {
            best = Some((key, distance));
        }
    }

    best.map(|(key, _)| key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_split_and_namespace_rules() {
        assert_eq!(parse_token("var.age"), Ok(Token::Var("age".into())));
        assert_eq!(
            parse_token("session.user.email"),
            Ok(Token::Session("user.email".into()))
        );
        assert_eq!(parse_token("file.a.b"), Ok(Token::File("a.b".into())));
        assert!(parse_token("var").is_err());
        assert!(parse_token(".x").is_err());
        assert!(parse_token("var.").is_err());
        assert!(parse_token("other.x").is_err());
        assert!(parse_token("var.x y").is_err());
    }

    #[test]
    fn closest_match_prefers_a_close_hit() {
        let keys = [
            "API_TOKEN".to_string(),
            "API_TOKENS".to_string(),
            "UNRELATED".to_string(),
        ];
        // "API_TOKN" is distance 1 from API_TOKEN and 2 from API_TOKENS.
        assert_eq!(
            closest_match("API_TOKN", keys.iter()),
            Some(&"API_TOKEN".to_string())
        );
        // Nothing close: nearest regardless of distance wins.
        assert_eq!(
            closest_match("ZZZ", keys.iter()),
            Some(&"API_TOKEN".to_string())
        );
    }
}
