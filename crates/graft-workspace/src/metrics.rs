//! Lane counters, read from the catalog and oplog themselves.
//!
//! Gauges (stored blobs, current oplog backlog) come from the tables; the
//! cumulative ingest, sweep and release-stage counters are maintained in
//! `catalog_meta` as operations happen. There is no separate monitoring stack.

use thiserror::Error;

use crate::{CatalogErr, OplogErr, Workspace};

#[derive(Debug, Error)]
pub enum MetricsErr {
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogErr),

    #[error("oplog: {0}")]
    Oplog(#[from] OplogErr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageMetric {
    pub storage: String,
    pub blobs: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneMetrics {
    pub oplog_docs: i64,
    pub oplog_frames: i64,
    pub oplog_bytes: i64,
    pub storage: Vec<StorageMetric>,
    pub pinned_blobs: i64,
    pub releases: i64,
    pub ingested_blobs: i64,
    pub ingested_bytes: i64,
    pub swept_blobs: i64,
    pub swept_bytes: i64,
    pub release_stages: i64,
    pub release_stage_millis: i64,
}

pub fn collect(workspace: &Workspace) -> Result<LaneMetrics, MetricsErr> {
    let docs = workspace.oplog().docs()?;
    let oplog_docs = docs.len() as i64;
    let oplog_frames = docs.iter().map(|doc| doc.frames).sum();
    let oplog_bytes = docs.iter().map(|doc| doc.bytes).sum();

    let catalog = workspace.catalog();
    let storage = catalog
        .storage_stats()?
        .into_iter()
        .map(|(storage, blobs, bytes)| StorageMetric { storage, blobs, bytes })
        .collect();
    let status = catalog.status()?;

    Ok(LaneMetrics {
        oplog_docs,
        oplog_frames,
        oplog_bytes,
        storage,
        pinned_blobs: status.pinned_blobs,
        releases: status.releases,
        ingested_blobs: catalog.metric("metric.blob.ingested.count")?,
        ingested_bytes: catalog.metric("metric.blob.ingested.bytes")?,
        swept_blobs: catalog.metric("metric.blob.swept.count")?,
        swept_bytes: catalog.metric("metric.blob.swept.bytes")?,
        release_stages: catalog.metric("metric.release.stage.count")?,
        release_stage_millis: catalog.metric("metric.release.stage.millis")?,
    })
}
