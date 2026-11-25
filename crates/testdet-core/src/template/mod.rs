//! Templating: type-aware substitution and reference extraction (§7.8).
//!
//! `value.rs` owns the token grammar, scanning, reference extraction and
//! stringification; `resolve.rs` owns [`Resolver`], which walks a JSON value
//! and substitutes with the resolved value's type intact for whole-value
//! expressions and a stringified form for embedded ones.

pub mod resolve;
pub mod value;

pub use resolve::Resolver;
pub use value::{Malformed, Reference, Token, malformed, parse_token, references};
