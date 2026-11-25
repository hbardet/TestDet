//! Config loading: span-preserving document projection.
//!
//! [`Document`] is the bridge between raw YAML on disk and the typed config
//! conversion, schema validation and resolution performed by later tasks.

pub mod crosscheck;
pub mod discover;
pub mod document;
pub mod merge;
pub mod model;
pub mod parse;
pub mod resolve;
pub mod schema;
pub mod validate;

pub use crosscheck::{USERS_SURFACE, check_value, crosscheck};
pub use discover::{
    Config, LoadOptions, discover, load_config, load_diagnostics, load_with_diagnostics,
};
pub use document::Document;
pub use resolve::{Resolved, SecretsConfig, resolve};
