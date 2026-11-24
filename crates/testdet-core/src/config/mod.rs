//! Config loading: span-preserving document projection.
//!
//! [`Document`] is the bridge between raw YAML on disk and the typed config
//! conversion, schema validation and resolution performed by later tasks.

pub mod document;
pub mod model;
pub mod schema;
pub mod validate;

pub use document::Document;
