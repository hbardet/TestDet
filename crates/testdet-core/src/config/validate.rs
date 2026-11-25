//! Schema-driven validation of untyped config documents.
//!
//! Runs the derived JSON Schema (Task 5) over an untyped [`Document`] (Task 4)
//! and reports every problem at once, then adds the one check the schema
//! cannot express: `version` must be the integer `1` (§7.5), which is checked
//! by hand before the schema pass because the `Version` newtype derives as a
//! plain `integer` in JSON Schema.
//!
//! The public entry point is [`validate_document`]; [`map_error`] is the
//! error→diagnostic mapping used by it (and by future callers that want to
//! translate a single schema error). Every message names the offending value
//! and the legal alternative; nothing here prints and nothing here decides an
//! exit code.

use std::collections::BTreeSet;

use jsonschema::ValidationError;
use jsonschema::error::ValidationErrorKind;
use jsonschema::paths::{Location, LocationSegment};
use schemars::Schema;
use serde_json::Value;

use crate::config::Document;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity, codes};

/// Which schema a document is validated against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Manifest,
    Operation,
    Users,
    Secrets,
}

/// Validate `doc` against `schema`, reporting every problem found.
///
/// For a manifest, `version` is checked by hand first (§7.5): missing → `E_VERSION`,
/// present but not the integer `1` → `E_VERSION`. The schema pass then reports the
/// remaining problems. Output is sorted by `(file, line, column)` with absent
/// locations first, so downstream snapshots are stable.
pub fn validate_document(doc: &Document, schema: &Schema, kind: SchemaKind) -> Diagnostics {
    let mut diagnostics = Vec::new();

    if kind == SchemaKind::Manifest {
        check_version(doc, &mut diagnostics);
    }

    let schema_value = match serde_json::to_value(schema) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: Some(doc.path().to_path_buf()),
                span: None,
                path: None,
                message: format!("cannot serialise validation schema: {error}"),
                hint: None,
            });
            return Diagnostics::from(diagnostics);
        }
    };

    match jsonschema::validator_for(&schema_value) {
        Ok(validator) => {
            for error in validator.iter_errors(doc.root()) {
                diagnostics.extend(map_one_error(&error, doc, &schema_value, kind));
            }
        }
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file: Some(doc.path().to_path_buf()),
                span: None,
                path: None,
                message: format!("cannot build validation schema: {error}"),
                hint: None,
            });
        }
    }

    sort_diagnostics(&mut diagnostics);
    Diagnostics::from(diagnostics)
}

/// Translate a single schema error into a [`Diagnostic`].
///
/// The instance path of the error is rebuilt into the dotted form understood by
/// [`Document::span_at`]. `schema` (the serialised schema, with `$ref`s resolved
/// against it) is needed to name the legal alternatives for a suggestion hint
/// and the expected `kind`s for a failed tagged union.
pub fn map_error(error: &ValidationError<'_>, doc: &Document, schema: &Value) -> Diagnostic {
    let path = dotted_path(error.instance_path());
    let span = doc.span_at(&path);
    let file = Some(doc.path().to_path_buf());

    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            // The caller splits this case per key; if reached directly, map the
            // first unexpected key.
            match unexpected.first() {
                Some(key) => unknown_field_diagnostic(error, key, doc, schema),
                None => fallback_diagnostic(error, doc, &path, span),
            }
        }
        ValidationErrorKind::Required { property } => {
            let missing = property
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| property.to_string());
            let missing_path = join_path(&path, &missing);
            Diagnostic {
                code: codes::E_MISSING_FIELD,
                severity: Severity::Error,
                file,
                span: doc.span_at(&missing_path),
                path: Some(missing_path),
                message: format!("missing required field `{missing}`"),
                hint: None,
            }
        }
        ValidationErrorKind::Type { .. } => Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file,
            span,
            path: Some(path),
            message: format!("{error}"),
            hint: None,
        },
        ValidationErrorKind::Constant { .. } => Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file,
            span,
            path: Some(path),
            message: format!("{error}"),
            hint: None,
        },
        ValidationErrorKind::Enum { .. } => Diagnostic {
            code: codes::E_WRONG_TYPE,
            severity: Severity::Error,
            file,
            span,
            path: Some(path),
            message: format!("{error}"),
            hint: None,
        },
        ValidationErrorKind::AnyOf { .. }
        | ValidationErrorKind::OneOfNotValid { .. }
        | ValidationErrorKind::OneOfMultipleValid { .. } => {
            let kinds = expected_kinds(error, schema);
            let message = if kinds.is_empty() {
                format!("{error}")
            } else {
                let list = kinds
                    .iter()
                    .map(|kind| format!("`{kind}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                match error.kind() {
                    ValidationErrorKind::OneOfMultipleValid { .. } => {
                        format!("matches more than one of: {list}")
                    }
                    _ => format!("expected one of: {list}"),
                }
            };
            Diagnostic {
                code: codes::E_WRONG_TYPE,
                severity: Severity::Error,
                file,
                span,
                path: Some(path),
                message,
                hint: None,
            }
        }
        _ => fallback_diagnostic(error, doc, &path, span),
    }
}

/// Map one top-level schema error to zero or more diagnostics: split
/// `additionalProperties` per unknown key, and expand a failed tagged union
/// per branch.
fn map_one_error(
    error: &ValidationError<'_>,
    doc: &Document,
    schema: &Value,
    kind: SchemaKind,
) -> Vec<Diagnostic> {
    match error.kind() {
        // `additionalProperties` bundles every unexpected key of one object
        // into a single error; expand it so each unknown key gets its own
        // diagnostic and its own span.
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            let mut out = Vec::with_capacity(unexpected.len());
            for key in unexpected {
                out.push(unknown_field_diagnostic(error, key, doc, schema));
            }
            out
        }
        // The hand check above owns a missing `version` for a manifest
        // (reported as E_VERSION); the schema's `required` must not
        // double-report it as E_MISSING_FIELD.
        ValidationErrorKind::Required { property }
            if kind == SchemaKind::Manifest && property.as_str() == Some("version") =>
        {
            Vec::new()
        }
        // A tagged union (`request`, `body`) collapses to one error; expand it
        // into the single branch named by the instance's `kind` discriminator.
        ValidationErrorKind::AnyOf { .. } | ValidationErrorKind::OneOfNotValid { .. } => {
            map_tagged_union(error, doc, schema, kind)
        }
        _ => vec![map_error(error, doc, schema)],
    }
}

/// Expand a failed tagged union into the errors of the single branch named by
/// the instance's `kind` discriminator, by mapping the matching branch's
/// sub-errors from the error's own `context` (which carries full schema paths).
///
/// When `kind` is absent, the collapsed diagnostic explains the missing
/// discriminator; when `kind` is unknown, the collapsed diagnostic names the
/// legal alternatives. Either way the collapse survives.
fn map_tagged_union(
    error: &ValidationError<'_>,
    doc: &Document,
    schema: &Value,
    kind: SchemaKind,
) -> Vec<Diagnostic> {
    let Some(branches) = union_branches(error, schema) else {
        return vec![map_error(error, doc, schema)];
    };

    // A tagged union has a `kind` discriminator on at least one branch; a plain
    // `anyOf` (e.g. `struct-or-null`, like `transport: [Transport, null]`) does
    // not. The two shapes need different handling.
    let tagged = branches
        .iter()
        .any(|branch| branch_has_kind(branch, schema));

    let instance = instance_at(doc.root(), error.instance_path());
    let discriminator = instance
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str);

    match (tagged, discriminator) {
        // Tagged union with a known `kind`: map only the matching branch.
        (true, Some(discriminator)) => {
            let Some(branch_index) = branches
                .iter()
                .position(|branch| branch_accepts_kind(branch, schema, discriminator))
            else {
                return vec![map_error(error, doc, schema)];
            };
            map_branch_errors(error, doc, schema, kind, branch_index)
        }
        // Tagged union without `kind`: report the missing discriminator.
        (true, None) => vec![missing_discriminator_diagnostic(error, doc, schema)],
        // Plain `anyOf`: surface the specific sub-errors (e.g. the struct
        // branch's `additionalProperties`), dropping the collapse-level `type`
        // mismatches produced by the `null` branch.
        (false, _) => map_all_branch_errors(error, doc, schema, kind),
    }
}

/// The oneOf/anyOf error's `context` holds the sub-errors for each branch
/// (context[i] for branch i), each with a full `schema_path` into `schema`.
/// Map only `branch_index`'s sub-errors so the paths/codes are specific.
fn map_branch_errors(
    error: &ValidationError<'_>,
    doc: &Document,
    schema: &Value,
    kind: SchemaKind,
    branch_index: usize,
) -> Vec<Diagnostic> {
    let branch_errors = match error.kind() {
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            context.get(branch_index)
        }
        _ => None,
    };
    let Some(branch_errors) = branch_errors else {
        return vec![map_error(error, doc, schema)];
    };

    let mut out = Vec::new();
    for sub in branch_errors {
        out.extend(map_one_error(sub, doc, schema, kind));
    }
    if out.is_empty() {
        vec![map_error(error, doc, schema)]
    } else {
        out
    }
}

/// Map the sub-errors of every branch of a plain (non-tagged) `anyOf`. The
/// `null`/type branch yields a collapse-level `E_WRONG_TYPE` at the instance
/// path (e.g. "not null"); drop those when a deeper, actionable diagnostic
/// exists (e.g. the struct branch's `additionalProperties`).
fn map_all_branch_errors(
    error: &ValidationError<'_>,
    doc: &Document,
    schema: &Value,
    kind: SchemaKind,
) -> Vec<Diagnostic> {
    let context = match error.kind() {
        ValidationErrorKind::AnyOf { context } | ValidationErrorKind::OneOfNotValid { context } => {
            context
        }
        _ => return vec![map_error(error, doc, schema)],
    };

    let mut out = Vec::new();
    for branch_errors in context {
        for sub in branch_errors {
            out.extend(map_one_error(sub, doc, schema, kind));
        }
    }
    if out.is_empty() {
        return vec![map_error(error, doc, schema)];
    }

    let union_path = dotted_path(error.instance_path());
    let has_specific = out
        .iter()
        .any(|diagnostic| diagnostic.path.as_deref() != Some(union_path.as_str()));
    if has_specific {
        out.retain(|diagnostic| diagnostic.path.as_deref() != Some(union_path.as_str()));
    }
    out
}

/// The collapsed diagnostic for a tagged union whose `kind` discriminator is
/// missing entirely.
fn missing_discriminator_diagnostic(
    error: &ValidationError<'_>,
    doc: &Document,
    schema: &Value,
) -> Diagnostic {
    let path = dotted_path(error.instance_path());
    let kinds = expected_kinds(error, schema);
    let message = if kinds.is_empty() {
        format!("{error}")
    } else {
        let list = kinds
            .iter()
            .map(|kind| format!("`{kind}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("missing discriminator `kind`; expected one of: {list}")
    };
    Diagnostic {
        code: codes::E_WRONG_TYPE,
        severity: Severity::Error,
        file: Some(doc.path().to_path_buf()),
        span: doc.span_at(&path),
        path: Some(path),
        message,
        hint: None,
    }
}

/// The value at `location` within `root`, walking object properties and array
/// indices.
fn instance_at<'a>(root: &'a Value, location: &Location) -> Option<&'a Value> {
    let mut node = root;
    for segment in location.segments() {
        node = match segment {
            LocationSegment::Property(property) => node.get(property.as_ref())?,
            LocationSegment::Index(index) => node.get(index)?,
        };
    }
    Some(node)
}

/// The `oneOf`/`anyOf` branch array for the failing keyword. The schema path
/// points either at the array itself (`/definitions/Request/oneOf`) or at the
/// object carrying the keyword, so both shapes are handled.
fn union_branches<'a>(error: &ValidationError<'_>, schema: &'a Value) -> Option<&'a [Value]> {
    let node = json_pointer(schema, error.schema_path().as_str())?;
    if let Some(branches) = node.as_array() {
        return Some(branches.as_slice());
    }
    node.get("oneOf")
        .or_else(|| node.get("anyOf"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
}

/// Whether a tagged-union branch accepts `kind`, reading
/// `properties.kind.const` (string) or `properties.kind.enum`. Resolves `$ref`
/// and recurses through nested `oneOf`/`anyOf` (e.g. `body: anyOf [Body, null]`).
fn branch_accepts_kind(branch: &Value, schema: &Value, kind: &str) -> bool {
    let node = resolve(schema, branch);
    if let Some(kind_schema) = node
        .get("properties")
        .and_then(|properties| properties.get("kind"))
    {
        if let Some(Value::String(value)) = kind_schema.get("const") {
            return value == kind;
        }
        if let Some(values) = kind_schema.get("enum").and_then(Value::as_array) {
            return values.iter().any(|value| value.as_str() == Some(kind));
        }
    }
    // A branch may be a `$ref` to a definition that is itself a tagged union
    // (e.g. `body: anyOf [{$ref: #/definitions/Body}, {type: null}]`).
    for keyword in ["oneOf", "anyOf"] {
        if let Some(sub_branches) = node.get(keyword).and_then(Value::as_array)
            && sub_branches
                .iter()
                .any(|sub| branch_accepts_kind(sub, schema, kind))
        {
            return true;
        }
    }
    false
}

/// Whether a branch (after `$ref` resolution) carries a `kind` discriminator,
/// directly or through nested `oneOf`/`anyOf`. Distinguishes tagged unions from
/// plain `anyOf` (e.g. `struct-or-null`, like `transport: [Transport, null]`).
fn branch_has_kind(branch: &Value, schema: &Value) -> bool {
    let node = resolve(schema, branch);
    if let Some(kind_schema) = node
        .get("properties")
        .and_then(|properties| properties.get("kind"))
        && (kind_schema.get("const").is_some() || kind_schema.get("enum").is_some())
    {
        return true;
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(sub_branches) = node.get(keyword).and_then(Value::as_array)
            && sub_branches.iter().any(|sub| branch_has_kind(sub, schema))
        {
            return true;
        }
    }
    false
}

/// The `version: 1` check, run first for a manifest (§7.5).
fn check_version(doc: &Document, out: &mut Vec<Diagnostic>) {
    let file = Some(doc.path().to_path_buf());
    match doc.root().get("version") {
        None => out.push(Diagnostic {
            code: codes::E_VERSION,
            severity: Severity::Error,
            file,
            span: doc.span_at("version"),
            path: Some("version".to_string()),
            message: "missing required `version`".to_string(),
            hint: Some("add `version: 1`".to_string()),
        }),
        Some(value) if !is_version_one(value) => out.push(Diagnostic {
            code: codes::E_VERSION,
            severity: Severity::Error,
            file,
            span: doc.span_at("version"),
            path: Some("version".to_string()),
            message: format!("version must be 1, found {value}"),
            hint: None,
        }),
        _ => {}
    }
}

/// True when the JSON value is the integer `1` (a `1.0` float does not count).
fn is_version_one(value: &Value) -> bool {
    value.as_i64() == Some(1) || value.as_u64() == Some(1)
}

/// Build the `E_UNKNOWN_FIELD` diagnostic for one unexpected key of an object.
fn unknown_field_diagnostic(
    error: &ValidationError<'_>,
    key: &str,
    doc: &Document,
    schema: &Value,
) -> Diagnostic {
    let parent = dotted_path(error.instance_path());
    let path = join_path(&parent, key);
    Diagnostic {
        code: codes::E_UNKNOWN_FIELD,
        severity: Severity::Error,
        file: Some(doc.path().to_path_buf()),
        span: doc.span_at(&path),
        path: Some(path),
        message: format!("unknown field `{key}`"),
        hint: closest_sibling_hint(&parent, key, schema),
    }
}

/// The catch-all for keywords without a specific mapping: the raw keyword is
/// named in the message.
fn fallback_diagnostic(
    error: &ValidationError<'_>,
    doc: &Document,
    path: &str,
    span: Option<crate::diagnostic::Span>,
) -> Diagnostic {
    Diagnostic {
        code: codes::E_WRONG_TYPE,
        severity: Severity::Error,
        file: Some(doc.path().to_path_buf()),
        span,
        path: Some(path.to_string()),
        message: format!("{}: {error}", error.kind().keyword()),
        hint: None,
    }
}

/// `` did you mean `{parent}.{closest}`? `` — the nearest legal sibling of the
/// unknown key by Levenshtein distance, qualified by the owning object's dotted
/// path (omitted when the owner is the document root).
fn closest_sibling_hint(parent: &str, unknown: &str, schema: &Value) -> Option<String> {
    let closest = legal_properties(schema, parent)
        .into_iter()
        .min_by_key(|candidate| levenshtein(unknown, candidate))?;
    if parent.is_empty() {
        Some(format!("did you mean `{closest}`?"))
    } else {
        Some(format!("did you mean `{parent}.{closest}`?"))
    }
}

/// The legal property names of the object at dotted `parent` path, resolved
/// through `$ref`/`allOf`/`anyOf`/`oneOf` wrappers. Empty when the path names
/// a map (arbitrary keys) or is unknown.
fn legal_properties(schema: &Value, parent: &str) -> Vec<String> {
    let segments: Vec<String> = parent
        .split('.')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    let Some(node) = navigate(schema, &segments) else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    collect_property_names(schema, node, &mut names);
    names.into_iter().collect()
}

/// The expected `kind` values for a failed tagged union, read from the schema
/// node the failing `oneOf`/`anyOf` keyword sat at (which may be the array
/// itself). Empty when the union has no per-variant `kind` const (e.g. a plain
/// `anyOf` of a struct-or-null).
fn expected_kinds(error: &ValidationError<'_>, schema: &Value) -> Vec<String> {
    let Some(node) = json_pointer(schema, error.schema_path().as_str()) else {
        return Vec::new();
    };
    let branches = if let Some(branches) = node.as_array() {
        branches
    } else {
        let Some(branches) = node
            .get("oneOf")
            .or_else(|| node.get("anyOf"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        branches
    };
    let mut kinds = Vec::new();
    for branch in branches {
        let kind_schema = branch
            .get("properties")
            .and_then(|properties| properties.get("kind"));
        // Internally tagged union: `properties.kind.const` or `enum`.
        if let Some(kind) = kind_schema
            .and_then(|kind| kind.get("const"))
            .and_then(Value::as_str)
        {
            kinds.push(kind.to_string());
            continue;
        }
        if let Some(values) = kind_schema
            .and_then(|kind| kind.get("enum"))
            .and_then(Value::as_array)
        {
            for value in values {
                if let Some(kind) = value.as_str() {
                    kinds.push(kind.to_string());
                }
            }
            continue;
        }
        // Simple enum: `const` on the variant itself.
        if let Some(kind) = branch.get("const").and_then(Value::as_str) {
            kinds.push(kind.to_string());
        }
    }
    kinds
}

/// Navigate a schema node to the object at `segments`, descending through
/// `properties` and resolving `$ref`/`allOf`/`anyOf`/`oneOf` wrappers.
fn navigate<'a>(schema: &'a Value, segments: &[String]) -> Option<&'a Value> {
    let mut node = schema;
    for segment in segments {
        node = descend(schema, node, segment)?;
    }
    Some(node)
}

fn descend<'a>(schema: &'a Value, node: &'a Value, segment: &str) -> Option<&'a Value> {
    let node = resolve(schema, node);
    if let Some(properties) = node.get("properties").and_then(Value::as_object)
        && let Some(child) = properties.get(segment)
    {
        return Some(child);
    }
    // Map-valued object (`BTreeMap<..>`): any key follows `additionalProperties`
    // (only when it is a schema object, not the boolean `false`).
    if let Some(additional) = node.get("additionalProperties")
        && additional.is_object()
    {
        return Some(additional);
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                if let Some(found) = descend(schema, branch, segment) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn collect_property_names(schema: &Value, node: &Value, out: &mut BTreeSet<String>) {
    let node = resolve(schema, node);
    if let Some(properties) = node.get("properties").and_then(Value::as_object) {
        out.extend(properties.keys().cloned());
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                collect_property_names(schema, branch, out);
            }
        }
    }
}

/// Follow a single local `$ref` (`#/definitions/...`), or return `node` unchanged.
fn resolve<'a>(schema: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => json_pointer(schema, reference).unwrap_or(node),
        None => node,
    }
}

/// Resolve a JSON Pointer (optionally prefixed with `#`) against a schema.
fn json_pointer<'a>(schema: &'a Value, pointer: &str) -> Option<&'a Value> {
    let path = pointer.strip_prefix('#').unwrap_or(pointer);
    if path.is_empty() {
        return Some(schema);
    }
    let mut node = schema;
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let key = segment.replace("~1", "/").replace("~0", "~");
        node = match node {
            Value::Object(map) => map.get(&key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(node)
}

/// Rebuild the dotted path understood by [`Document::span_at`] from a schema
/// instance location (a JSON Pointer).
fn dotted_path(location: &Location) -> String {
    location
        .segments()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

/// Levenshtein edit distance over Unicode scalar values (own implementation,
/// no dependency).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            current[j + 1] = substitution.min(insertion).min(deletion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Sort diagnostics by `(file, line, column)` with absent locations first, so
/// downstream snapshots are stable regardless of validation order.
fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| {
                a.span
                    .map(|span| span.line)
                    .cmp(&b.span.map(|span| span.line))
            })
            .then_with(|| {
                a.span
                    .map(|span| span.column)
                    .cmp(&b.span.map(|span| span.column))
            })
    });
}
