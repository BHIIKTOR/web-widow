//! Chrome/Chromium discovery ().
//!
//! Order: `render.chrome_path`, then `$CHROME_PATH`, then well-known paths,
//! including the Playwright browser cache. Discovery is lazy: a server with no
//! render calls never touches this code, and a missing browser is a typed
//! `render_unavailable` at call time rather than a startup failure.

use std::path::{Path, PathBuf};

use crate::core::error::{Result, WidowError};

/// Test seam: the browser resolver can be injected so a missing-Chrome test
/// never depends on the host's installed binaries.
pub trait ChromeResolver: Send + Sync + std::fmt::Debug {
 /// Return an executable Chrome/Chromium path or an actionable error naming
 /// `render.chrome_path` and every location that was checked.
 fn resolve(&self) -> Result<PathBuf>;

 /// Locations considered, for diagnostics and evidence.
 fn checked(&self) -> Vec<String>;
}

#[derive(Debug)]
pub struct SystemChromeResolver {
 configured: Option<PathBuf>,
 discovery: bool,
 env: Option<PathBuf>,
 well_known: Vec<PathBuf>,
}

impl SystemChromeResolver {
 pub fn new(configured: Option<PathBuf>, discovery: bool) -> Self {
 let env = std::env::var_os("CHROME_PATH")
.map(PathBuf::from)
.filter(|path| !path.as_os_str().is_empty());
 let well_known = if discovery {
 well_known_candidates()
 } else {
 Vec::new()
 };
 Self {
 configured,
 discovery,
 env,
 well_known,
 }
 }

 #[cfg(test)]
 pub fn from_parts(
 configured: Option<PathBuf>,
 discovery: bool,
 env: Option<PathBuf>,
 well_known: Vec<PathBuf>) -> Self {
 Self {
 configured,
 discovery,
 env,
 well_known,
 }
 }

 fn candidates(&self) -> Vec<(String, PathBuf)> {
 let mut candidates = Vec::new();
 if let Some(configured) = &self.configured {
 candidates.push(("render.chrome_path".to_string(), configured.clone()));
 }
 if !self.discovery {
 return candidates;
 }
 if let Some(env) = &self.env {
 candidates.push(("$CHROME_PATH".to_string(), env.clone()));
 }
 for path in &self.well_known {
 candidates.push((path.display().to_string(), path.clone()));
 }
 candidates
 }
}

impl ChromeResolver for SystemChromeResolver {
 fn resolve(&self) -> Result<PathBuf> {
 let candidates = self.candidates();
 for (_label, path) in &candidates {
 if is_executable_file(path) {
 return Ok(path.clone());
 }
 }
 let checked = if candidates.is_empty() {
 if self.configured.is_some() {
 "render.chrome_path was set but discovery is disabled".to_string()
 } else {
 "render.chrome_path is unset and discovery is disabled".to_string()
 }
 } else {
 candidates
.iter()
.map(|(label, path)| match label.as_str() {
 // The configured path is already human-readable; the env
 // label is not, so show both where they differ.
 "render.chrome_path" => path.display().to_string(),
 "$CHROME_PATH" => format!("$CHROME_PATH ({})", path.display()),
 _ => path.display().to_string(),
 })
.collect::<Vec<_>>()
.join(", ")
 };
 Err(WidowError::RenderUnavailable(format!(
 "no Chrome/Chromium binary found; checked {checked}. Set render.chrome_path to a Chrome/Chromium executable (or install Chrome) to enable rendering."
)))
 }

 fn checked(&self) -> Vec<String> {
 self.candidates()
.into_iter()
.map(|(label, path)| match label.as_str() {
 "$CHROME_PATH" => format!("$CHROME_PATH ({})", path.display()),
 _ => path.display().to_string(),
 })
.chain(std::iter::once(
 "render.chrome_path (unset or not executable)".to_string()))
.collect()
 }
}

fn is_executable_file(path: &Path) -> bool {
 let Ok(metadata) = std::fs::metadata(path) else {
 return false;
 };
 if !metadata.is_file() {
 return false;
 }
 #[cfg(unix)]
 {
 use std::os::unix::fs::PermissionsExt;
 metadata.permissions().mode() & 0o111 != 0
 }
 #[cfg(not(unix))]
 {
 true
 }
}

fn well_known_candidates() -> Vec<PathBuf> {
 let mut candidates = Vec::new();
 for path in [
 "/usr/bin/chromium",
 "/usr/bin/chromium-browser",
 "/usr/bin/google-chrome",
 "/usr/bin/google-chrome-stable",
 "/usr/local/bin/chromium",
 "/usr/local/bin/google-chrome",
 "/snap/bin/chromium",
 "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
 "/Applications/Chromium.app/Contents/MacOS/Chromium",
 ] {
 candidates.push(PathBuf::from(path));
 }
 for root in playwright_cache_roots() {
 collect_playwright_candidates(&root, &mut candidates);
 }
 candidates
}

fn playwright_cache_roots() -> Vec<PathBuf> {
 let mut roots = Vec::new();
 if let Some(home) = dirs::home_dir() {
 roots.push(home.join(".cache/ms-playwright"));
 }
 if let Some(cache) = dirs::cache_dir() {
 roots.push(cache.join("ms-playwright"));
 }
 roots
}

fn collect_playwright_candidates(root: &Path, candidates: &mut Vec<PathBuf>) {
 let Ok(entries) = std::fs::read_dir(root) else {
 return;
 };
 let mut installed: Vec<PathBuf> = entries
.flatten()
.map(|entry| entry.path())
.filter(|path| {
 path.file_name()
.and_then(|name| name.to_str())
.is_some_and(|name| name.starts_with("chromium-"))
 })
.collect();
 // Newest browser build first; the exact version is not pinned because the
 // cache is tool-managed (that is why render.chrome_path is the supported
 // configuration).
 installed.sort();
 installed.reverse();
 for directory in installed {
 for relative in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
 candidates.push(directory.join(relative));
 }
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 fn temp_executable(dir: &tempfile::TempDir) -> PathBuf {
 let path = dir.path().join("chrome");
 std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
 #[cfg(unix)]
 {
 use std::os::unix::fs::PermissionsExt;
 std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
 }
 path
 }

 #[test]
 fn configured_path_wins_and_is_returned_when_executable() {
 let dir = tempfile::tempdir().unwrap();
 let chrome = temp_executable(&dir);
 let resolver =
 SystemChromeResolver::from_parts(Some(chrome.clone()), false, None, Vec::new());
 assert_eq!(resolver.resolve().unwrap(), chrome);
 }

 #[test]
 fn a_missing_configured_path_names_the_key_and_the_checked_path() {
 let dir = tempfile::tempdir().unwrap();
 let missing = dir.path().join("not-here");
 let resolver =
 SystemChromeResolver::from_parts(Some(missing.clone()), false, None, Vec::new());
 let error = resolver.resolve().unwrap_err();
 let message = error.to_string();
 assert!(matches!(error, WidowError::RenderUnavailable(_)));
 assert!(message.contains("render.chrome_path"), "{message}");
 assert!(
 message.contains(&missing.display().to_string()),
 "{message}"
);
 }

 #[test]
 fn env_path_is_used_when_no_configured_path_is_set() {
 let dir = tempfile::tempdir().unwrap();
 let chrome = temp_executable(&dir);
 let resolver =
 SystemChromeResolver::from_parts(None, true, Some(chrome.clone()), Vec::new());
 assert_eq!(resolver.resolve().unwrap(), chrome);
 }

 #[test]
 fn discovery_disabled_without_a_configured_path_is_an_actionable_error() {
 let resolver = SystemChromeResolver::from_parts(None, false, None, Vec::new());
 let message = resolver.resolve().unwrap_err().to_string();
 assert!(message.contains("render.chrome_path"), "{message}");
 assert!(message.contains("discovery is disabled"), "{message}");
 }

 #[test]
 fn no_candidate_lists_every_checked_well_known_path() {
 let dir = tempfile::tempdir().unwrap();
 let fake = dir.path().join("not-chrome");
 let resolver = SystemChromeResolver::from_parts(
 None,
 true,
 Some(dir.path().join("env-not-chrome")),
 vec![fake.clone()]);
 let message = resolver.resolve().unwrap_err().to_string();
 assert!(message.contains("$CHROME_PATH"), "{message}");
 assert!(message.contains(&fake.display().to_string()), "{message}");
 }

 #[test]
 fn checked_reports_the_config_key() {
 let resolver = SystemChromeResolver::from_parts(None, false, None, Vec::new());
 assert!(
 resolver
.checked()
.iter()
.any(|entry| entry.contains("render.chrome_path"))
);
 }
}
