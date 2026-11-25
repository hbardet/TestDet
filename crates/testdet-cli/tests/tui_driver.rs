//! The executor driver's environment switch (C2, §7.16).
//!
//! The switch itself needs no server: what is asserted here is that the driver
//! re-loads the config for the named environment, resolves it, and reports an
//! unknown name as diagnostics text instead of panicking.

use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use testdet_cli::tui_driver::ExecutorDriver;
use testdet_core::config::{LoadOptions, load_config, resolve};
use testdet_core::redact::redactor_for;
use testdet_tui::Driver;

fn fixture() -> (TempDir, LoadOptions) {
    let root = TempDir::new().expect("a temp directory");
    let manifest = root.path().join("testdet/testdet.yaml");
    fs::create_dir_all(manifest.parent().expect("has a parent")).expect("create the fixture dir");
    fs::write(
        &manifest,
        "version: 1\n\
         name: demo\n\
         defaultEnvironment: staging\n\
         collections: []\n\
         environments:\n\
         \x20 staging:\n\
         \x20   baseUrl: https://staging.example.com\n\
         \x20 production:\n\
         \x20   baseUrl: https://api.example.com\n",
    )
    .expect("write the manifest");
    let loader = LoadOptions {
        config: Some(manifest),
        cwd: root.path().to_path_buf(),
        env: None,
    };
    (root, loader)
}

fn driver(loader: &LoadOptions) -> ExecutorDriver {
    let config = load_config(loader).expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    let redactor = Arc::new(redactor_for(&config, &resolved));
    ExecutorDriver::spawn(loader.clone(), Arc::new(resolved), redactor, None)
        .expect("the worker starts")
}

#[test]
fn switching_environment_re_resolves_the_config() {
    let (_root, loader) = fixture();
    let mut driver = driver(&loader);

    let production = driver
        .switch_environment("production")
        .expect("a declared environment resolves");
    assert_eq!(production.env_name, "production");
    assert_eq!(production.base_url.host_str(), Some("api.example.com"));

    // Back again: the loader kept the original options, so nothing is sticky.
    let staging = driver
        .switch_environment("staging")
        .expect("the default environment resolves too");
    assert_eq!(staging.base_url.host_str(), Some("staging.example.com"));

    let error = driver
        .switch_environment("nowhere")
        .expect_err("an undeclared environment is an error");
    assert!(
        error.contains("unknown environment") && error.contains("nowhere"),
        "the diagnostics name the problem: {error}"
    );
}

#[test]
fn cancelling_an_idle_driver_is_a_no_op() {
    let (_root, loader) = fixture();
    let mut driver = driver(&loader);

    // `Esc` when nothing runs must not poison the next request: the token the
    // driver replaces on switch is fresh, and the worker keeps running.
    driver.cancel();
    assert!(driver.events().is_empty());
    let resolved = driver
        .switch_environment("staging")
        .expect("the driver still works after an idle cancel");
    assert_eq!(resolved.env_name, "staging");
}
