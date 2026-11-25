//! The `Outcome` boundary type and its nested value types (§7.9).
//!
//! `Outcome` is the serialisable record of one completed operation attempt. It
//! is what every core entry point returns, and what a frontend formats. It
//! never contains "a string to display": the request echo, response headers,
//! cookies and body are data, and the frontend owns presentation.
//!
//! `body` is [`Option<Bytes>`], base64-encoded in JSON (standard alphabet,
//! with padding) because a raw byte string cannot be a JSON string. The
//! companion `body_retained: bool` is explicit rather than inferred: a reader
//! sees `"body_retained": false` with `"body": null` when retention is off,
//! so "absent" is never confusable with "not retained".

use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::redact::Redactor;

/// The serialisable record of one completed operation attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub operation: String,
    pub user: Option<String>,
    pub request: RequestEcho,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub cookies: Vec<CookieInfo>,
    /// Base64-encoded when retained; `null` when `body_retained` is `false`.
    #[serde(with = "base64_bytes")]
    pub body: Option<Bytes>,
    /// Whether `body` was kept at all (§7.9 body retention).
    pub body_retained: bool,
    /// Size in bytes of the received body, set regardless of retention.
    pub body_size: u64,
    pub graphql_errors: Vec<GraphQLError>,
    pub timing: Timing,
    pub transport_error: Option<TransportError>,
}

impl Outcome {
    /// Scrub every registered secret from the parts of this outcome that can
    /// carry one: the request URL, request headers, response headers, cookie
    /// values, and the body.
    ///
    /// The body is scrubbed only when it is valid UTF-8: a JSON body is walked
    /// value-by-value and re-serialised, other UTF-8 text is scrubbed whole.
    /// Non-UTF-8 bytes are left untouched — we cannot safely interpret them as
    /// text, so we scrub nothing rather than corrupt them.
    pub fn redact(&mut self, redactor: &Redactor) {
        self.request.url = redactor.scrub(&self.request.url);
        for (_, value) in &mut self.request.headers {
            *value = redactor.scrub(value);
        }
        for (_, value) in &mut self.headers {
            *value = redactor.scrub(value);
        }
        for cookie in &mut self.cookies {
            cookie.value = redactor.scrub(&cookie.value);
        }
        // A transport error's message embeds the request URL (and thus any
        // secret in a query string); GraphQL errors are echoed server text.
        if let Some(error) = &mut self.transport_error {
            error.message = redactor.scrub(&error.message);
        }
        for error in &mut self.graphql_errors {
            error.message = redactor.scrub(&error.message);
            if let Some(extensions) = &mut error.extensions {
                redactor.scrub_json(extensions);
            }
        }

        if let Some(body) = &self.body
            && let Ok(text) = std::str::from_utf8(body)
        {
            let scrubbed = match serde_json::from_str::<Value>(text) {
                Ok(mut value) => {
                    redactor.scrub_json(&mut value);
                    value.to_string()
                }
                Err(_) => redactor.scrub(text),
            };
            self.body = Some(Bytes::from(scrubbed));
        }
        // Non-UTF-8 bytes are left untouched: we cannot safely interpret
        // them as text, so we scrub nothing rather than corrupt them.
    }
}

/// The request as it was actually sent, before it went on the wire (§7.9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEcho {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body_size: u64,
}

/// One response cookie, parsed from `Set-Cookie` rather than scraped out of
/// `headers`, so `HttpOnly`, `Secure`, expiry and the jar-normalised domain
/// survive the trip to a frontend (§7.9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieInfo {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub http_only: bool,
    pub secure: bool,
    pub expires: Option<String>,
}

/// Request/response timing. `total`, `connect` and `ttfb` serialise as
/// integer milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timing {
    #[serde(with = "millis")]
    pub total: Duration,
    #[serde(default, with = "millis::option")]
    pub connect: Option<Duration>,
    #[serde(default, with = "millis::option")]
    pub ttfb: Option<Duration>,
}

/// One GraphQL error entry from `errors[]` (§7.9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQLError {
    pub message: String,
    pub path: Option<Vec<String>>,
    pub extensions: Option<Value>,
}

/// A transport-level failure, kept alongside `status` and `graphql_errors` so
/// a response that both failed and carried a GraphQL error can show both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportError {
    pub kind: TransportKind,
    pub message: String,
}

/// The category of a transport failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Connect,
    Timeout,
    Read,
    Tls,
    Cancelled,
    Other,
}

/// Serialise a [`Duration`] as integer milliseconds (and back).
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }

    /// The `Option<Duration>` variant: `null` when absent, milliseconds when set.
    pub mod option {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match duration {
                Some(duration) => serializer.serialize_u64(duration.as_millis() as u64),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: Deserializer<'de>,
        {
            let millis: Option<u64> = Option::deserialize(deserializer)?;
            Ok(millis.map(Duration::from_millis))
        }
    }
}

/// Serialise an optional byte body as base64 (standard alphabet, padding) or
/// `null` when absent.
mod base64_bytes {
    use base64::Engine;
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(body: &Option<Bytes>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match body {
            Some(bytes) => {
                serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Bytes>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded: Option<String> = Option::deserialize(deserializer)?;
        match encoded {
            Some(encoded) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(serde::de::Error::custom)?;
                Ok(Some(Bytes::from(bytes)))
            }
            None => Ok(None),
        }
    }
}
