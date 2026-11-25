//! Fixtures for the crate's own unit tests.
//!
//! `App::new` needs a real `Config` and `Resolved`, and neither can be built
//! from a literal outside `testdet-core` (`Version`'s field and the last three
//! `Resolved` fields are private), so the fixtures are YAML files on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fixture tree that removes itself when dropped.
pub(crate) struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// The fixture's root directory.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Write a minimal one-environment, one-operation fixture and return it.
pub(crate) fn fixture_env(label: &str) -> Fixture {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!(
        "testdet-tui-{label}-{}-{nanos}",
        std::process::id()
    ));
    write(
        &root,
        "testdet.yaml",
        "version: 1\n\
         name: demo\n\
         defaultEnvironment: local\n\
         collections: [products]\n\
         environments:\n\
         \x20 local:\n\
         \x20   baseUrl: http://localhost:8080\n",
    );
    write(
        &root,
        "products/list-products.yaml",
        "id: listProducts\n\
         description: List products\n\
         tags: [read]\n\
         request:\n\
         \x20 kind: http\n\
         \x20 method: GET\n\
         \x20 path: /products\n",
    );
    Fixture { root }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|error| {
            panic!("cannot create {}: {error}", parent.display());
        });
    }
    fs::write(&path, content)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
