//! Templating: type-aware substitution and reference extraction (§7.8).

use std::collections::BTreeMap;

use serde_json::Value;
use serde_json::json;

use testdet_core::diagnostic::codes;
use testdet_core::operation::model::{FileEncoding, FileSpec};
use testdet_core::secrets::{Lookup, Origin, SecretSource};
use testdet_core::template::{Resolver, references};

/// A minimal in-memory secret source for tests. `names()` is exercised so the
/// `{{env.NAME}}` closest-match hint has something to suggest.
struct TestSecrets {
    values: BTreeMap<String, String>,
    names: Vec<String>,
}

impl SecretSource for TestSecrets {
    fn lookup(&self, name: &str) -> Lookup {
        Lookup {
            value: self.values.get(name).cloned(),
            origin: Some(Origin::Shell),
        }
    }

    fn names(&self) -> Vec<String> {
        self.names.clone()
    }
}

fn empty_secrets() -> TestSecrets {
    TestSecrets {
        values: BTreeMap::new(),
        names: Vec::new(),
    }
}

fn resolver<'a>(
    vars: &'a BTreeMap<String, Value>,
    session: &'a BTreeMap<String, Value>,
    env: &'a dyn SecretSource,
    files: &'a BTreeMap<String, FileSpec>,
) -> Resolver<'a> {
    Resolver {
        vars,
        session,
        env,
        files,
    }
}

#[test]
fn a_whole_value_expression_preserves_the_type() {
    let vars = BTreeMap::from([("age".to_string(), json!(30))]);
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let resolved = resolver.resolve_str("{{var.age}}").unwrap();
    assert!(resolved.is_number(), "expected a number, got {resolved:?}");
    assert_eq!(resolved, json!(30));
    assert_ne!(resolved, Value::String("30".into()));
}

#[test]
fn a_whole_value_big_integer_survives_substitution() {
    let huge: Value = serde_json::from_str("99999999999999999999").expect("arbitrary precision");
    let vars = BTreeMap::from([("huge".to_string(), huge)]);
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let resolved = resolver.resolve_str("{{var.huge}}").unwrap();
    assert!(resolved.is_number(), "expected a number, got {resolved:?}");
    assert_eq!(resolved.to_string(), "99999999999999999999");
}

#[test]
fn an_object_and_a_bool_survive_a_whole_value_expression() {
    let vars = BTreeMap::from([
        ("profile".to_string(), json!({"name": "Ada", "admin": true})),
        ("flag".to_string(), json!(true)),
    ]);
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let profile = resolver.resolve_str("{{var.profile}}").unwrap();
    assert!(profile.is_object(), "expected an object, got {profile:?}");
    assert_eq!(profile, json!({"name": "Ada", "admin": true}));

    let flag = resolver.resolve_str("{{var.flag}}").unwrap();
    assert!(flag.is_boolean(), "expected a bool, got {flag:?}");
    assert_eq!(flag, json!(true));
}

#[test]
fn an_embedded_expression_is_stringified() {
    let vars = BTreeMap::new();
    let session = BTreeMap::from([("token".to_string(), json!("abc"))]);
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let resolved = resolver.resolve_str("Bearer {{session.token}}").unwrap();
    assert_eq!(resolved, json!("Bearer abc"));
}

#[test]
fn a_bare_token_is_never_substituted_with_an_empty_string() {
    let vars = BTreeMap::new();
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let err = resolver.resolve_str("{{var.missing}}").unwrap_err();
    assert!(err.has_errors());
    let diagnostic = err.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_VAR);
}

#[test]
fn unresolved_token_suggests_the_closest_name() {
    let vars = BTreeMap::from([("API_TOKEN".to_string(), json!("secret"))]);
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let err = resolver.resolve_str("{{var.API_TOKN}}").unwrap_err();
    let diagnostic = err.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_VAR);
    let hint = diagnostic.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("API_TOKEN"),
        "hint must suggest `API_TOKEN`, got: {hint:?}"
    );
}

#[test]
fn malformed_expressions_are_diagnostics_with_a_path() {
    let vars = BTreeMap::from([("x".to_string(), json!(1))]);
    let session = BTreeMap::new();
    let files = BTreeMap::new();
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    for (input, fragment) in [
        ("{{var.}}", "{{var.}}"),
        ("{{ }}", "{{ }}"),
        ("{{var.x", "{{var.x"),
        // The token span is `{{{var.x}}` (the stray `{` makes `{var` the
        // namespace); the message names that span and the reason.
        ("{{{var.x}}}", "unknown namespace"),
    ] {
        let err = resolver.resolve_str(input).unwrap_err();
        let diagnostic = err.iter().next().unwrap();
        assert_eq!(
            diagnostic.code,
            codes::E_WRONG_TYPE,
            "`{input}` must be E_WRONG_TYPE, got {:?}",
            diagnostic.code
        );
        assert!(
            diagnostic.message.contains(fragment),
            "message must name the offending expression for `{input}` (looking for `{fragment}`), got: {}",
            diagnostic.message
        );
    }

    // The recursive walker attaches the document path to string diagnostics.
    let err = resolver
        .resolve(&json!({"nested": {"x": "{{var.}}"}}))
        .unwrap_err();
    let diagnostic = err.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_WRONG_TYPE);
    assert_eq!(diagnostic.path.as_deref(), Some("nested.x"));
}

#[test]
fn references_reports_every_token_with_its_path() {
    let value = json!({
        "variables": {
            "input": {
                "items": [
                    {"name": "{{var.first}}"},
                    {"name": "{{session.second}}", "id": "{{file.third}}"}
                ]
            }
        }
    });

    let refs = references(&value);
    assert_eq!(refs.len(), 3, "expected 3 references, got {refs:?}");

    let mut paths: Vec<(&str, &str)> = refs
        .iter()
        .map(|r| (r.raw.as_str(), r.path.as_str()))
        .collect();
    paths.sort();

    assert_eq!(paths[0], ("{{file.third}}", "variables.input.items.1.id"));
    assert_eq!(
        paths[1],
        ("{{session.second}}", "variables.input.items.1.name")
    );
    assert_eq!(paths[2], ("{{var.first}}", "variables.input.items.0.name"));

    // Malformed expressions are not references.
    assert!(references(&json!({"a": "{{var.}}"})).is_empty());
    assert!(references(&json!({"a": "{{var.x"})).is_empty());
}

#[test]
fn a_whole_file_reference_resolves_to_the_token_unchanged() {
    let vars = BTreeMap::new();
    let session = BTreeMap::new();
    let files = BTreeMap::from([(
        "avatar".to_string(),
        FileSpec {
            path: "uploads/avatar.png".to_string(),
            mime_type: Some("image/png".to_string()),
            filename: Some("avatar.png".to_string()),
            encoding: FileEncoding::Multipart,
        },
    )]);
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    let resolved = resolver.resolve_str("{{file.avatar}}").unwrap();
    assert_eq!(resolved, Value::String("{{file.avatar}}".into()));
}

#[test]
fn an_unknown_or_embedded_file_reference_is_a_diagnostic() {
    let vars = BTreeMap::new();
    let session = BTreeMap::new();
    let files = BTreeMap::from([(
        "avatar".to_string(),
        FileSpec {
            path: "uploads/avatar.png".to_string(),
            mime_type: None,
            filename: None,
            encoding: FileEncoding::default(),
        },
    )]);
    let env = empty_secrets();
    let resolver = resolver(&vars, &session, &env, &files);

    // Unknown name -> E_UNKNOWN_FILE, hint lists the declared name.
    let err = resolver.resolve_str("{{file.avater}}").unwrap_err();
    let diagnostic = err.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_FILE);
    let hint = diagnostic.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("avatar"),
        "hint must list the declared file `avatar`, got: {hint:?}"
    );

    // Embedded file reference -> E_WRONG_TYPE.
    let err = resolver.resolve_str("prefix-{{file.avatar}}").unwrap_err();
    let diagnostic = err.iter().next().unwrap();
    assert_eq!(diagnostic.code, codes::E_WRONG_TYPE);
    let hint = diagnostic.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("{{file.avatar}}"),
        "hint must name the whole-value form, got: {hint:?}"
    );
}
