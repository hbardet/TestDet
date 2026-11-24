use testdet_core::diagnostic::{Diagnostic, Diagnostics, Severity, Span, codes, render_text};

fn warning(code: &'static str) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Warning,
        file: None,
        span: None,
        path: None,
        message: String::new(),
        hint: None,
    }
}

fn error(code: &'static str) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: String::new(),
        hint: None,
    }
}

#[test]
fn renders_file_line_path_code_message_and_hint() {
    let d = Diagnostic {
        code: codes::E_UNKNOWN_VAR,
        severity: Severity::Error,
        file: Some("testdet/collection/getUser.yaml".into()),
        span: Some(Span {
            line: 17,
            column: 24,
        }),
        path: Some("headers.Authorization".into()),
        message: "'{{env.API_TOKN}}' does not resolve".into(),
        hint: Some("did you mean API_TOKEN?".into()),
    };
    let text = render_text(&Diagnostics::from(vec![d]));
    assert!(
        text.contains("testdet/collection/getUser.yaml:17:24"),
        "{text}"
    );
    assert!(
        text.contains("headers.Authorization") && text.contains("E_UNKNOWN_VAR"),
        "{text}"
    );
    assert!(text.contains("hint: did you mean API_TOKEN?"), "{text}");
}

#[test]
fn worst_severity_follows_the_exit_code_table() {
    let mut d = Diagnostics::new();
    assert_eq!(d.worst_severity(), None);
    d.push(warning(codes::W_UNSET_SECRET));
    assert!(!d.has_errors());
    d.push(error(codes::E_UNKNOWN_ENV));
    assert!(d.has_errors());
}
