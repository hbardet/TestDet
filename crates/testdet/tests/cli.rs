#[test]
fn help_lists_every_subcommand_and_exits_zero() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .arg("--help")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    for cmd in ["tui", "validate", "run", "env", "config", "schema", "show"] {
        assert!(text.contains(cmd), "`--help` must mention `{cmd}`");
    }
}
