//! Files and uploads: path safety, one-read caching, and MIME sniffing (§7.14).
//!
//! A `files:` block names a file relative to the directory that holds
//! `testdet/testdet.yaml`. Everything here is defensive by construction: a
//! path may never leave that directory ([`check_path`]), a file is read at
//! most once per run ([`FileCache`]), and a declared MIME type is never
//! second-guessed while an absent one is sniffed from the bytes themselves
//! ([`sniff_mime`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use bytes::Bytes;

use crate::diagnostic::{Diagnostic, Severity, codes};
use crate::operation::model::FileSpec;

/// The default size ceiling for one file: 5 MB.
///
/// The spec calls the ceiling configurable but does not name a setting for it
/// in v1, so this task enforces the default and leaves the key for later.
pub const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;

/// The hint every path-escape diagnostic carries, verbatim from §7.14.
const ESCAPE_HINT: &str = "a file path must stay inside the config directory";

/// One file, loaded and ready to become a request part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedFile {
    /// The canonical absolute path the file was read from.
    pub path: PathBuf,
    /// The file's bytes.
    pub bytes: Bytes,
    /// The declared MIME type, or the sniffed one.
    pub mime: String,
    /// The `Content-Disposition` filename, defaulted and sanitised.
    pub filename: String,
}

/// A per-run file cache: one read per file, however many requests use it.
///
/// Keyed by canonical path, so `fixtures/a.png` and `./fixtures/../fixtures/a.png`
/// are one entry. The bytes are cached; MIME type and filename are derived per
/// spec, because two specs may point at one file with different declarations.
#[derive(Debug)]
pub struct FileCache {
    root: PathBuf,
    files: BTreeMap<PathBuf, Bytes>,
    reads: usize,
}

impl FileCache {
    /// A cache resolving relative paths against `root`, the config directory.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: BTreeMap::new(),
            reads: 0,
        }
    }

    /// Load `spec`, reading the file at most once per cache.
    ///
    /// Path safety runs first ([`check_path`]), so a missing or escaping path
    /// never reaches the size check or the cache.
    // The `Err` type is the pinned interface; boxing it would leak into every
    // caller of `prepare`.
    #[allow(clippy::result_large_err)]
    pub fn load(&mut self, spec: &FileSpec) -> Result<LoadedFile, Diagnostic> {
        let path = check_path(&self.root, spec)?;

        let bytes = match self.files.get(&path) {
            Some(bytes) => bytes.clone(),
            None => {
                // Check the length before reading, so an oversized file is
                // refused without pulling megabytes into memory. A file that
                // grows between the two checks is caught again below.
                let length = fs::metadata(&path).map_err(|_| unknown_file(&path))?.len();
                if length > MAX_FILE_BYTES as u64 {
                    return Err(too_large(&path, length));
                }

                let bytes = fs::read(&path).map_err(|_| unknown_file(&path))?;
                self.reads += 1;
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(too_large(&path, bytes.len() as u64));
                }
                let bytes = Bytes::from(bytes);
                self.files.insert(path.clone(), bytes.clone());
                bytes
            }
        };

        Ok(LoadedFile {
            mime: sniff_mime(&bytes, spec.mime_type.as_deref()),
            filename: filename_for(&path, spec),
            bytes,
            path,
        })
    }

    /// How many `std::fs::read` calls this cache has made. Test-visible.
    pub fn reads(&self) -> usize {
        self.reads
    }
}

/// Resolve `spec.path` against `root`, refusing anything that could escape it.
///
/// The decision tree, in order:
///
/// 1. an absolute path is `E_PATH_ESCAPE`, before any filesystem access;
/// 2. any `..` component is `E_PATH_ESCAPE`, before any filesystem access;
/// 3. the joined path is canonicalized, so a missing file is `E_UNKNOWN_FILE`
///    naming the resolved absolute path;
/// 4. the canonical path must still start with the canonical root, because a
///    symlink inside the config directory may point outside it — otherwise
///    `E_PATH_ESCAPE` again.
// The `Err` type is the pinned interface; boxing it would leak into every
// caller of `prepare`.
#[allow(clippy::result_large_err)]
pub fn check_path(root: &Path, spec: &FileSpec) -> Result<PathBuf, Diagnostic> {
    let requested = Path::new(&spec.path);

    if requested.is_absolute() {
        return Err(escape(format!("`{}` is an absolute path", spec.path)));
    }
    if requested
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(escape(format!("`{}` contains `..`", spec.path)));
    }

    let joined = root.join(requested);
    let canonical = fs::canonicalize(&joined).map_err(|_| unknown_file(&joined))?;
    // A root that cannot be canonicalized cannot contain the file either, but
    // reporting the file is the more useful diagnostic, so fall back to the
    // root as given and let the containment check decide.
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !canonical.starts_with(&canonical_root) {
        return Err(escape(format!(
            "`{}` resolves to `{}`, which is outside the config directory",
            spec.path,
            canonical.display()
        )));
    }

    Ok(canonical)
}

/// The MIME type for `bytes`: the declared one verbatim, else sniffed.
///
/// Sniffing is deliberately small and predictable (§7.14): the five binary
/// magic numbers a test fixture is likely to carry, then UTF-8 text split into
/// JSON and plain text, then `application/octet-stream`.
pub fn sniff_mime(bytes: &[u8], declared: Option<&str>) -> String {
    if let Some(declared) = declared {
        return declared.to_string();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".to_string();
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".to_string();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif".to_string();
    }
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf".to_string();
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return "application/zip".to_string();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        // Control bytes other than tab/newline/CR mean this is not text we can
        // honestly label `text/plain`.
        let control = text
            .bytes()
            .any(|byte| byte < 0x20 && byte != b'\t' && byte != b'\n' && byte != b'\r');
        if !control {
            let trimmed = text.trim_start();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && serde_json::from_str::<serde_json::Value>(text).is_ok()
            {
                return "application/json".to_string();
            }
            return "text/plain".to_string();
        }
    }
    "application/octet-stream".to_string()
}

/// The `Content-Disposition` filename: the spec's, else the file's basename,
/// with `"` and CR/LF replaced by `_` so no header can be broken by a filename.
fn filename_for(path: &Path, spec: &FileSpec) -> String {
    let name = match &spec.filename {
        Some(name) if !name.is_empty() => name.clone(),
        _ => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string()),
    };
    name.chars()
        .map(|character| match character {
            '"' | '\r' | '\n' => '_',
            other => other,
        })
        .collect()
}

fn escape(message: String) -> Diagnostic {
    Diagnostic {
        code: codes::E_PATH_ESCAPE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message,
        hint: Some(ESCAPE_HINT.to_string()),
    }
}

fn unknown_file(path: &Path) -> Diagnostic {
    Diagnostic {
        code: codes::E_UNKNOWN_FILE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("`{}` does not exist or cannot be read", path.display()),
        hint: Some("check the path; it is resolved against the config directory".to_string()),
    }
}

fn too_large(path: &Path, length: u64) -> Diagnostic {
    Diagnostic {
        code: codes::E_FILE_TOO_LARGE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!(
            "`{}` is {length} bytes; the limit is {MAX_FILE_BYTES} bytes (5 MB)",
            path.display()
        ),
        hint: Some("reference a smaller file".to_string()),
    }
}
