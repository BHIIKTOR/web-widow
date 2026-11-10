//! the sealed session store.
//!
//! Layout, under `<data_dir>/auth/` (`0700`, created on the first seal):
//!
//! - `<site>.sess` (`0600`): `WWS1 | nonce(24) | ciphertext+tag`, where the
//! plaintext is the capture payload and the AEAD associated data is the site
//! name, so a jar copied to another site fails to authenticate.
//! - `<site>.meta` (`0600`): value-free JSON ([`SessionMeta`]) — capture time,
//! earliest expiry, cookie count, hosts, verify URL. Never a cookie.
//!
//! The 32-byte data key lives in the OS keyring (service [`KEYRING_SERVICE`],
//! account `session/<site>`), never in a file. Envelope: XChaCha20-Poly1305
//! (). Writes are temp-and-rename with the file created `0600`.
//!
//! **Fail closed.** With no reachable keyring, or a locked collection, the
//! store returns [`WidowError::AuthStoreUnavailable`] /
//! [`WidowError::AuthStoreLocked`] and writes nothing: the key is stored
//! *before* any file is created, and there is no plaintext fallback.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::valid_profile_name;
use crate::core::error::{Result, WidowError};

/// Sealed-jar magic (`web-widow session`, format 1).
pub const MAGIC: &[u8; 4] = b"WWS1";
/// Data-key length: 32 random bytes per site ().
pub const KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag length.
const TAG_LEN: usize = 16;
/// Keyring service; the account is `session/<site>`.
pub const KEYRING_SERVICE: &str = "web-widow";
const KEYRING_ACCOUNT_PREFIX: &str = "session/";

/// The value-free sidecar written beside a sealed jar.
///
/// By construction it cannot carry a cookie: every field is a count, a
/// timestamp, a host or the operator's verify URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionMeta {
 /// Unix seconds when the capture happened.
 pub captured_at: i64,
 /// The earliest cookie expiry in the jar (unix seconds), if any.
 pub expires_at_min: Option<i64>,
 /// How many cookies the jar holds.
 pub cookie_count: usize,
 /// The hosts the jar was sealed for, binding host first.
 pub hosts: Vec<String>,
 /// The profile's verify URL, when the capture was verified.
 pub verify_url: Option<String>,
}

impl SessionMeta {
 /// Whether the jar is past its earliest expiry at `now` (unix seconds).
 pub fn expired_at(&self, now: i64) -> bool {
 self.expires_at_min.is_some_and(|expiry| expiry <= now)
 }
}

/// What `auth status` may report: presence, counts, expiry — never a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStatus {
 /// A sealed jar exists.
 pub present: bool,
 /// The jar's metadata sidecar, when readable.
 pub meta: Option<SessionMeta>,
 /// The keyring holds a key for this site (checked only when a jar exists).
 pub key_present: bool,
}

impl SessionStatus {
 pub fn expired_at(&self, now: i64) -> bool {
 self.meta.as_ref().is_some_and(|meta| meta.expired_at(now))
 }
}

/// A decrypted session. The payload is zeroized on drop.
pub struct Unsealed {
 pub payload: Zeroizing<Vec<u8>>,
 pub meta: SessionMeta,
}

impl std::fmt::Debug for Unsealed {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("Unsealed")
.field("meta", &self.meta)
.field("payload_len", &self.payload.len())
.finish()
 }
}

/// Where the 32-byte per-site data key lives. The real implementation is the OS
/// keyring; unit tests inject an in-memory one. A key is never written to a
/// file by either implementation.
pub trait KeySource: Send + Sync {
 /// The key for `site`, or `None` when no key was ever stored.
 fn load(&self, site: &str) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>>;
 /// Store (or replace) the key for `site`.
 fn store(&self, site: &str, key: &[u8; KEY_LEN]) -> Result<()>;
 /// Delete the key for `site`; an absent key is not an error.
 fn delete(&self, site: &str) -> Result<()>;
}

/// The Secret Service-backed key source ().
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringKeys;

impl KeySource for KeyringKeys {
 fn load(&self, site: &str) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>> {
 match entry(site)?.get_password() {
 Ok(encoded) => decode_key(&encoded).map(Some),
 Err(keyring::Error::NoEntry) => Ok(None),
 Err(error) => Err(unavailable(error)),
 }
 }

 fn store(&self, site: &str, key: &[u8; KEY_LEN]) -> Result<()> {
 entry(site)?
.set_password(&encode_key(key))
.map_err(unavailable)
 }

 fn delete(&self, site: &str) -> Result<()> {
 match entry(site)?.delete_credential() {
 Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
 Err(error) => Err(unavailable(error)),
 }
 }
}

/// The sealed session store rooted at `<data_dir>/auth/`.
pub struct SessionStore {
 dir: PathBuf,
 keys: Box<dyn KeySource>,
}

impl std::fmt::Debug for SessionStore {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("SessionStore")
.field("dir", &self.dir)
.finish_non_exhaustive()
 }
}

impl SessionStore {
 /// A store whose keys live in the OS keyring.
 pub fn new(dir: PathBuf) -> Self {
 Self {
 dir,
 keys: Box::new(KeyringKeys),
 }
 }

 /// A store with an injected key source (tests; never the server path).
 pub fn with_keys(dir: PathBuf, keys: Box<dyn KeySource>) -> Self {
 Self { dir, keys }
 }

 pub fn dir(&self) -> &Path {
 &self.dir
 }

 /// Probe the key store for `site` **without touching the filesystem**.
 ///
 /// `auth login` calls this before it opens a browser window (): an
 /// unusable or locked keyring must be a typed refusal with no window and no
 /// directory, exactly like a failed seal.
 pub fn preflight(&self, site: &str) -> Result<()> {
 check_site(site)?;
 self.keys.load(site).map(|_| ())
 }

 /// Create the store directory `0700` when absent, and refuse one that any
 /// other user could read or traverse.
 pub fn prepare(&self) -> Result<()> {
 if !self.dir.exists() {
 let mut builder = DirBuilder::new();
 builder.recursive(true).mode(0o700);
 builder.create(&self.dir).map_err(|error| {
 WidowError::Internal(format!("cannot create {}: {error}", self.dir.display()))
 })?;
 }
 let metadata = fs::metadata(&self.dir).map_err(|error| {
 WidowError::Internal(format!("cannot stat {}: {error}", self.dir.display()))
 })?;
 if !metadata.is_dir() {
 return Err(stored_error(format!(
 "{} exists and is not a directory",
 self.dir.display()
)));
 }
 let mode = metadata.permissions().mode() & 0o777;
 if mode & 0o077 != 0 {
 return Err(stored_error(format!(
 "{} is {mode:03o}; the sealed session store requires 0700",
 self.dir.display()
)));
 }
 Ok(())
 }

 /// Seal `payload` for `site` under a fresh 32-byte key.
 ///
 /// The key is stored first: if the keyring is unavailable or locked, no
 /// file is created and the typed error is returned. A re-seal rotates the
 /// key and replaces both files atomically.
 pub fn seal(&self, site: &str, payload: &[u8], meta: &SessionMeta) -> Result<()> {
 check_site(site)?;

 let key = Zeroizing::new(<[u8; KEY_LEN]>::generate());
 let cipher = XChaCha20Poly1305::new(&Key::from(*key));
 let nonce = XNonce::generate();
 let ciphertext = cipher
.encrypt(
 &nonce,
 Payload {
 msg: payload,
 aad: site.as_bytes(),
 })
.map_err(|_| WidowError::Internal("session envelope encryption failed".to_string()))?;

 // Fail closed: the key must be in the keyring before anything is
 // created, so an unavailable or locked store leaves no trace at all.
 self.keys.store(site, &key)?;
 self.prepare()?;

 let mut jar = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
 jar.extend_from_slice(MAGIC);
 jar.extend_from_slice(&nonce);
 jar.extend_from_slice(&ciphertext);
 write_atomic(&self.session_path(site), &jar)?;

 // The sidecar is value-free by type; serialising it cannot leak a cookie.
 let meta_json = serde_json::to_vec_pretty(meta).map_err(|error| {
 WidowError::Internal(format!("cannot encode session meta: {error}"))
 })?;
 write_atomic(&self.meta_path(site), &meta_json)?;
 Ok(())
 }

 /// Decrypt the jar for `site`. `Ok(None)` when no jar exists; every other
 /// failure is typed and never falls back to plaintext.
 pub fn unseal(&self, site: &str) -> Result<Option<Unsealed>> {
 check_site(site)?;
 let bytes = match fs::read(self.session_path(site)) {
 Ok(bytes) => bytes,
 Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
 Err(error) => {
 return Err(WidowError::Internal(format!(
 "cannot read {}: {error}",
 self.session_path(site).display()
)));
 }
 };
 if bytes.len() < MAGIC.len() + NONCE_LEN + TAG_LEN || &bytes[..MAGIC.len()] != MAGIC {
 return Err(stored_error(format!(
 "the sealed session for `{site}` is not a `WWS1` jar"
)));
 }
 let key = self.keys.load(site)?.ok_or_else(|| {
 stored_error(format!(
 "the sealed session for `{site}` has no keyring key; discard it with `auth logout`"
))
 })?;
 let nonce: &XNonce = <&XNonce>::try_from(&bytes[MAGIC.len()..MAGIC.len() + NONCE_LEN])
.map_err(|_| stored_error(format!("the sealed session for `{site}` is truncated")))?;
 let cipher = XChaCha20Poly1305::new(&Key::from(*key));
 let plaintext = cipher
.decrypt(
 nonce,
 Payload {
 msg: &bytes[MAGIC.len() + NONCE_LEN..],
 aad: site.as_bytes(),
 })
.map_err(|_| {
 stored_error(format!(
 "the sealed session for `{site}` could not be authenticated"
))
 })?;
 Ok(Some(Unsealed {
 payload: Zeroizing::new(plaintext),
 meta: self.read_meta(site)?,
 }))
 }

 /// Presence, counts and expiry for one site. The keyring is consulted only
 /// when a jar exists, so a fresh store needs no session bus.
 pub fn status(&self, site: &str) -> Result<SessionStatus> {
 check_site(site)?;
 if !self.session_path(site).exists() {
 return Ok(SessionStatus::default());
 }
 Ok(SessionStatus {
 present: true,
 meta: Some(self.read_meta(site)?),
 key_present: self.keys.load(site)?.is_some(),
 })
 }

 /// Every sealed site, sorted. A missing store directory is empty.
 pub fn sites(&self) -> Result<Vec<String>> {
 let entries = match fs::read_dir(&self.dir) {
 Ok(entries) => entries,
 Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
 Err(error) => {
 return Err(WidowError::Internal(format!(
 "cannot read {}: {error}",
 self.dir.display()
)));
 }
 };
 let mut sites = Vec::new();
 for entry in entries {
 let entry = entry.map_err(|error| {
 WidowError::Internal(format!("cannot read {}: {error}", self.dir.display()))
 })?;
 let name = entry.file_name();
 let name = name.to_string_lossy();
 if let Some(site) = name.strip_suffix(".sess")
 && valid_profile_name(site)
 {
 sites.push(site.to_string());
 }
 }
 sites.sort();
 Ok(sites)
 }

 /// Delete the jar, its sidecar and its keyring key.
 ///
 /// Files first: a keyring failure still leaves no usable session behind,
 /// and the typed error says the key may remain. `Ok(false)` means there was
 /// no jar to delete.
 pub fn logout(&self, site: &str) -> Result<bool> {
 check_site(site)?;
 let mut removed = false;
 for path in [self.session_path(site), self.meta_path(site)] {
 match fs::remove_file(&path) {
 Ok(()) => removed = true,
 Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
 Err(error) => {
 return Err(WidowError::Internal(format!(
 "cannot remove {}: {error}",
 path.display()
)));
 }
 }
 }
 self.keys.delete(site)?;
 Ok(removed)
 }

 fn session_path(&self, site: &str) -> PathBuf {
 self.dir.join(format!("{site}.sess"))
 }

 fn meta_path(&self, site: &str) -> PathBuf {
 self.dir.join(format!("{site}.meta"))
 }

 fn read_meta(&self, site: &str) -> Result<SessionMeta> {
 let path = self.meta_path(site);
 let bytes = fs::read(&path).map_err(|error| {
 stored_error(format!(
 "the sealed session for `{site}` has no readable metadata ({}): {error}",
 path.display()
))
 })?;
 serde_json::from_slice(&bytes).map_err(|error| {
 stored_error(format!(
 "the metadata for `{site}` is not a session sidecar: {error}"
))
 })
 }
}

fn check_site(site: &str) -> Result<()> {
 if valid_profile_name(site) {
 Ok(())
 } else {
 Err(WidowError::InvalidUrl(format!(
 "`{site}` is not a valid session site name"
)))
 }
}

fn entry(site: &str) -> Result<keyring::Entry> {
 keyring::Entry::new(KEYRING_SERVICE, &format!("{KEYRING_ACCOUNT_PREFIX}{site}"))
.map_err(unavailable)
}

/// `PlatformFailure`/`NoDefaultStore` mean the store cannot be reached at all
/// (typically: no session bus, or no Secret Service provider); `NoStorageAccess`
/// means the collection exists but is locked. Both are fail-closed.
fn unavailable(error: keyring::Error) -> WidowError {
 let detail = match &error {
 keyring::Error::NoStorageAccess(platform) => {
 return WidowError::AuthStoreLocked {
 detail: single_line(&platform.to_string()),
 };
 }
 keyring::Error::NoDefaultStore => format!(
 "no credential store: {}",
 single_line(&store_status_detail())
),
 other => single_line(&other.to_string()),
 };
 WidowError::AuthStoreUnavailable { detail }
}

fn store_status_detail() -> String {
 match keyring::Entry::store_status() {
 Ok(()) => "the OS credential store reports no error".to_string(),
 Err(error) => error.to_string(),
 }
}

fn stored_error(detail: impl Into<String>) -> WidowError {
 WidowError::AuthStoreUnavailable {
 detail: detail.into(),
 }
}

/// One line, bounded: a platform error is operator-useful but must never carry
/// a newline into a log line or grow without limit.
fn single_line(message: &str) -> String {
 let cleaned: String = message
.chars()
.map(|character| {
 if character.is_control() {
 ' '
 } else {
 character
 }
 })
.collect();
 let trimmed = cleaned.trim();
 trimmed.chars().take(200).collect()
}

fn encode_key(key: &[u8; KEY_LEN]) -> String {
 let mut encoded = String::with_capacity(KEY_LEN * 2);
 for byte in key {
 encoded.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
 encoded.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
 }
 encoded
}

fn decode_key(encoded: &str) -> Result<Zeroizing<[u8; KEY_LEN]>> {
 let bytes = encoded.as_bytes();
 if bytes.len() != KEY_LEN * 2 {
 return Err(stored_error(
 "the stored session key is not 32 bytes".to_string()));
 }
 let mut key = Zeroizing::new([0u8; KEY_LEN]);
 for (index, chunk) in bytes.as_chunks::<2>().0.iter().enumerate() {
 let high = hex_digit(chunk[0])
.ok_or_else(|| stored_error("the stored session key is not hex".to_string()))?;
 let low = hex_digit(chunk[1])
.ok_or_else(|| stored_error("the stored session key is not hex".to_string()))?;
 key[index] = (high << 4) | low;
 }
 Ok(key)
}

fn hex_digit(byte: u8) -> Option<u8> {
 match byte {
 b'0'..=b'9' => Some(byte - b'0'),
 b'a'..=b'f' => Some(byte - b'a' + 10),
 b'A'..=b'F' => Some(byte - b'A' + 10),
 _ => None,
 }
}

/// Write `bytes` to `path` through a `0600` temp file and a rename, so a reader
/// sees either the old file or the new one, never a partial jar.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
 let name = path
.file_name()
.map(|name| name.to_string_lossy().into_owned())
.ok_or_else(|| WidowError::Internal(format!("{} has no file name", path.display())))?;
 let temp = path.with_file_name(format!("{name}.tmp"));

 // A stale temp from a severed run must not keep wider permissions.
 let mut file = OpenOptions::new()
.write(true)
.create(true)
.truncate(true)
.mode(0o600)
.open(&temp)
.map_err(|error| {
 WidowError::Internal(format!("cannot create {}: {error}", temp.display()))
 })?;
 file.set_permissions(fs::Permissions::from_mode(0o600))
.map_err(|error| {
 WidowError::Internal(format!("cannot chmod {}: {error}", temp.display()))
 })?;
 file.write_all(bytes).map_err(|error| {
 WidowError::Internal(format!("cannot write {}: {error}", temp.display()))
 })?;
 file.sync_all().map_err(|error| {
 WidowError::Internal(format!("cannot flush {}: {error}", temp.display()))
 })?;
 drop(file);

 fs::rename(&temp, path).map_err(|error| {
 WidowError::Internal(format!(
 "cannot rename {} to {}: {error}",
 temp.display(),
 path.display()
))
 })?;
 if let Some(dir) = path.parent()
 && let Ok(handle) = File::open(dir)
 {
 let _ = handle.sync_all();
 }
 Ok(())
}

#[cfg(test)]
mod tests {
 use super::*;
 use std::collections::BTreeMap;
 use std::sync::Mutex;

 /// In-memory key source: the same fail-closed contract without a bus.
 #[derive(Default)]
 struct MemoryKeys {
 keys: Mutex<BTreeMap<String, [u8; KEY_LEN]>>,
 fail_store: Option<WidowError>,
 }

 impl KeySource for MemoryKeys {
 fn load(&self, site: &str) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>> {
 let keys = self.keys.lock().unwrap();
 Ok(keys.get(site).map(|key| Zeroizing::new(*key)))
 }

 fn store(&self, site: &str, key: &[u8; KEY_LEN]) -> Result<()> {
 if let Some(error) = &self.fail_store {
 return Err(match error {
 WidowError::AuthStoreLocked { detail } => WidowError::AuthStoreLocked {
 detail: detail.clone(),
 },
 WidowError::AuthStoreUnavailable { detail } => {
 WidowError::AuthStoreUnavailable {
 detail: detail.clone(),
 }
 }
 other => WidowError::Internal(other.to_string()),
 });
 }
 self.keys.lock().unwrap().insert(site.to_string(), *key);
 Ok(())
 }

 fn delete(&self, site: &str) -> Result<()> {
 self.keys.lock().unwrap().remove(site);
 Ok(())
 }
 }

 fn meta() -> SessionMeta {
 SessionMeta {
 captured_at: 1_800_000_000,
 expires_at_min: Some(1_800_003_600),
 cookie_count: 3,
 hosts: vec!["example.com".to_string()],
 verify_url: Some("https://example.com/account".to_string()),
 }
 }

 fn store(dir: &Path) -> (SessionStore, std::sync::Arc<MemoryKeys>) {
 let keys = std::sync::Arc::new(MemoryKeys::default());
 let store = SessionStore::with_keys(
 dir.join("auth"),
 Box::new(ArcKeys(std::sync::Arc::clone(&keys))));
 (store, keys)
 }

 struct ArcKeys(std::sync::Arc<MemoryKeys>);

 impl KeySource for ArcKeys {
 fn load(&self, site: &str) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>> {
 self.0.load(site)
 }
 fn store(&self, site: &str, key: &[u8; KEY_LEN]) -> Result<()> {
 self.0.store(site, key)
 }
 fn delete(&self, site: &str) -> Result<()> {
 self.0.delete(site)
 }
 }

 fn mode_of(path: &Path) -> u32 {
 fs::metadata(path).unwrap().permissions().mode() & 0o777
 }

 #[test]
 fn a_sealed_session_round_trips_and_leaves_no_plaintext() {
 let temp = tempfile::tempdir().unwrap();
 let (store, keys) = store(temp.path());
 let payload = b"canary-cookie-value=super-secret".to_vec();

 store.seal("example", &payload, &meta()).unwrap();

 assert_eq!(mode_of(store.dir()), 0o700, "store directory is 0700");
 assert_eq!(mode_of(&store.session_path("example")), 0o600);
 assert_eq!(mode_of(&store.meta_path("example")), 0o600);

 let jar = fs::read(store.session_path("example")).unwrap();
 assert_eq!(&jar[..4], MAGIC);
 assert!(
 !jar.windows(11).any(|window| window == b"canary-cook"),
 "the jar must be ciphertext"
);
 let sidecar = fs::read_to_string(store.meta_path("example")).unwrap();
 assert!(!sidecar.contains("canary"), "the sidecar is value-free");
 assert!(!sidecar.contains("super-secret"));
 for forbidden in ["name", "value", "domain", "path", "cookie="] {
 assert!(
 !sidecar.contains(forbidden),
 "sidecar carries `{forbidden}`"
);
 }

 let unsealed = store.unseal("example").unwrap().unwrap();
 assert_eq!(&unsealed.payload[..], &payload[..]);
 assert_eq!(unsealed.meta, meta());
 assert_eq!(store.status("example").unwrap().meta.unwrap(), meta());
 assert!(keys.load("example").unwrap().is_some());
 }

 #[test]
 fn a_reseal_rotates_the_key_and_replaces_the_jar() {
 let temp = tempfile::tempdir().unwrap();
 let (store, _keys) = store(temp.path());
 store.seal("example", b"first", &meta()).unwrap();
 let first_jar = fs::read(store.session_path("example")).unwrap();
 let first_tag = first_jar[4 + NONCE_LEN..].to_vec();

 let rotated = SessionMeta {
 cookie_count: 5,
..meta()
 };
 store.seal("example", b"second", &rotated).unwrap();
 let second = fs::read(store.session_path("example")).unwrap();
 assert_ne!(
 &second[4 + NONCE_LEN..],
 &first_tag[..],
 "a new seal must re-key and re-encrypt"
);
 let unsealed = store.unseal("example").unwrap().unwrap();
 assert_eq!(&unsealed.payload[..], b"second");
 assert_eq!(unsealed.meta.cookie_count, 5);
 }

 #[test]
 fn a_tampered_jar_is_refused() {
 let temp = tempfile::tempdir().unwrap();
 let (store, _keys) = store(temp.path());
 store.seal("example", b"payload", &meta()).unwrap();

 let mut jar = fs::read(store.session_path("example")).unwrap();
 let last = jar.len() - 1;
 jar[last] ^= 0x01;
 fs::write(store.session_path("example"), &jar).unwrap();

 let error = store.unseal("example").unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("could not be authenticated"));
 }

 #[test]
 fn a_jar_copied_to_another_site_does_not_authenticate() {
 let temp = tempfile::tempdir().unwrap();
 let (store, keys) = store(temp.path());
 store.seal("first", b"payload", &meta()).unwrap();
 let jar = fs::read(store.session_path("first")).unwrap();
 fs::write(store.session_path("second"), &jar).unwrap();
 // Same key, different site: the AAD binds the jar to its own site.
 keys.store("second", &keys.load("first").unwrap().unwrap())
.unwrap();

 let error = store.unseal("second").unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("could not be authenticated"));
 }

 #[test]
 fn a_missing_jar_unseals_to_none_and_a_missing_sidecar_is_typed() {
 let temp = tempfile::tempdir().unwrap();
 let (store, keys) = store(temp.path());
 assert!(store.unseal("example").unwrap().is_none());
 assert_eq!(store.status("example").unwrap(), SessionStatus::default());
 assert!(store.sites().unwrap().is_empty());

 store.prepare().unwrap();
 let mut jar = MAGIC.to_vec();
 jar.extend_from_slice(&[0u8; 40]);
 fs::write(store.session_path("example"), &jar).unwrap();

 // A jar without its key is a typed refusal, not a plaintext fallback.
 let error = store.unseal("example").unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("no keyring key"));

 // A sealed jar whose sidecar is gone is typed rather than half-reported.
 store.seal("example", b"payload", &meta()).unwrap();
 fs::remove_file(store.meta_path("example")).unwrap();
 let error = store.unseal("example").unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("no readable metadata"));
 let error = store.status("example").unwrap_err();
 assert!(error.to_string().contains("no readable metadata"));
 assert!(keys.load("example").unwrap().is_some());
 }

 #[test]
 fn an_unavailable_keyring_creates_no_file() {
 let temp = tempfile::tempdir().unwrap();
 let keys = std::sync::Arc::new(MemoryKeys {
 fail_store: Some(WidowError::AuthStoreUnavailable {
 detail: "no session bus".to_string(),
 }),
..MemoryKeys::default()
 });
 let store = SessionStore::with_keys(temp.path().join("auth"), Box::new(ArcKeys(keys)));

 let error = store.seal("example", b"payload", &meta()).unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("no session bus"));

 let entries: Vec<_> = match fs::read_dir(store.dir()) {
 Ok(entries) => entries.flatten().map(|e| e.file_name()).collect(),
 Err(_) => Vec::new(),
 };
 assert!(
 entries.is_empty(),
 "fail-closed: no file or temp may exist, found {entries:?}"
);
 }

 #[test]
 fn a_locked_collection_is_typed_and_creates_no_file() {
 let temp = tempfile::tempdir().unwrap();
 let keys = std::sync::Arc::new(MemoryKeys {
 fail_store: Some(WidowError::AuthStoreLocked {
 detail: "the collection is locked".to_string(),
 }),
..MemoryKeys::default()
 });
 let store = SessionStore::with_keys(temp.path().join("auth"), Box::new(ArcKeys(keys)));

 let error = store.seal("example", b"payload", &meta()).unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_locked");
 assert!(!store.session_path("example").exists());
 assert!(!store.meta_path("example").exists());
 }

 #[test]
 fn a_world_accessible_store_directory_is_refused() {
 let temp = tempfile::tempdir().unwrap();
 let dir = temp.path().join("auth");
 fs::create_dir(&dir).unwrap();
 fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
 let (store, _keys) = store(temp.path());

 let error = store.prepare().unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 assert!(error.to_string().contains("0700"));
 assert!(
 store.seal("example", b"payload", &meta()).is_err(),
 "a wide store directory must not accept a seal"
);
 }

 #[test]
 fn logout_removes_both_files_and_the_key() {
 let temp = tempfile::tempdir().unwrap();
 let (store, keys) = store(temp.path());
 store.seal("example", b"payload", &meta()).unwrap();
 store.seal("other", b"payload", &meta()).unwrap();
 assert_eq!(
 store.sites().unwrap(),
 vec!["example".to_string(), "other".to_string()]
);

 assert!(store.logout("example").unwrap());
 assert!(!store.session_path("example").exists());
 assert!(!store.meta_path("example").exists());
 assert!(keys.load("example").unwrap().is_none());
 assert!(!store.status("example").unwrap().present);
 assert!(
 !store.logout("example").unwrap(),
 "a second logout is a no-op"
);
 assert_eq!(store.sites().unwrap(), vec!["other".to_string()]);
 }

 #[test]
 fn site_names_cannot_escape_the_store() {
 let temp = tempfile::tempdir().unwrap();
 let (store, _keys) = store(temp.path());
 for site in ["", ".", "..", "../x", "a/b", "-x", "x y"] {
 let error = store
.seal(site, b"payload", &meta())
.expect_err(&format!("`{site}` must be refused"));
 assert_eq!(
 error.code().as_str(),
 "invalid_url",
 "`{site}` must be a site-name refusal"
);
 }
 assert!(!store.dir().exists(), "a refused site writes nothing");
 }

 #[test]
 fn expiry_is_reported_from_the_sidecar() {
 let meta = SessionMeta {
 captured_at: 100,
 expires_at_min: Some(200),
..SessionMeta::default()
 };
 assert!(!meta.expired_at(199));
 assert!(meta.expired_at(200));
 let session_only = SessionMeta {
 expires_at_min: None,
..SessionMeta::default()
 };
 assert!(!session_only.expired_at(i64::MAX));
 }

 #[test]
 fn key_encoding_round_trips_and_refuses_junk() {
 let key = [7u8; KEY_LEN];
 let encoded = encode_key(&key);
 assert_eq!(encoded.len(), KEY_LEN * 2);
 assert_eq!(decode_key(&encoded).unwrap()[..], key[..]);
 for junk in [
 "",
 "00",
 &"zz".repeat(KEY_LEN),
 &"0".repeat(KEY_LEN * 2 - 1),
 ] {
 let error = decode_key(junk).unwrap_err();
 assert_eq!(error.code().as_str(), "auth_store_unavailable");
 }
 assert_eq!(single_line("a\nb\tc"), "a b c");
 }
}
