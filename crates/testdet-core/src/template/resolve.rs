//! The resolver: type-aware substitution over any JSON value (§7.8).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};
use crate::operation::model::FileSpec;
use crate::secrets::SecretSource;

use super::value::{Segment, Token, closest_match, join_path, parse_token, scan, stringify};

/// Resolves template expressions against four namespaces.
///
/// All maps are borrowed: the resolver never owns its inputs. `env` is a
/// `&dyn SecretSource` (the seam Task 13 implements); `files` names the
/// operation's `files:` block.
pub struct Resolver<'a> {
    pub vars: &'a BTreeMap<String, Value>,
    pub session: &'a BTreeMap<String, Value>,
    pub env: &'a dyn SecretSource,
    pub files: &'a BTreeMap<String, FileSpec>,
}

impl<'a> Resolver<'a> {
    /// Recursively resolve every string in `value` (and inside objects and
    /// arrays), collecting ALL diagnostics instead of stopping at the first.
    ///
    /// Non-string scalars pass through untouched. Diagnostics produced for a
    /// string carry the document `path` at which that string sits.
    pub fn resolve(&self, value: &Value) -> Result<Value, Diagnostics> {
        let mut diagnostics = Diagnostics::new();
        let resolved = self.resolve_inner(value, "", &mut diagnostics);
        if diagnostics.has_errors() {
            Err(diagnostics)
        } else {
            Ok(resolved)
        }
    }

    /// Resolve a single string. This is the string entry point: it has no
    /// document path of its own, so its diagnostics carry `path: None` (the
    /// caller or [`Resolver::resolve`] attaches the owner path).
    pub fn resolve_str(&self, s: &str) -> Result<Value, Diagnostics> {
        let mut diagnostics = Diagnostics::new();

        let segments = match scan(s) {
            Ok(segments) => segments,
            Err(offset) => {
                diagnostics.push(Diagnostic {
                    code: codes::E_WRONG_TYPE,
                    severity: Severity::Error,
                    file: None,
                    span: None,
                    path: None,
                    message: format!("unterminated `{{{{` at byte offset {offset} in `{s}`"),
                    hint: Some(namespace_hint()),
                });
                return Err(diagnostics);
            }
        };

        // A whole value is exactly one token with no literal text around it.
        // `"  {{var.x}}  "` has literal segments, so it is embedded.
        let whole = matches!(segments.as_slice(), [Segment::Token { .. }]);

        if whole {
            let Segment::Token { raw, inner } = &segments[0] else {
                unreachable!("guarded by `whole`");
            };
            return match parse_token(inner) {
                Ok(token) => match self.lookup(&token, raw, true) {
                    Ok(value) => Ok(value),
                    Err(diagnostic) => {
                        diagnostics.push(*diagnostic);
                        Err(diagnostics)
                    }
                },
                Err(reason) => {
                    diagnostics.push(syntax_diagnostic(raw, &reason));
                    Err(diagnostics)
                }
            };
        }

        let mut out = String::new();
        for segment in &segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Token { raw, inner } => match parse_token(inner) {
                    Ok(token) => match self.lookup(&token, raw, false) {
                        Ok(Value::Null) => {
                            diagnostics.push(null_diagnostic(&token));
                        }
                        Ok(value) => out.push_str(&stringify(&value)),
                        Err(diagnostic) => diagnostics.push(*diagnostic),
                    },
                    Err(reason) => diagnostics.push(syntax_diagnostic(raw, &reason)),
                },
            }
        }

        if diagnostics.has_errors() {
            Err(diagnostics)
        } else {
            Ok(Value::String(out))
        }
    }

    fn resolve_inner(&self, value: &Value, path: &str, diagnostics: &mut Diagnostics) -> Value {
        match value {
            Value::String(s) => match self.resolve_str(s) {
                Ok(resolved) => resolved,
                Err(inner) => {
                    for diagnostic in inner.iter() {
                        let mut diagnostic = diagnostic.clone();
                        diagnostic.path = if path.is_empty() {
                            None
                        } else {
                            Some(path.to_string())
                        };
                        diagnostics.push(diagnostic);
                    }
                    Value::String(s.clone())
                }
            },
            Value::Array(items) => {
                let resolved = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.resolve_inner(item, &join_path(path, &index.to_string()), diagnostics)
                    })
                    .collect();
                Value::Array(resolved)
            }
            Value::Object(map) => {
                let mut resolved = serde_json::Map::new();
                for (key, item) in map {
                    let child = self.resolve_inner(item, &join_path(path, key), diagnostics);
                    resolved.insert(key.clone(), child);
                }
                Value::Object(resolved)
            }
            other => other.clone(),
        }
    }

    /// Resolve one well-formed token. `whole` selects whole-value vs embedded
    /// behaviour: files are only legal whole-value, and only an embedded null
    /// is refused (a whole-value null substitutes with its type intact).
    fn lookup(&self, token: &Token, raw: &str, whole: bool) -> Result<Value, Box<Diagnostic>> {
        match token {
            Token::Var(name) => match self.vars.get(name) {
                Some(value) => Ok(value.clone()),
                None => Err(Box::new(var_miss(name, self.vars))),
            },
            Token::Session(name) => match self.session.get(name) {
                Some(value) => Ok(value.clone()),
                None => Err(Box::new(session_miss(name, self.session))),
            },
            Token::Env(name) => match self.env.lookup(name).value {
                Some(value) => Ok(Value::String(value)),
                None => Err(Box::new(env_miss(name, &self.env.names()))),
            },
            Token::File(name) => {
                if !whole {
                    return Err(Box::new(Diagnostic {
                        code: codes::E_WRONG_TYPE,
                        severity: Severity::Error,
                        file: None,
                        span: None,
                        path: None,
                        message: format!(
                            "a file reference must be the whole value, but `{raw}` is embedded \
                             in surrounding text"
                        ),
                        hint: Some(format!("use `{{{{file.{name}}}}}` alone")),
                    }));
                }
                match self.files.get(name) {
                    // Task 19 finds this token via `references()` and assembles
                    // the multipart body; the token string passes through as-is.
                    Some(_) => Ok(Value::String(raw.to_string())),
                    None => Err(Box::new(Diagnostic {
                        code: codes::E_UNKNOWN_FILE,
                        severity: Severity::Error,
                        file: None,
                        span: None,
                        path: None,
                        message: format!("unknown file `{name}`"),
                        hint: Some(file_names_hint(self.files)),
                    })),
                }
            }
        }
    }
}

/// The hint listing the four valid namespaces.
fn namespace_hint() -> String {
    "use `env.NAME`, `var.name`, `file.name` or `session.name`".to_string()
}

/// A malformed expression is a syntactic problem: `E_WRONG_TYPE`.
fn syntax_diagnostic(raw: &str, reason: &str) -> Diagnostic {
    Diagnostic {
        code: codes::E_WRONG_TYPE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("`{raw}` is not a valid expression: {reason}"),
        hint: Some(namespace_hint()),
    }
}

/// An embedded null refuses to substitute: `E_UNKNOWN_VAR` family, never an
/// empty string.
fn null_diagnostic(token: &Token) -> Diagnostic {
    Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("`{}` is null; refusing to substitute", token_name(token)),
        hint: None,
    }
}

/// The variable lookup miss, with the closest declared name.
fn var_miss(name: &str, vars: &BTreeMap<String, Value>) -> Diagnostic {
    Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("`{{{{var.{name}}}}}` does not resolve"),
        hint: closest_match(name, vars.keys()).map(|key| format!("did you mean `{key}`?")),
    }
}

/// The session lookup miss: closest captured name, or a run-order hint when
/// nothing has been captured yet.
fn session_miss(name: &str, session: &BTreeMap<String, Value>) -> Diagnostic {
    let hint = if session.is_empty() {
        Some("a capture must run before this operation".to_string())
    } else {
        closest_match(name, session.keys()).map(|key| format!("did you mean `{key}`?"))
    };
    Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("`{{{{session.{name}}}}}` does not resolve"),
        hint,
    }
}

/// The env lookup miss: name the secrets file, never the value, and suggest a
/// close name when the source can enumerate one.
fn env_miss(name: &str, names: &[String]) -> Diagnostic {
    let mut hint = format!("set `{name}` in testdet/secrets/<env>.yaml");
    if let Some(key) = closest_match(name, names.iter()) {
        hint.push_str(&format!("; did you mean `{key}`?"));
    }
    Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("secret `{name}` is not set"),
        hint: Some(hint),
    }
}

/// List the declared file names for an `E_UNKNOWN_FILE` hint.
fn file_names_hint(files: &BTreeMap<String, FileSpec>) -> String {
    if files.is_empty() {
        return "this operation declares no files".to_string();
    }
    let names = files
        .keys()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("declared files: {names}")
}

/// The bare name inside a token, for null-refusal messages.
fn token_name(token: &Token) -> &str {
    match token {
        Token::Env(name) | Token::Var(name) | Token::File(name) | Token::Session(name) => name,
    }
}
