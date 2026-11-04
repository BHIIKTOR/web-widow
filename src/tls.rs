//! Local TLS material for the loopback listener (v0.8).
//!
//! Generated material is a small local CA plus a server leaf under
//! `<data>/tls/`; an operator-provided PEM pair is the override. This module
//! only prepares and validates material; the HTTP accept loop lives in
//! [`crate::mcp::http`].

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ServerConfig;
use rustls::{CertificateError, RootCertStore, crypto};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::HttpConfig;

const TLS_DIR: &str = "tls";
const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca-key.pem";
const SERVER_CERT: &str = "server.pem";
const SERVER_KEY: &str = "server-key.pem";
const META: &str = "meta.json";
const META_FORMAT: u32 = 1;
const CA_DAYS: i64 = 3650;
const LEAF_DAYS: i64 = 397;
const RENEW_WITHIN_DAYS: i64 = 30;
const SANS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];
const CA_CN: &str = "web-widow local CA";
const LEAF_CN: &str = "web-widow local server";

/// Which material the listener will serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsSource {
    Generated,
    Override,
}

impl TlsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generated => "generated",
            Self::Override => "override",
        }
    }
}

/// Prepared TLS configuration for the listener.
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub config: Arc<ServerConfig>,
    pub source: TlsSource,
    pub ca_path: Option<PathBuf>,
}

/// Typed startup errors. The prefix is the stable operator-facing token.
#[derive(Debug, Error)]
pub enum TlsError {
    #[error("tls_requires_http: [http].tls = true requires `serve --http`")]
    RequiresHttp,
    #[error(
        "tls_override_incomplete: [http].tls_cert_file and [http].tls_key_file must be set together"
    )]
    OverrideIncomplete,
    #[error(
        "tls_bind_not_loopback: generated certificates cover loopback only ({addr}); supply tls_cert_file/tls_key_file with the right SAN or keep the loopback bind"
    )]
    BindNotLoopback { addr: SocketAddr },
    #[error("tls_material_missing: {path}: {detail}")]
    Missing { path: PathBuf, detail: String },
    #[error("tls_material_incomplete: {path}: {detail}")]
    Incomplete { path: PathBuf, detail: String },
    #[error("tls_material_invalid: {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
    #[error("tls_ca_expired: {path}: {detail}; delete {dir} to re-mint and re-import the new CA")]
    CaExpired {
        path: PathBuf,
        dir: PathBuf,
        detail: String,
    },
}

impl TlsError {
    fn invalid(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    format: u32,
    /// Diagnostics only: the CA expiry is re-checked against the certificate
    /// chain and this record is absent when `meta.json` had to be rebuilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_not_after: Option<String>,
    server_not_after: String,
    sans: Vec<String>,
    created_at: String,
    renewed_at: String,
}

/// Load the configured material. `data_dir` is the already-locked data root;
/// generation happens only here, after [`crate::store::DataDir`] is held.
pub fn load(
    config: &HttpConfig,
    data_dir: &Path,
    bind: SocketAddr,
) -> Result<TlsMaterial, TlsError> {
    match (&config.tls_cert_file, &config.tls_key_file) {
        (Some(cert), Some(key)) => load_override(cert, key),
        (Some(_), None) | (None, Some(_)) => Err(TlsError::OverrideIncomplete),
        (None, None) => {
            if !bind.ip().is_loopback() {
                return Err(TlsError::BindNotLoopback { addr: bind });
            }
            generated(data_dir)
        }
    }
}

fn load_override(cert_path: &Path, key_path: &Path) -> Result<TlsMaterial, TlsError> {
    check_private_key_mode(key_path)?;
    let certs = read_certs(cert_path)?;
    let key = read_key(key_path)?;
    let config = server_config(certs, key).map_err(|error| TlsError::Invalid {
        path: key_path.to_path_buf(),
        detail: format!("[http].tls_cert_file/[http].tls_key_file: {error}"),
    })?;
    Ok(TlsMaterial {
        config: Arc::new(config),
        source: TlsSource::Override,
        ca_path: None,
    })
}

fn generated(data_dir: &Path) -> Result<TlsMaterial, TlsError> {
    let dir = data_dir.join(TLS_DIR);
    if !dir.exists() {
        create_tls_dir(&dir)?;
        generate_set(&dir, CA_DAYS, LEAF_DAYS)?;
    } else {
        check_tls_dir(&dir)?;
        let present = [CA_CERT, CA_KEY, SERVER_CERT, SERVER_KEY, META]
            .iter()
            .filter(|name| dir.join(name).exists())
            .count();
        if present == 0 {
            generate_set(&dir, CA_DAYS, LEAF_DAYS)?;
        } else if !(dir.join(CA_CERT).exists() && dir.join(CA_KEY).exists()) {
            return Err(TlsError::Incomplete {
                path: dir,
                detail: "the generated CA is partial; ca.pem and ca-key.pem are required"
                    .to_string(),
            });
        } else {
            ensure_existing(&dir)?;
        }
    }

    let ca_path = dir.join(CA_CERT);
    let certs = read_certs(&dir.join(SERVER_CERT))?;
    let key = read_key(&dir.join(SERVER_KEY))?;
    let config = server_config(certs, key)?;
    Ok(TlsMaterial {
        config: Arc::new(config),
        source: TlsSource::Generated,
        ca_path: Some(ca_path),
    })
}

/// Validate durable material and repair an unusable leaf while the CA is
/// intact. Only the leaf, its key and `meta.json` may be re-minted, always by
/// the same CA; the CA itself is never regenerated or overwritten here.
fn ensure_existing(dir: &Path) -> Result<(), TlsError> {
    check_private_key_mode(&dir.join(CA_KEY))?;
    let ca_cert = read_certs(&dir.join(CA_CERT))?;
    let ca_key = read_key(&dir.join(CA_KEY))?;
    // `with_single_cert` compares SubjectPublicKeyInfo: a CA certificate and a
    // key that no longer match are refused, not "repaired".
    let _ = server_config(ca_cert.clone(), ca_key.clone_key())?;
    let verifier = chain_verifier(&ca_cert, &dir.join(CA_CERT))?;

    // `meta.json` is diagnostics only: an absent or unreadable record only
    // loses the recorded expiry and renewal window, it never blocks startup.
    let meta = read_meta(dir);
    let now = Utc::now();
    if let Some(recorded) = meta.as_ref().and_then(|meta| meta.ca_not_after.as_deref())
        && parse_time(recorded, &dir.join(META)).is_ok_and(|expiry| expiry <= now)
    {
        return Err(TlsError::CaExpired {
            path: dir.join(CA_CERT),
            dir: dir.to_path_buf(),
            detail: format!("CA expired at {recorded}"),
        });
    }

    // The renewal decision never depends on the stored leaf still verifying:
    // an expired, not-yet-valid, SAN-incomplete, soon-to-expire or missing leaf
    // is re-minted by the intact CA.
    if !leaf_is_current(dir, &verifier, meta.as_ref())? {
        renew_leaf(dir, meta.as_ref())?;
    }
    Ok(())
}

/// `meta.json` when present, readable and of the current format.
fn read_meta(dir: &Path) -> Option<Meta> {
    let text = fs::read_to_string(dir.join(META)).ok()?;
    let meta: Meta = serde_json::from_str(&text).ok()?;
    (meta.format == META_FORMAT).then_some(meta)
}

fn chain_verifier(
    ca_cert: &[CertificateDer<'static>],
    ca_path: &Path,
) -> Result<Arc<WebPkiServerVerifier>, TlsError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(ca_cert[0].clone())
        .map_err(|error| TlsError::Invalid {
            path: ca_path.to_path_buf(),
            detail: format!("CA certificate is invalid: {error}"),
        })?;
    let provider = Arc::new(crypto::aws_lc_rs::default_provider());
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|error| TlsError::Invalid {
            path: ca_path.to_path_buf(),
            detail: format!("cannot build the verifier: {error}"),
        })
}

/// Whether the stored leaf can be served unchanged. Repairable wear returns
/// `Ok(false)` and is re-minted; material that is unparseable, foreign or whose
/// key does not match returns `Err` and nothing is overwritten.
fn leaf_is_current(
    dir: &Path,
    verifier: &WebPkiServerVerifier,
    meta: Option<&Meta>,
) -> Result<bool, TlsError> {
    let cert_path = dir.join(SERVER_CERT);
    let key_path = dir.join(SERVER_KEY);
    if !cert_path.exists() || !key_path.exists() {
        return Ok(false);
    }
    check_private_key_mode(&key_path)?;
    let leaf_cert = read_certs(&cert_path)?;
    let leaf_key = read_key(&key_path)?;
    server_config(leaf_cert.clone(), leaf_key)?;

    // Expired, not-yet-valid and SAN-incomplete leaves are renewal triggers;
    // an unfamiliar issuer or a broken signature is not.
    for name in SANS {
        let server_name = ServerName::try_from(name).map_err(|error| TlsError::Invalid {
            path: cert_path.clone(),
            detail: format!("cannot parse SAN `{name}`: {error}"),
        })?;
        if let Err(error) = verifier.verify_server_cert(
            &leaf_cert[0],
            &leaf_cert[1..],
            &server_name,
            &[],
            UnixTime::now(),
        ) {
            if is_renewable_leaf_error(&error) {
                return Ok(false);
            }
            return Err(TlsError::Invalid {
                path: cert_path.clone(),
                detail: format!("leaf does not verify for `{name}`: {error}"),
            });
        }
    }

    // A lost record is rebuilt together with the leaf it describes.
    let Some(meta) = meta else {
        return Ok(false);
    };
    if parse_time(&meta.server_not_after, &dir.join(META))
        .is_ok_and(|window| window <= Utc::now() + chrono::Duration::days(RENEW_WITHIN_DAYS))
    {
        return Ok(false);
    }
    if meta.sans != SANS.iter().map(|s| (*s).to_string()).collect::<Vec<_>>() {
        return Ok(false);
    }
    Ok(true)
}

/// Leaf conditions that a re-mint repairs on its own.
fn is_renewable_leaf_error(error: &rustls::Error) -> bool {
    matches!(
        error,
        rustls::Error::InvalidCertificate(
            CertificateError::Expired
                | CertificateError::ExpiredContext { .. }
                | CertificateError::NotValidYet
                | CertificateError::NotValidYetContext { .. }
                | CertificateError::NotValidForName
                | CertificateError::NotValidForNameContext { .. }
        )
    )
}

fn renew_leaf(dir: &Path, old_meta: Option<&Meta>) -> Result<(), TlsError> {
    let ca_path = dir.join(CA_CERT);
    let ca_cert = read_certs(&ca_path)?;
    let ca_key_pem = fs::read_to_string(dir.join(CA_KEY)).map_err(|error| TlsError::Missing {
        path: dir.join(CA_KEY),
        detail: format!("cannot read the CA key: {error}"),
    })?;
    let signing_key = KeyPair::from_pem(&ca_key_pem).map_err(|error| TlsError::Invalid {
        path: dir.join(CA_KEY),
        detail: format!("cannot load the CA key: {error}"),
    })?;
    let ca_params = ca_params();
    let issuer = Issuer::from_params(&ca_params, &signing_key);
    let (leaf, leaf_key, server_not_after) = make_leaf(&issuer)?;

    // A freshly minted leaf is always inside its own window, so a failed chain
    // check here can only be the CA: refuse before overwriting anything.
    let verifier = chain_verifier(&ca_cert, &ca_path)?;
    for name in SANS {
        let server_name = ServerName::try_from(name).map_err(|error| TlsError::Invalid {
            path: ca_path.clone(),
            detail: format!("cannot parse SAN `{name}`: {error}"),
        })?;
        if let Err(error) =
            verifier.verify_server_cert(leaf.der(), &[], &server_name, &[], UnixTime::now())
        {
            return Err(match error {
                rustls::Error::InvalidCertificate(
                    CertificateError::Expired | CertificateError::ExpiredContext { .. },
                ) => TlsError::CaExpired {
                    path: ca_path.clone(),
                    dir: dir.to_path_buf(),
                    detail: format!("CA cannot issue a leaf at {}: {error}", now_rfc3339()),
                },
                other => TlsError::Invalid {
                    path: ca_path.clone(),
                    detail: format!("minted leaf does not verify: {other}"),
                },
            });
        }
    }

    write_file(&dir.join(SERVER_CERT), leaf.pem().as_bytes(), 0o644)?;
    write_file(
        &dir.join(SERVER_KEY),
        leaf_key.serialize_pem().as_bytes(),
        0o600,
    )?;
    let meta = Meta {
        format: META_FORMAT,
        ca_not_after: old_meta
            .and_then(|meta| meta.ca_not_after.as_deref())
            .filter(|value| parse_time(value, &dir.join(META)).is_ok())
            .map(str::to_string),
        server_not_after,
        sans: SANS.iter().map(|s| (*s).to_string()).collect(),
        created_at: old_meta
            .map(|meta| meta.created_at.clone())
            .unwrap_or_else(now_rfc3339),
        renewed_at: now_rfc3339(),
    };
    write_file(
        &dir.join(META),
        serde_json::to_string_pretty(&meta)
            .map_err(|error| TlsError::invalid(dir.join(META), error.to_string()))?
            .as_bytes(),
        0o644,
    )?;
    Ok(())
}

fn generate_set(dir: &Path, ca_days: i64, leaf_days: i64) -> Result<(), TlsError> {
    let ca_key = KeyPair::generate().map_err(|error| TlsError::invalid(dir, error.to_string()))?;
    let params = ca_params();
    let ca = params
        .self_signed(&ca_key)
        .map_err(|error| TlsError::invalid(dir, error.to_string()))?;
    let issuer = Issuer::from_params(&params, &ca_key);
    let (leaf, leaf_key, server_not_after) = make_leaf_with_days(&issuer, leaf_days)?;
    let ca_not_after = now_rfc3339_plus(ca_days);
    write_file(&dir.join(CA_CERT), ca.pem().as_bytes(), 0o644)?;
    write_file(&dir.join(CA_KEY), ca_key.serialize_pem().as_bytes(), 0o600)?;
    write_file(&dir.join(SERVER_CERT), leaf.pem().as_bytes(), 0o644)?;
    write_file(
        &dir.join(SERVER_KEY),
        leaf_key.serialize_pem().as_bytes(),
        0o600,
    )?;
    let meta = Meta {
        format: META_FORMAT,
        ca_not_after: Some(ca_not_after),
        server_not_after,
        sans: SANS.iter().map(|s| (*s).to_string()).collect(),
        created_at: now_rfc3339(),
        renewed_at: now_rfc3339(),
    };
    write_file(
        &dir.join(META),
        serde_json::to_string_pretty(&meta)
            .map_err(|error| TlsError::invalid(dir.join(META), error.to_string()))?
            .as_bytes(),
        0o644,
    )?;
    Ok(())
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("empty SAN list");
    params.distinguished_name.push(DnType::CommonName, CA_CN);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.use_authority_key_identifier_extension = true;
    let now = now_time();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(CA_DAYS);
    params
}

fn make_leaf(
    issuer: &Issuer<'_, &KeyPair>,
) -> Result<(rcgen::Certificate, KeyPair, String), TlsError> {
    make_leaf_with_days(issuer, LEAF_DAYS)
}

fn make_leaf_with_days(
    issuer: &Issuer<'_, &KeyPair>,
    days: i64,
) -> Result<(rcgen::Certificate, KeyPair, String), TlsError> {
    let leaf_key = KeyPair::generate().map_err(|error| TlsError::Invalid {
        path: PathBuf::from("tls"),
        detail: error.to_string(),
    })?;
    let mut params = CertificateParams::new(
        SANS.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
    )
    .map_err(|error| TlsError::Invalid {
        path: PathBuf::from("tls"),
        detail: error.to_string(),
    })?;
    params.distinguished_name.push(DnType::CommonName, LEAF_CN);
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    let now = now_time();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(days);
    let leaf = params
        .signed_by(&leaf_key, issuer)
        .map_err(|error| TlsError::Invalid {
            path: PathBuf::from("tls"),
            detail: error.to_string(),
        })?;
    Ok((leaf, leaf_key, now_rfc3339_plus(days)))
}

fn server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, TlsError> {
    if certs.is_empty() {
        return Err(TlsError::invalid("tls", "certificate chain is empty"));
    }
    let provider = Arc::new(crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TlsError::invalid("tls", error.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| {
            TlsError::invalid("tls", format!("certificate/key pair is invalid: {error}"))
        })?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|error| TlsError::Missing {
            path: path.to_path_buf(),
            detail: format!("cannot read: {error}"),
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::Invalid {
            path: path.to_path_buf(),
            detail: format!("invalid PEM: {error}"),
        })?;
    if certs.is_empty() {
        return Err(TlsError::Invalid {
            path: path.to_path_buf(),
            detail: "no certificate sections".to_string(),
        });
    }
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    if !path.is_file() {
        return Err(TlsError::Missing {
            path: path.to_path_buf(),
            detail: "file does not exist".to_string(),
        });
    }
    PrivateKeyDer::from_pem_file(path).map_err(|error| TlsError::Invalid {
        path: path.to_path_buf(),
        detail: format!("invalid private key: {error}"),
    })
}

fn check_private_key_mode(path: &Path) -> Result<(), TlsError> {
    let metadata = fs::metadata(path).map_err(|error| TlsError::Missing {
        path: path.to_path_buf(),
        detail: format!("cannot stat: {error}"),
    })?;
    if !metadata.is_file() {
        return Err(TlsError::Invalid {
            path: path.to_path_buf(),
            detail: "not a regular file".to_string(),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TlsError::Invalid {
            path: path.to_path_buf(),
            detail: format!("private key mode is {mode:o}; expected 0600"),
        });
    }
    Ok(())
}

fn create_tls_dir(path: &Path) -> Result<(), TlsError> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(path).map_err(|error| TlsError::Incomplete {
        path: path.to_path_buf(),
        detail: format!("cannot create: {error}"),
    })
}

fn check_tls_dir(path: &Path) -> Result<(), TlsError> {
    let metadata = fs::metadata(path).map_err(|error| TlsError::Missing {
        path: path.to_path_buf(),
        detail: format!("cannot stat: {error}"),
    })?;
    if !metadata.is_dir() {
        return Err(TlsError::Invalid {
            path: path.to_path_buf(),
            detail: "not a directory".to_string(),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TlsError::Invalid {
            path: path.to_path_buf(),
            detail: format!("directory mode is {mode:o}; expected 0700"),
        });
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), TlsError> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tls")
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|error| {
            TlsError::invalid(path, format!("cannot write {}: {error}", tmp.display()))
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            TlsError::invalid(path, format!("cannot write {}: {error}", tmp.display()))
        })?;
    drop(file);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|error| {
        TlsError::invalid(path, format!("cannot chmod {}: {error}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|error| {
        TlsError::invalid(path, format!("cannot replace {}: {error}", path.display()))
    })?;
    Ok(())
}

fn parse_time(value: &str, path: &Path) -> Result<DateTime<Utc>, TlsError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| TlsError::Invalid {
            path: path.to_path_buf(),
            detail: format!("invalid RFC3339 time `{value}`: {error}"),
        })
}

fn now_time() -> time::OffsetDateTime {
    time::OffsetDateTime::from(SystemTime::now())
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn now_rfc3339_plus(days: i64) -> String {
    (Utc::now() + chrono::Duration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn config() -> HttpConfig {
        HttpConfig {
            tls: true,
            ..HttpConfig::default()
        }
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn generated_set_is_private_and_reused_without_churn() {
        let temp = tempdir().unwrap();
        let first = generated(temp.path()).unwrap();
        assert_eq!(first.source, TlsSource::Generated);
        let dir = temp.path().join(TLS_DIR);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(CA_CERT)), 0o644);
        assert_eq!(mode(&dir.join(CA_KEY)), 0o600);
        assert_eq!(mode(&dir.join(SERVER_CERT)), 0o644);
        assert_eq!(mode(&dir.join(SERVER_KEY)), 0o600);
        assert_eq!(mode(&dir.join(META)), 0o644);

        let before: Vec<_> = [CA_CERT, CA_KEY, SERVER_CERT, SERVER_KEY, META]
            .iter()
            .map(|name| {
                (
                    dir.join(name),
                    read(&dir.join(name)),
                    fs::metadata(dir.join(name)).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let second = generated(temp.path()).unwrap();
        assert_eq!(second.source, TlsSource::Generated);
        for (path, bytes, modified) in before {
            assert_eq!(read(&path), bytes);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        }
    }

    #[test]
    fn a_short_leaf_is_reminted_by_the_same_ca() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, CA_DAYS, 10).unwrap();
        let ca_before = read(&dir.join(CA_CERT));
        let old_server = read(&dir.join(SERVER_CERT));
        let old_meta: serde_json::Value = serde_json::from_slice(&read(&dir.join(META))).unwrap();
        let old_not_after = old_meta["server_not_after"].as_str().unwrap().to_string();

        let material = generated(temp.path()).unwrap();
        assert_eq!(material.source, TlsSource::Generated);
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);
        assert_ne!(read(&dir.join(SERVER_CERT)), old_server);
        let new_meta: serde_json::Value = serde_json::from_slice(&read(&dir.join(META))).unwrap();
        assert_ne!(
            new_meta["server_not_after"].as_str().unwrap(),
            old_not_after
        );
    }

    ///: a leaf that is already expired (forward clock or expired while the
    /// service was stopped) is re-minted by the same CA instead of being
    /// refused for failing verification.
    #[test]
    fn an_expired_leaf_is_reminted_by_the_same_ca() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, CA_DAYS, -1).unwrap();
        let ca_before = read(&dir.join(CA_CERT));
        let old_server = read(&dir.join(SERVER_CERT));
        let old_key = read(&dir.join(SERVER_KEY));
        let old_meta: serde_json::Value = serde_json::from_slice(&read(&dir.join(META))).unwrap();
        let ca_not_after = old_meta["ca_not_after"].as_str().unwrap().to_string();

        let material = generated(temp.path()).unwrap();
        assert_eq!(material.source, TlsSource::Generated);
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);
        assert_ne!(read(&dir.join(SERVER_CERT)), old_server);
        assert_ne!(read(&dir.join(SERVER_KEY)), old_key);
        let meta: serde_json::Value = serde_json::from_slice(&read(&dir.join(META))).unwrap();
        assert!(
            parse_time(meta["server_not_after"].as_str().unwrap(), &dir).unwrap() > Utc::now(),
            "the re-minted leaf must be valid at startup"
        );
        assert_eq!(
            meta["ca_not_after"].as_str().unwrap(),
            ca_not_after,
            "renewal must preserve the recorded CA expiry"
        );
    }

    ///: lost leaf files and a lost diagnostics record are rebuilt by the
    /// intact CA; the CA bytes are never touched.
    #[test]
    fn missing_leaf_and_missing_meta_are_reminted_by_the_same_ca() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        let ca_before = read(&dir.join(CA_CERT));

        for name in [SERVER_CERT, SERVER_KEY] {
            fs::remove_file(dir.join(name)).unwrap();
        }
        generated(temp.path()).unwrap();
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);
        assert!(!read(&dir.join(SERVER_CERT)).is_empty());
        assert_eq!(mode(&dir.join(SERVER_KEY)), 0o600);

        fs::remove_file(dir.join(META)).unwrap();
        generated(temp.path()).unwrap();
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);
        let meta: serde_json::Value = serde_json::from_slice(&read(&dir.join(META))).unwrap();
        assert_eq!(meta["format"], 1);
    }

    ///: a leaf that does not chain to this CA, or whose key does not match,
    /// is still refused and left untouched.
    #[test]
    fn a_foreign_or_mismatched_leaf_is_refused_without_overwriting() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let other = tempdir().unwrap();
        let other_dir = other.path().join(TLS_DIR);
        fs::create_dir(&other_dir).unwrap();
        fs::set_permissions(&other_dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        generate_set(&other_dir, CA_DAYS, LEAF_DAYS).unwrap();
        let ca_before = read(&dir.join(CA_CERT));

        for name in [SERVER_CERT, SERVER_KEY] {
            fs::copy(other_dir.join(name), dir.join(name)).unwrap();
        }
        let foreign = read(&dir.join(SERVER_CERT));
        match generated(temp.path()) {
            Err(TlsError::Invalid { .. }) => {}
            other => panic!("expected tls_material_invalid for a foreign leaf, got {other:?}"),
        }
        assert_eq!(read(&dir.join(SERVER_CERT)), foreign);
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);

        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        let ca_key_before = read(&dir.join(CA_KEY));
        fs::copy(dir.join(CA_KEY), dir.join(SERVER_KEY)).unwrap();
        match generated(temp.path()) {
            Err(TlsError::Invalid { .. }) => {}
            other => panic!("expected tls_material_invalid for a mismatched pair, got {other:?}"),
        }
        assert_eq!(read(&dir.join(CA_KEY)), ca_key_before);
        assert_eq!(read(&dir.join(SERVER_KEY)), ca_key_before);
    }

    #[test]
    fn an_expired_ca_is_typed_and_never_overwritten() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, -1, 10).unwrap();
        let ca_before = read(&dir.join(CA_CERT));
        match generated(temp.path()) {
            Err(TlsError::CaExpired { .. }) => {}
            other => panic!("expected tls_ca_expired, got {other:?}"),
        }
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);
    }

    #[test]
    fn partial_corrupt_and_widened_states_are_typed_and_not_overwritten() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join(TLS_DIR);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        let ca_before = read(&dir.join(CA_CERT));

        // The CA is never rebuilt in place: a partial CA is a typed error.
        fs::remove_file(dir.join(CA_KEY)).unwrap();
        match generated(temp.path()) {
            Err(TlsError::Incomplete { .. }) => {}
            other => panic!("expected tls_material_incomplete, got {other:?}"),
        }
        assert_eq!(read(&dir.join(CA_CERT)), ca_before);

        // restore a complete set, then corrupt the leaf
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        fs::write(dir.join(SERVER_CERT), b"not a certificate").unwrap();
        let corrupt = read(&dir.join(SERVER_CERT));
        match generated(temp.path()) {
            Err(TlsError::Invalid { .. }) | Err(TlsError::Missing { .. }) => {}
            other => panic!("expected tls_material_invalid, got {other:?}"),
        }
        assert_eq!(read(&dir.join(SERVER_CERT)), corrupt);

        // restore, widen the leaf key mode
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        fs::set_permissions(dir.join(SERVER_KEY), fs::Permissions::from_mode(0o644)).unwrap();
        match generated(temp.path()) {
            Err(TlsError::Invalid { .. }) => {}
            other => panic!("expected widened-key invalid, got {other:?}"),
        }

        // restore, widen the CA key mode
        generate_set(&dir, CA_DAYS, LEAF_DAYS).unwrap();
        fs::set_permissions(dir.join(CA_KEY), fs::Permissions::from_mode(0o644)).unwrap();
        match generated(temp.path()) {
            Err(TlsError::Invalid { .. }) => {}
            other => panic!("expected widened CA key invalid, got {other:?}"),
        }
    }

    #[test]
    fn override_precedence_leaves_generated_material_untouched() {
        let temp = tempdir().unwrap();
        generated(temp.path()).unwrap();
        let dir = temp.path().join(TLS_DIR);
        let ca_mtime = fs::metadata(dir.join(CA_CERT)).unwrap().modified().unwrap();
        let ca_bytes = read(&dir.join(CA_CERT));
        let override_config = HttpConfig {
            tls: true,
            tls_cert_file: Some(dir.join(SERVER_CERT)),
            tls_key_file: Some(dir.join(SERVER_KEY)),
        };
        let material = load(
            &override_config,
            temp.path(),
            "127.0.0.1:8765".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(material.source, TlsSource::Override);
        assert!(material.ca_path.is_none());
        assert_eq!(read(&dir.join(CA_CERT)), ca_bytes);
        assert_eq!(
            fs::metadata(dir.join(CA_CERT)).unwrap().modified().unwrap(),
            ca_mtime
        );
    }

    #[test]
    fn invalid_or_missing_override_names_the_pair_without_falling_back() {
        let temp = tempdir().unwrap();
        generated(temp.path()).unwrap();
        let dir = temp.path().join(TLS_DIR);
        let missing = HttpConfig {
            tls: true,
            tls_cert_file: Some(dir.join(SERVER_CERT)),
            tls_key_file: Some(temp.path().join("missing-key.pem")),
        };
        match load(&missing, temp.path(), "127.0.0.1:8765".parse().unwrap()) {
            Err(TlsError::Missing { path, .. }) => assert!(path.ends_with("missing-key.pem")),
            other => panic!("expected missing override, got {other:?}"),
        }

        let mismatched = HttpConfig {
            tls: true,
            tls_cert_file: Some(dir.join(SERVER_CERT)),
            tls_key_file: Some(dir.join(CA_KEY)),
        };
        match load(&mismatched, temp.path(), "127.0.0.1:8765".parse().unwrap()) {
            Err(TlsError::Invalid { detail, .. }) => {
                assert!(
                    detail.contains("tls_cert_file") && detail.contains("tls_key_file"),
                    "{detail}"
                )
            }
            other => panic!("expected mismatched override, got {other:?}"),
        }
    }

    #[test]
    fn generated_material_rejects_non_loopback_and_override_is_both_or_neither() {
        let temp = tempdir().unwrap();
        let address: SocketAddr = "192.0.2.10:8765".parse().unwrap();
        match load(&config(), temp.path(), address) {
            Err(TlsError::BindNotLoopback { .. }) => {}
            other => panic!("expected tls_bind_not_loopback, got {other:?}"),
        }
        assert!(!temp.path().join(TLS_DIR).exists());

        let one = HttpConfig {
            tls: true,
            tls_cert_file: Some(temp.path().join("cert.pem")),
            tls_key_file: None,
        };
        match load(&one, temp.path(), "127.0.0.1:8765".parse().unwrap()) {
            Err(TlsError::OverrideIncomplete) => {}
            other => panic!("expected tls_override_incomplete, got {other:?}"),
        }
    }
}
