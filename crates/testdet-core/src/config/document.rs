//! Span-preserving YAML document loader.
//!
//! Parses a config file once with [`saphyr`], projects the untyped document
//! into [`serde_json::Value`], and keeps a `path -> Span` side table so that
//! later tasks can point a diagnostic at the exact source location of any
//! node. Paths are dot-separated: mapping keys and sequence indices are both
//! joined with `.`, so `headers.Authorization` names a mapping field and
//! `collections.0` names the first element of a sequence. The root node's
//! path is the empty string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, YamlData};

use crate::diagnostic::{Diagnostic, Diagnostics, Severity, Span, codes};

/// An untyped YAML document with source spans for every node.
#[derive(Debug, Clone)]
pub struct Document {
    path: PathBuf,
    root: serde_json::Value,
    spans: BTreeMap<String, Span>,
}

impl Document {
    /// Load and project the YAML file at `path`.
    ///
    /// The top level must be a mapping. Parse failures, duplicate keys, a
    /// non-mapping top level, YAML anchors/aliases and YAML merge keys are all
    /// reported as [`Diagnostics`] errors.
    pub fn load(path: &Path) -> Result<Document, Diagnostics> {
        let source = std::fs::read_to_string(path).map_err(|error| {
            Diagnostics::from(vec![Diagnostic {
                code: codes::E_UNKNOWN_FILE,
                severity: Severity::Error,
                file: Some(path.to_path_buf()),
                span: None,
                path: None,
                message: format!("cannot read config file: {error}"),
                hint: None,
            }])
        })?;

        // `saphyr` expands anchors and aliases before projecting the tree, so
        // they are invisible to the walk below. Reject them from the event
        // stream before building anything.
        let pre_scan = scan_anchors_and_aliases(&source, path);
        if pre_scan.has_errors() {
            return Err(pre_scan);
        }

        let docs = MarkedYaml::load_from_str(&source).map_err(|error| {
            let span = span_of(error.marker());
            // saphyr reports a duplicate mapping key with this exact info
            // string; every other scan failure is a malformed document.
            let code = if error.info().contains("duplicated key") {
                codes::E_DUPLICATE_KEY
            } else {
                codes::E_WRONG_TYPE
            };
            Diagnostics::from(vec![Diagnostic {
                code,
                severity: Severity::Error,
                file: Some(path.to_path_buf()),
                span: Some(span),
                path: None,
                message: format!("invalid YAML: {}", error.info()),
                hint: None,
            }])
        })?;

        if docs.is_empty() {
            return Err(non_mapping_error(
                path,
                Span { line: 1, column: 1 },
                "expected a YAML document, found an empty file",
            ));
        }
        if docs.len() > 1 {
            return Err(non_mapping_error(
                path,
                Span { line: 1, column: 1 },
                &format!("expected a single YAML document, found {}", docs.len()),
            ));
        }

        let root_node = &docs[0];
        if !root_node.data.is_mapping() {
            let found = match &root_node.data {
                YamlData::Sequence(_) => "a sequence",
                _ => "a scalar",
            };
            return Err(non_mapping_error(
                path,
                span_of(&root_node.span.start),
                &format!("expected a mapping at the top level, found {found}"),
            ));
        }

        let mut spans = BTreeMap::new();
        spans.insert(String::new(), span_of(&root_node.span.start));

        let mut diags = Diagnostics::new();
        let root = build(root_node, "", path, &mut spans, &mut diags);
        if diags.has_errors() {
            return Err(diags);
        }

        Ok(Document {
            path: path.to_path_buf(),
            root,
            spans,
        })
    }

    /// The filesystem path this document was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The untyped document projected into JSON.
    pub fn root(&self) -> &serde_json::Value {
        &self.root
    }

    /// The source span of the node at the given dotted path, or `None` when
    /// the path is unknown. The empty string names the root node.
    pub fn span_at(&self, path: &str) -> Option<Span> {
        self.spans.get(path).copied()
    }
}

/// Convert a saphyr marker (1-based line, 0-based column) into our 1-based
/// [`Span`].
fn span_of(marker: &saphyr::Marker) -> Span {
    Span {
        line: marker.line() as u32,
        column: marker.col() as u32 + 1,
    }
}

fn non_mapping_error(path: &Path, span: Span, message: &str) -> Diagnostics {
    Diagnostics::from(vec![Diagnostic {
        code: codes::E_WRONG_TYPE,
        severity: Severity::Error,
        file: Some(path.to_path_buf()),
        span: Some(span),
        path: None,
        message: message.to_string(),
        hint: None,
    }])
}

/// Reject YAML anchors and aliases by walking the parser event stream once.
///
/// `saphyr` transparently resolves both when it projects the tree, so neither
/// is ever visible to the tree walk. The events are the only place they
/// surface: an anchor appears as a non-zero `anchor_id` on a scalar or
/// collection start, and an alias as [`Event::Alias`]. Merge keys are left to
/// the tree walk, which already rejects a literal `<<` key as an unknown
/// field. Every offending construct is reported, not just the first.
fn scan_anchors_and_aliases(source: &str, path: &Path) -> Diagnostics {
    let mut diags = Diagnostics::new();
    let mut parser = saphyr_parser::Parser::new_from_str(source);
    while let Some(result) = parser.next_event() {
        let Ok((event, span)) = result else {
            // A scan error is reported by the tree walk's own loader; stop the
            // pre-scan here rather than duplicate it.
            break;
        };
        let span = span_of(&span.start);
        match event {
            saphyr_parser::Event::Alias(_) => diags.push(unsupported_construct(
                path,
                span,
                "YAML aliases (`*name`) are not supported",
            )),
            saphyr_parser::Event::Scalar(_, _, anchor_id, _)
            | saphyr_parser::Event::SequenceStart(anchor_id, _)
            | saphyr_parser::Event::MappingStart(anchor_id, _)
                if anchor_id != 0 =>
            {
                diags.push(unsupported_construct(
                    path,
                    span,
                    "YAML anchors (`&name`) are not supported",
                ));
            }
            _ => {}
        }
    }
    diags
}

fn unsupported_construct(path: &Path, span: Span, message: &str) -> Diagnostic {
    Diagnostic {
        code: codes::E_UNKNOWN_FIELD,
        severity: Severity::Error,
        file: Some(path.to_path_buf()),
        span: Some(span),
        path: None,
        message: message.to_string(),
        hint: None,
    }
}

/// Project a saphyr node into JSON while recording spans for every child path.
///
/// A mapping field's recorded span points at the field's *key*; a sequence
/// element's recorded span points at the element node. Scalar leaves do not
/// overwrite the span of the field that names them, so `span_at` on a scalar
/// field yields the key's location.
fn build(
    node: &MarkedYaml,
    path: &str,
    file: &Path,
    spans: &mut BTreeMap<String, Span>,
    diags: &mut Diagnostics,
) -> serde_json::Value {
    match &node.data {
        YamlData::Value(scalar) => scalar_to_json(scalar),
        YamlData::Sequence(items) => {
            let mut array = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let child = join(path, &index.to_string());
                spans.insert(child.clone(), span_of(&item.span.start));
                array.push(build(item, &child, file, spans, diags));
            }
            serde_json::Value::Array(array)
        }
        YamlData::Mapping(mapping) => {
            let mut object = serde_json::Map::new();
            for (key, value) in mapping {
                let Some(key_str) = key_string(key) else {
                    // Complex (non-scalar) keys have no JSON object key and no
                    // dotted-path name; skip them rather than lose the value.
                    continue;
                };
                let child = join(path, &key_str);
                if key_str == "<<" {
                    diags.push(Diagnostic {
                        code: codes::E_UNKNOWN_FIELD,
                        severity: Severity::Error,
                        file: Some(file.to_path_buf()),
                        span: Some(span_of(&key.span.start)),
                        path: Some(child),
                        message: "YAML merge keys (`<<`) are not supported".to_string(),
                        hint: None,
                    });
                    continue;
                }
                spans.insert(child.clone(), span_of(&key.span.start));
                object.insert(key_str, build(value, &child, file, spans, diags));
            }
            serde_json::Value::Object(object)
        }
        YamlData::Tagged(_, inner) => build(inner, path, file, spans, diags),
        YamlData::Representation(raw, _, _) => serde_json::Value::String(raw.to_string()),
        // Aliases are transparently expanded by saphyr before we see the tree,
        // and BadValue means a scalar could not be resolved; neither is
        // reachable through the default loader in normal operation.
        YamlData::Alias(_) | YamlData::BadValue => serde_json::Value::Null,
    }
}

/// The JSON object key for a scalar mapping key, or `None` for a complex key.
fn key_string(key: &MarkedYaml) -> Option<String> {
    match &key.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        YamlData::Value(Scalar::Integer(i)) => Some(i.to_string()),
        YamlData::Value(Scalar::Boolean(b)) => Some(b.to_string()),
        YamlData::Value(Scalar::FloatingPoint(f)) => Some(f.0.to_string()),
        YamlData::Value(Scalar::Null) => Some("null".to_string()),
        YamlData::Representation(raw, _, _) => Some(raw.to_string()),
        _ => None,
    }
}

/// Project a YAML scalar into a JSON value, preserving its resolved kind:
/// integers stay integers (never `f64`), YAML 1.2 booleans (`true`/`false`)
/// become JSON booleans, `null`/`~` become JSON null, and everything else
/// (including `NO` and unquoted dates) stays a string.
fn scalar_to_json(scalar: &Scalar) -> serde_json::Value {
    match scalar {
        Scalar::Null => serde_json::Value::Null,
        Scalar::Boolean(b) => serde_json::Value::Bool(*b),
        Scalar::Integer(i) => serde_json::Value::Number((*i).into()),
        Scalar::FloatingPoint(f) => {
            let value = f.0;
            match serde_json::Number::from_f64(value) {
                Some(number) => serde_json::Value::Number(number),
                // `.inf`/`.nan` have no JSON number form; keep their text.
                None => serde_json::Value::String(
                    if value.is_nan() {
                        ".nan"
                    } else if value.is_sign_negative() {
                        "-.inf"
                    } else {
                        ".inf"
                    }
                    .to_string(),
                ),
            }
        }
        Scalar::String(s) => serde_json::Value::String(s.to_string()),
    }
}

fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}
