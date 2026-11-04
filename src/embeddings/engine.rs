//! Local ONNX embedding engine (v0.7).
//!
//! The engine is local and offline by construction: `fastembed` is built with
//! `ort-load-dynamic` (no `hf-hub`, no `ureq`, no native-tls, no image models)
//! and the model/tokenizer files are read from a provisioned directory whose
//! per-file SHA-256 is checked against the pinned manifest. A missing or
//! mismatched artifact is the typed `embedding_unavailable`, never a fallback.
//!
//! ONNX Runtime itself is provisioned as `libonnxruntime.so` and loaded through
//! `ort::init_from`; the search order is the strict config path, then
//! `WEB_WIDOW_ORT_PATH`, then `ORT_DYLIB_PATH`, then `<exe dir>`, then
//! `<data_dir>/lib`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use ort::environment::Environment;
use sha2::{Digest, Sha256};

use crate::config::EmbeddingsConfig;
use crate::core::error::{Result, WidowError};

/// The only model id in v1.
pub const MODEL_ID: &str = "minilm-l6-v2-q";
/// `all-MiniLM-L6-v2` output dimensions.
pub const MODEL_DIM: usize = 384;
/// `all-MiniLM-L6-v2` input window.
const MODEL_MAX_LENGTH: usize = 256;
/// Intra-op threads for one engine: bounded so a backfill cannot pin every core.
const INTRA_THREADS: usize = 4;

/// One pinned model file: relative path, byte length and SHA-256.
pub struct ModelFile {
    pub relative: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// The built-in manifest for one model id.
pub struct ModelManifest {
    pub id: &'static str,
    pub dim: usize,
    pub files: &'static [ModelFile],
}

/// `Xenova/all-MiniLM-L6-v2` quantized, pinned 2026-11-02.
pub const MINILM_L6_V2_Q: ModelManifest = ModelManifest {
    id: MODEL_ID,
    dim: MODEL_DIM,
    files: &[
        ModelFile {
            relative: "onnx/model_quantized.onnx",
            bytes: 22_972_370,
            sha256: "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1",
        },
        ModelFile {
            relative: "tokenizer.json",
            bytes: 711_661,
            sha256: "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
        },
        ModelFile {
            relative: "config.json",
            bytes: 650,
            sha256: "7135149f7cffa1a573466c6e4d8423ed73b62fd2332c575bf738a0d033f70df7",
        },
        ModelFile {
            relative: "special_tokens_map.json",
            bytes: 125,
            sha256: "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
        },
        ModelFile {
            relative: "tokenizer_config.json",
            bytes: 366,
            sha256: "9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3",
        },
    ],
};

/// The manifest for a configured model id; v1 has exactly one.
pub fn manifest(model: &str) -> Option<&'static ModelManifest> {
    (model == MODEL_ID).then_some(&MINILM_L6_V2_Q)
}

/// Resolved artifact locations for one engine.
#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub library: PathBuf,
    pub model_dir: PathBuf,
}

impl EnginePaths {
    /// The strict config path/name, or the discovery description for an error.
    pub fn library_source(&self, config: &EmbeddingsConfig) -> String {
        match config.ort_dylib_path.as_deref() {
            Some(path) => format!("[embeddings].ort_dylib_path ({})", path.display()),
            None => self.library.display().to_string(),
        }
    }
}

/// Resolve the library and model directory without loading anything.
///
/// A strict `ort_dylib_path`/`model_dir` is used exactly and a missing file is
/// typed, naming the config key. Discovery lists every path it searched.
pub fn resolve_paths(
    config: &EmbeddingsConfig,
    data_dir: &Path,
    manifest: &ModelManifest,
) -> Result<EnginePaths> {
    let library = resolve_library(config, data_dir)?;
    let model_dir = match &config.model_dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(WidowError::EmbeddingUnavailable(format!(
                    "[embeddings].model_dir ({}) is not a directory",
                    dir.display()
                )));
            }
            dir.clone()
        }
        None => data_dir.join("models").join(manifest.id),
    };
    if !model_dir.is_dir() {
        return Err(WidowError::EmbeddingUnavailable(format!(
            "model directory {} does not exist; provision the pinned {model_id} files or set [embeddings].model_dir",
            model_dir.display(),
            model_id = manifest.id
        )));
    }
    Ok(EnginePaths { library, model_dir })
}

fn resolve_library(config: &EmbeddingsConfig, data_dir: &Path) -> Result<PathBuf> {
    if let Some(path) = &config.ort_dylib_path {
        if !path.is_file() {
            return Err(WidowError::EmbeddingUnavailable(format!(
                "[embeddings].ort_dylib_path ({}) is not a file",
                path.display()
            )));
        }
        return Ok(path.clone());
    }
    let mut searched = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for variable in ["WEB_WIDOW_ORT_PATH", "ORT_DYLIB_PATH"] {
        if let Some(value) = std::env::var_os(variable) {
            let path = PathBuf::from(value);
            if !path.as_os_str().is_empty() {
                candidates.push(path);
            }
        }
    }
    if let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        candidates.push(exe.join("libonnxruntime.so"));
    }
    candidates.push(data_dir.join("lib").join("libonnxruntime.so"));
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
        searched.push(candidate.display().to_string());
    }
    Err(WidowError::EmbeddingUnavailable(format!(
        "libonnxruntime.so was not found (searched: {}); set [embeddings].ort_dylib_path, WEB_WIDOW_ORT_PATH, or install it beside the binary or in <data_dir>/lib",
        if searched.is_empty() {
            "no candidate paths".to_string()
        } else {
            searched.join(", ")
        }
    )))
}

/// Verify every pinned model file: byte length and SHA-256.
///
/// Returns the file bytes in manifest order. A mismatch names the file and the
/// config key so an operator can replace the artifact.
pub fn verify_model_files(model_dir: &Path, manifest: &ModelManifest) -> Result<Vec<Vec<u8>>> {
    let mut contents = Vec::with_capacity(manifest.files.len());
    for file in manifest.files {
        let path = model_dir.join(file.relative);
        let bytes = std::fs::read(&path).map_err(|error| {
            WidowError::EmbeddingUnavailable(format!(
                "model file {} is not readable ({}): {error}; set [embeddings].model_dir or re-provision the pinned model",
                file.relative,
                path.display()
            ))
        })?;
        if bytes.len() as u64 != file.bytes {
            return Err(WidowError::EmbeddingUnavailable(format!(
                "model file {} is {} bytes, expected {} (sha256 {})",
                file.relative,
                bytes.len(),
                file.bytes,
                file.sha256
            )));
        }
        let digest = sha256_hex(&bytes);
        if digest != file.sha256 {
            return Err(WidowError::EmbeddingUnavailable(format!(
                "model file {} has sha256 {digest}, expected {}; re-provision the pinned model",
                file.relative, file.sha256
            )));
        }
        contents.push(bytes);
    }
    Ok(contents)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// ONNX Runtime allows exactly one environment per process. It is initialized
/// once from the first resolved library and shared by every engine (production
/// has one; tests create several services in one process).
static ENVIRONMENT: OnceLock<std::result::Result<Environment, String>> = OnceLock::new();

fn shared_environment(library: &Path) -> Result<Environment> {
    let environment = ENVIRONMENT.get_or_init(|| {
        ort::init_from(library)
            .map_err(|error| {
                format!(
                    "cannot load ONNX Runtime from {}: {error}",
                    library.display()
                )
            })?
            .with_name("web-widow")
            .build()
            .map_err(|error| {
                format!(
                    "cannot create the ONNX Runtime environment from {}: {error:?}",
                    library.display()
                )
            })
    });
    environment
        .clone()
        .map_err(WidowError::EmbeddingUnavailable)
}

/// The loaded local engine. One `TextEmbedding` behind a mutex; inference runs
/// on a blocking thread supplied by the caller.
pub struct Engine {
    model: &'static str,
    dim: usize,
    embedder: Mutex<TextEmbedding>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Engine")
            .field("model", &self.model)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Load the native library and the pinned model files.
    pub fn load(paths: &EnginePaths, manifest: &'static ModelManifest) -> Result<Self> {
        let files = verify_model_files(&paths.model_dir, manifest)?;
        let environment = shared_environment(&paths.library)?;

        let [onnx, tokenizer, config, special_tokens, tokenizer_config] =
            <[Vec<u8>; 5]>::try_from(files).map_err(|_| {
                WidowError::EmbeddingUnavailable(
                    "the pinned model manifest must list exactly five files".to_string(),
                )
            })?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: tokenizer,
            config_file: config,
            special_tokens_map_file: special_tokens,
            tokenizer_config_file: tokenizer_config,
        };
        let model = UserDefinedEmbeddingModel::new(onnx, tokenizer_files)
            .with_quantization(QuantizationMode::Dynamic)
            .with_pooling(Pooling::Mean);
        let options = InitOptionsUserDefined::new()
            .with_environment(environment)
            .with_max_length(MODEL_MAX_LENGTH)
            .with_intra_threads(INTRA_THREADS);
        let embedder =
            TextEmbedding::try_new_from_user_defined(model, options).map_err(|error| {
                WidowError::EmbeddingUnavailable(format!(
                    "the pinned {} model could not be loaded from {}: {error}",
                    manifest.id,
                    paths.model_dir.display()
                ))
            })?;
        Ok(Self {
            model: manifest.id,
            dim: manifest.dim,
            embedder: Mutex::new(embedder),
        })
    }

    pub fn model(&self) -> &'static str {
        self.model
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed a bounded batch. Every vector must have the manifest dimension.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut embedder = self
            .embedder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A dynamically quantized model refuses batching (fastembed guards it),
        // so the batch size is `None`; the caller already bounds the text list.
        let vectors = embedder.embed(texts, None).map_err(|error| {
            WidowError::EmbeddingFailed(format!("the embedding model failed: {error}"))
        })?;
        if vectors.len() != texts.len() {
            return Err(WidowError::EmbeddingFailed(format!(
                "the embedding model returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }
        for vector in &vectors {
            if vector.len() != self.dim {
                return Err(WidowError::EmbeddingFailed(format!(
                    "the embedding model returned {} dimensions, expected {}",
                    vector.len(),
                    self.dim
                )));
            }
        }
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ww-e1-engine-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_only_v1_model_id_resolves_to_the_pinned_manifest() {
        assert_eq!(manifest(MODEL_ID).unwrap().dim, 384);
        assert!(manifest("multilingual-e5-small").is_none());
    }

    #[test]
    fn a_strict_missing_library_names_the_config_key() {
        let dir = scratch_dir("lib");
        let config = EmbeddingsConfig {
            ort_dylib_path: Some(dir.join("missing.so")),
            ..EmbeddingsConfig::default()
        };
        let error = resolve_paths(&config, &dir, &MINILM_L6_V2_Q).unwrap_err();
        assert!(matches!(&error, WidowError::EmbeddingUnavailable(message)
            if message.contains("[embeddings].ort_dylib_path") && message.contains("missing.so")));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_strict_missing_model_dir_names_the_config_key() {
        let dir = scratch_dir("model");
        let library = dir.join("libonnxruntime.so");
        std::fs::write(&library, b"not really a library").unwrap();
        let config = EmbeddingsConfig {
            ort_dylib_path: Some(library),
            model_dir: Some(dir.join("absent-model")),
            ..EmbeddingsConfig::default()
        };
        let error = resolve_paths(&config, &dir, &MINILM_L6_V2_Q).unwrap_err();
        assert!(matches!(&error, WidowError::EmbeddingUnavailable(message)
            if message.contains("[embeddings].model_dir")));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_mismatched_model_file_names_the_file_and_expected_sha() {
        let dir = scratch_dir("sha");
        let onnx = dir.join("onnx");
        std::fs::create_dir_all(&onnx).unwrap();
        std::fs::write(onnx.join("model_quantized.onnx"), b"tampered").unwrap();
        let error = verify_model_files(&dir, &MINILM_L6_V2_Q).unwrap_err();
        let WidowError::EmbeddingUnavailable(message) = &error else {
            panic!("expected embedding_unavailable, got {error:?}");
        };
        assert!(message.contains("onnx/model_quantized.onnx"));
        assert!(message.contains("afdb6f1a"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
