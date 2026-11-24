//! JSON Schema derivation for the config, operation and users file models.
//!
//! Schemas are derived from the serde-annotated model types, so key names,
//! optionality and unknown-field rejection (`additionalProperties: false`)
//! here match the deserialisation contract exactly. Task 6 validates untyped
//! documents against these schemas before converting to typed values.

use schemars::Schema;
use schemars::generate::SchemaSettings;

use crate::config::model::{Manifest, UsersFile};
use crate::operation::model::Operation;

/// The JSON Schema for a manifest file (`testdet.yaml`).
#[must_use]
pub fn manifest() -> Schema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Manifest>()
}

/// The JSON Schema for an operation file.
#[must_use]
pub fn operation() -> Schema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Operation>()
}

/// The JSON Schema for a users file (`testdet/users.yaml`).
#[must_use]
pub fn users() -> Schema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<UsersFile>()
}
