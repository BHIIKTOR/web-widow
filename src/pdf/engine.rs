//! PDFium engine loading (IMP-DEC-041, C2-9).
//!
//! `pdfium-render` binds the native `libpdfium.so` through `libloading` at
//! runtime; the bindings are a process-wide singleton, so the library is loaded
//! once and cannot be swapped without a restart. `[pdf].pdfium_path` is strict;
//! otherwise the documented discovery chain is env → exe dir → data dir →
//! system. Every failure is the typed `render_unavailable` error naming the
//! setting that would fix it.
//!
//! Version pinning: the crate is built with the `pdfium_8125` bindings. An older
//! library fails symbol resolution (a typed load error); a newer library is
//! symbol-compatible and cannot be distinguished at runtime — the deployment
//! record pins the file's SHA-256 instead.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::Pdfium;

use crate::config::PdfConfig;
use crate::core::error::{Result, WidowError};

/// Environment variable used by the discovery chain (config-file-free).
pub const ENV_PDFIUM_PATH: &str = "WEB_WIDOW_PDFIUM_PATH";

/// The platform's library file name (`libpdfium.so` on Linux).
pub fn platform_library_name() -> std::ffi::OsString {
    Pdfium::pdfium_platform_library_name()
}

/// The process-wide parse mutex. PDFium is not reentrant; the crate's
/// `thread_safe` wrapper already serializes individual calls, and this lock
/// serializes whole-document parses so one PDF cannot interleave with another.
static PARSE_LOCK: Mutex<()> = Mutex::new(());

/// The loaded library source, remembered so a later call can tell whether the
/// configured path matches what this process actually bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    Path(PathBuf),
    System,
}

impl Source {
    fn describe(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::System => "the system library".to_string(),
        }
    }
}

type Loaded = std::result::Result<Source, String>;

static LOADED: OnceLock<Loaded> = OnceLock::new();

/// One engine resolution input. `env_path`/`exe_dir` are passed explicitly so a
/// test can exercise the order without mutating the process environment.
#[derive(Debug, Clone)]
pub struct EngineSpec {
    pub pdfium_path: Option<PathBuf>,
    pub discovery: bool,
    pub env_path: Option<PathBuf>,
    pub exe_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
}

impl EngineSpec {
    /// Build the spec from `[pdf]`, the environment and the running process.
    pub fn from_config(config: &PdfConfig, data_dir: Option<PathBuf>) -> Self {
        Self {
            pdfium_path: config.pdfium_path.clone(),
            discovery: config.pdfium_discovery,
            env_path: std::env::var_os(ENV_PDFIUM_PATH)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            exe_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
            data_dir,
        }
    }
}

enum Candidate {
    Path(PathBuf),
    System,
}

/// Ensure the process-wide engine is loaded. Idempotent; a strict path that
/// differs from what this process bound is rejected.
pub fn ensure(spec: &EngineSpec) -> Result<()> {
    ensure_with(&LOADED, spec).map(|_source| ())
}

/// The cache is a parameter so the resolution/failure paths are testable
/// without a second process. Production always uses [`LOADED`].
pub(crate) fn ensure_with(cache: &OnceLock<Loaded>, spec: &EngineSpec) -> Result<Source> {
    let state = cache.get_or_init(|| load(spec));
    match state {
        Ok(source) => {
            if let Some(configured) = &spec.pdfium_path
                && !matches!(source, Source::Path(path) if same_path(configured, path))
            {
                return Err(render_unavailable(format!(
                    "PDFium is already loaded from {}; `[pdf].pdfium_path` = {} cannot be applied \
                     until the process restarts",
                    source.describe(),
                    configured.display()
                )));
            }
            Ok(source.clone())
        }
        Err(message) => Err(render_unavailable(message.clone())),
    }
}

/// Take the process-wide parse lock (poison-tolerant: a panicking parse must not
/// wedge later extractions).
pub fn lock_parse() -> std::sync::MutexGuard<'static, ()> {
    PARSE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn load(spec: &EngineSpec) -> Loaded {
    let candidates = candidates(spec);
    if candidates.is_empty() {
        return Err(format!(
            "PDFium is not configured: set `[pdf].pdfium_path` to the library file or enable \
             `[pdf].pdfium_discovery` (searched: env {ENV_PDFIUM_PATH}, <exe dir>, \
             <data dir>/lib, system library)"
        ));
    }

    let mut attempts: Vec<String> = Vec::new();
    for candidate in candidates {
        match candidate {
            Candidate::Path(path) => {
                if !path.is_file() {
                    attempts.push(format!("{}: not found", path.display()));
                    continue;
                }
                match Pdfium::bind_to_library(&path) {
                    Ok(bindings) => {
                        // `Pdfium::new` calls FPDF_InitLibrary once (the
                        // bindings are a global).
                        let _ = Pdfium::new(bindings);
                        return Ok(Source::Path(path));
                    }
                    Err(error) => attempts.push(format!("{}: {error}", path.display())),
                }
            }
            Candidate::System => match Pdfium::bind_to_system_library() {
                Ok(bindings) => {
                    let _ = Pdfium::new(bindings);
                    return Ok(Source::System);
                }
                Err(error) => attempts.push(format!("system library: {error}")),
            },
        }
    }

    if spec.pdfium_path.is_some() {
        return Err(format!(
            "`[pdf].pdfium_path` could not be loaded; attempted: {}",
            attempts.join("; ")
        ));
    }
    Err(format!(
        "PDFium could not be loaded: set `[pdf].pdfium_path` to the library file or \
         `[pdf].pdfium_discovery` to search (env {ENV_PDFIUM_PATH}, <exe dir>, \
         <data dir>/lib, system library); attempted: {}",
        attempts.join("; ")
    ))
}

fn candidates(spec: &EngineSpec) -> Vec<Candidate> {
    if let Some(path) = &spec.pdfium_path {
        return vec![Candidate::Path(path.clone())];
    }
    if !spec.discovery {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(path) = &spec.env_path {
        out.push(Candidate::Path(path.clone()));
    }
    if let Some(exe_dir) = &spec.exe_dir {
        out.push(Candidate::Path(
            exe_dir.join(Pdfium::pdfium_platform_library_name()),
        ));
    }
    if let Some(data_dir) = &spec.data_dir {
        out.push(Candidate::Path(
            data_dir
                .join("lib")
                .join(Pdfium::pdfium_platform_library_name()),
        ));
    }
    out.push(Candidate::System);
    out
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn render_unavailable(message: String) -> WidowError {
    WidowError::RenderUnavailable(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_path(path: &str) -> EngineSpec {
        EngineSpec {
            pdfium_path: Some(PathBuf::from(path)),
            discovery: false,
            env_path: None,
            exe_dir: None,
            data_dir: None,
        }
    }

    #[test]
    fn a_strict_missing_path_names_the_config_key_and_does_not_fall_back() {
        let cache: OnceLock<Loaded> = OnceLock::new();
        let error = ensure_with(&cache, &spec_with_path("/nonexistent/libpdfium.so")).unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::RenderUnavailable
        );
        let message = error.to_string();
        assert!(message.contains("[pdf].pdfium_path"), "{message}");
        assert!(message.contains("/nonexistent/libpdfium.so"), "{message}");
        assert!(!message.contains("system"), "{message}");
        // The failure is cached, not re-attempted on every call.
        let again = ensure_with(&cache, &spec_with_path("/nonexistent/libpdfium.so")).unwrap_err();
        assert!(again.to_string().contains("[pdf].pdfium_path"));
    }

    #[test]
    fn no_path_and_discovery_off_names_both_keys() {
        let cache: OnceLock<Loaded> = OnceLock::new();
        let error = ensure_with(
            &cache,
            &EngineSpec {
                pdfium_path: None,
                discovery: false,
                env_path: None,
                exe_dir: None,
                data_dir: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::RenderUnavailable
        );
        let message = error.to_string();
        assert!(message.contains("[pdf].pdfium_path"), "{message}");
        assert!(message.contains("[pdf].pdfium_discovery"), "{message}");
    }

    #[test]
    fn discovery_attempts_are_reported_in_the_documented_order() {
        let cache: OnceLock<Loaded> = OnceLock::new();
        let spec = EngineSpec {
            pdfium_path: None,
            discovery: true,
            env_path: Some(PathBuf::from("/tmp/env-libpdfium.so")),
            exe_dir: Some(PathBuf::from("/tmp/exe-dir")),
            data_dir: Some(PathBuf::from("/tmp/data-dir")),
        };
        let error = ensure_with(&cache, &spec).unwrap_err();
        let message = error.to_string();
        let attempts = &message[message.find("attempted: ").expect("attempt list")..];
        let env_at = attempts.find("env-libpdfium.so").expect("env tried");
        let exe_at = attempts.find("exe-dir").expect("exe dir tried");
        let data_at = attempts.find("data-dir").expect("data dir tried");
        let system_at = attempts.find("system library").expect("system tried");
        assert!(
            env_at < exe_at && exe_at < data_at && data_at < system_at,
            "{message}"
        );
        assert!(message.contains("WEB_WIDOW_PDFIUM_PATH"), "{message}");
    }

    #[test]
    fn a_configured_path_is_strict_even_when_discovery_is_on() {
        let cache: OnceLock<Loaded> = OnceLock::new();
        let spec = EngineSpec {
            pdfium_path: Some(PathBuf::from("/tmp/configured/libpdfium.so")),
            discovery: true,
            env_path: Some(PathBuf::from("/tmp/env-libpdfium.so")),
            exe_dir: Some(PathBuf::from("/tmp/exe-dir")),
            data_dir: Some(PathBuf::from("/tmp/data-dir")),
        };
        let error = ensure_with(&cache, &spec).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("/tmp/configured/libpdfium.so"),
            "{message}"
        );
        assert!(!message.contains("env-libpdfium.so"), "{message}");
        assert!(!message.contains("system library"), "{message}");
    }
}
