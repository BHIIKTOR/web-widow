//! Persistence: SQLite index + filesystem content-addressed blobs.
//!
//! The store is the only module that touches the data directory, and the only
//! one that writes rows. Retention defaults live in `config::RetentionConfig`
//! and are applied by [`Store::gc`] (startup and after heavy batches).

pub mod blob;
pub mod compare;
pub mod corpus;
pub mod datadir;
pub mod db;
pub mod embeddings;
pub mod fts;
pub mod jobs;
pub mod monitor;
pub mod recipes;
pub mod webhooks;

use std::sync::Arc;

use crate::config::{FtsConfig, Paths, RetentionConfig};
use crate::core::error::Result;

pub use blob::BlobStore;
pub use compare::PageVersionRow;
pub use corpus::CorpusPageRow;
pub use datadir::{DataDir, write_private};
pub use db::{
    AuthAuditRow, BlobRow, CachedExtraction, ContentHitRow, ContentHits, Database, ExtractionRow,
    FetchRecord, GcReport, HostRow, IndexText, IndexWrite, PageRecord, ProfileRow, Recorded,
};
pub use embeddings::{EmbeddingCandidate, EmbeddingCounts, NewEmbedding, SemanticChunkRow};
pub use jobs::{CrawlResultRow, FrontierCounts, JobCounters, JobRow, LeasedUrl, NewJob, Seed};
pub use monitor::{
    ChangeRow, CheckRecord, MonitorRow, NewChangeDraft, NewMonitor, NewSnapshot, SnapshotRow,
};
pub use recipes::{NewRecipe, RecipeListItem, RecipeRow, RecipeSaved, RecipeVersionRow};
pub use webhooks::{
    DeliveryContext, DeliveryRow, DeliveryUpdate, DueDelivery, MonitorAttachmentRow,
};

/// Read connections used by the pool. has one reader at a
/// time; the pool exists so later batches do not change the store's shape.
pub const READ_CONNECTIONS: usize = 4;

#[derive(Debug)]
pub struct Store {
    pub db: Arc<Database>,
    pub blobs: BlobStore,
    paths: Paths,
}

impl Store {
    /// Open (and migrate) the store. `fts` supplies the index-text cap the v6
    /// backfill applies to the pages that predate the index.
    pub fn open(paths: &Paths, fts: &FtsConfig, index_authenticated: bool) -> Result<Self> {
        let db = Arc::new(Database::open(
            &paths.database(),
            READ_CONNECTIONS,
            fts,
            index_authenticated,
        )?);
        let blobs = BlobStore::new(paths.blobs());
        Ok(Self {
            db,
            blobs,
            paths: paths.clone(),
        })
    }

    /// The data-directory layout this store was opened with.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Store raw bytes in the CAS, returning the BLAKE3 hash.
    pub async fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        self.blobs.put(bytes).await
    }

    /// Prune expired rows and enforce the blob budget, then remove the files
    /// whose rows were deleted. Idempotent and safe to interrupt.
    pub async fn gc(
        &self,
        retention: RetentionConfig,
        auth_audit_days: u32,
        auth_page_days: u32,
    ) -> Result<GcReport> {
        let report = self
            .db
            .gc(retention, auth_audit_days, auth_page_days)
            .await?;
        for hash in &report.removed_blobs {
            if let Err(error) = self.blobs.remove(hash).await {
                tracing::warn!(hash = %hash, error = %error, "could not remove evicted blob");
            }
        }
        Ok(report)
    }
}
