//! Direct `config::run` tests: the resolved view, the redaction contract and
//! exit codes (§7.19, §7.12).

use std::fs;
use std::path::Path;

use testdet_cli::commands::config::{self, ConfigArgs};
use testdet_cli::exit::Exit;
use testdet_cli::format::Format;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn declaration() -> &'static str {
    "API_TOKEN:\n  description: token\n"
}

/// A config whose `staging` environment seeds the secret value in the process
/// environment (`source: env`) and echoes it in a header.
fn env_source_manifest() -> &'static str {
    "version: 1\nname: my-api\ncollections: []\ndefaultEnvironment: staging\nenvironments:\n  staging:\n    baseUrl: https://staging.example.com\n    secrets:\n      source: env\n    headers:\n      Authorization: Bearer sup3rs3cr3t-value\n"
}

/// A config whose `staging` environment reads the secret from a dotenv file
/// and declares one user with a token (plus a real `login` operation so
/// `defaultLogin`/`users.alice.login` resolve).
fn dotenv_source_manifest() -> &'static str {
    "version: 1\nname: my-api\ncollections: [users]\ndefaultEnvironment: staging\nenvironments:\n  staging:\n    baseUrl: https://staging.example.com\n    secrets:\n      source: dotenv\n      file: .env.staging\n    headers:\n      Authorization: Bearer cli-secret-value\n      X-Org-Id: org-123\n"
}

fn login_operation() -> &'static str {
    "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n"
}

fn users_file() -> &'static str {
    "defaultLogin: login\nusers:\n  alice:\n    login: login\n    token: tok-alice-value\n"
}

fn dotenv_file() -> &'static str {
    "API_TOKEN=cli-secret-value\n"
}

#[test]
fn json_output_redacts_a_seeded_secret_and_shows_the_censored_header() {
    // Seed a real secret value in the process environment (ruling 6).
    unsafe {
        std::env::set_var("API_TOKEN", "sup3rs3cr3t-value");
    }

    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", env_source_manifest());
    write(root.path(), "testdet/secrets/staging.yaml", declaration());
    let config = root.path().join("testdet/testdet.yaml");

    let (exit, output) = config::run(ConfigArgs {
        config: Some(config),
        env: Some("staging".to_string()),
        user: None,
        format: Format::Json,
    });

    unsafe {
        std::env::remove_var("API_TOKEN");
    }

    assert_eq!(exit, Exit::Ok);

    // The raw value must be absent from the whole output string.
    assert!(!output.contains("sup3rs3cr3t-value"));

    let parsed: serde_json::Value = serde_json::from_str(&output).expect("pretty JSON");
    assert_eq!(parsed["env"], "staging");
    assert_eq!(parsed["baseUrl"], "https://staging.example.com");
    assert_eq!(parsed["graphqlPath"], "/graphql");
    assert_eq!(
        parsed["headers"]["Authorization"],
        "Bearer <API_TOKEN>-CENSORED"
    );
    assert_eq!(parsed["secrets"]["API_TOKEN"]["declared"], true);
    assert_eq!(parsed["secrets"]["API_TOKEN"]["set"], true);
    assert_eq!(parsed["secrets"]["API_TOKEN"]["origin"]["kind"], "shell");
}

#[test]
fn text_output_groups_the_resolved_view_and_rejects_an_unknown_user() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        dotenv_source_manifest(),
    );
    write(root.path(), "testdet/secrets/staging.yaml", declaration());
    write(root.path(), "testdet/users.yaml", users_file());
    write(root.path(), "testdet/users/login.yaml", login_operation());
    write(root.path(), "testdet/.env.staging", dotenv_file());
    let config = root.path().join("testdet/testdet.yaml");
    let env_file = root
        .path()
        .canonicalize()
        .unwrap()
        .join("testdet")
        .join(".env.staging");

    let (exit, output) = config::run(ConfigArgs {
        config: Some(config.clone()),
        env: None,
        user: None,
        format: Format::Text,
    });
    assert_eq!(exit, Exit::Ok);

    assert!(output.contains("environment:\n  env: staging\n"));
    assert!(output.contains("  baseUrl: https://staging.example.com\n"));
    assert!(output.contains("  graphqlPath: /graphql\n"));
    assert!(output.contains("headers:\n"));
    assert!(output.contains("  Authorization: Bearer <API_TOKEN>-CENSORED\n"));
    assert!(output.contains("  X-Org-Id: org-123\n"));
    assert!(output.contains("users:\n  alice:\n    login: login\n    token: set\n"));
    assert!(output.contains(&format!(
        "secrets:\n  API_TOKEN:\n    declared: true\n    set: true\n    origin: {}\n",
        env_file.display()
    )));

    // Neither the dotenv secret nor the user token value may leak.
    assert!(!output.contains("cli-secret-value"));
    assert!(!output.contains("tok-alice-value"));

    // An unknown `--user` is a usage error, not silently ignored.
    let (exit, output) = config::run(ConfigArgs {
        config: Some(config),
        env: None,
        user: Some("nope".to_string()),
        format: Format::Text,
    });
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("unknown user `nope`"));
}

#[test]
fn ndjson_is_a_usage_error_for_config() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", env_source_manifest());
    write(root.path(), "testdet/secrets/staging.yaml", declaration());
    let config = root.path().join("testdet/testdet.yaml");

    let (exit, output) = config::run(ConfigArgs {
        config: Some(config),
        env: None,
        user: None,
        format: Format::Ndjson,
    });
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("`--format ndjson` only applies to `run`"));
}
