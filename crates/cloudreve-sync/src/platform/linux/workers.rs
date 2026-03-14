use super::cache_impl::CacheManagerImpl;
use super::inode::{InodeCacheState, InodeDb};
use crate::drive::commands::MountCommand;
use crate::drive::sync::SyncMode;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the background upload worker that picks up dirty files and triggers sync/upload.
///
/// Every 2 seconds, queries the InodeDb for dirty inodes that haven't been modified
/// in the last 3 seconds, then dispatches sync commands for each.
pub fn spawn_upload_worker(
    inode_db: Arc<InodeDb>,
    command_tx: mpsc::UnboundedSender<MountCommand>,
    sync_path: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));

        loop {
            interval.tick().await;

            let dirty_entries = inode_db.list_dirty_older_than(3);
            if dirty_entries.is_empty() {
                continue;
            }

            let mut paths_to_sync = Vec::new();
            for entry in &dirty_entries {
                if let Some(relative) = inode_db.resolve_path(entry.inode) {
                    let stripped = relative.strip_prefix("/").unwrap_or(&relative);
                    let local_path = sync_path.join(stripped);
                    paths_to_sync.push(local_path);
                }
            }

            if !paths_to_sync.is_empty() {
                tracing::debug!(
                    target: "fuse::upload_worker",
                    count = paths_to_sync.len(),
                    "Dispatching sync for dirty files"
                );
                let _ = command_tx.send(MountCommand::Sync {
                    local_paths: paths_to_sync,
                    mode: SyncMode::PathOnly,
                });
            }
        }
    })
}

/// Spawn the background eviction worker that frees cache space when it gets too full.
///
/// Every 5 minutes, checks if cache usage exceeds 90% of max. If so, evicts
/// the oldest cached (non-dirty, non-pinned) files until usage drops below 70%.
pub fn spawn_eviction_worker(
    inode_db: Arc<InodeDb>,
    cache: Arc<CacheManagerImpl>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));

        loop {
            interval.tick().await;

            let current = cache.current_size();
            let max = cache.max_size_bytes();
            let high_watermark = (max as f64 * 0.9) as u64;
            let low_watermark = (max as f64 * 0.7) as u64;

            if current <= high_watermark {
                continue;
            }

            tracing::info!(
                target: "fuse::eviction_worker",
                current_mb = current / (1024 * 1024),
                max_mb = max / (1024 * 1024),
                "Cache exceeds high watermark, starting eviction"
            );

            let evictable = inode_db.list_evictable();
            let mut freed = 0u64;

            for entry in evictable {
                if current - freed <= low_watermark {
                    break;
                }

                let cache_path = cache.cache_path_for_inode(entry.inode);
                let file_size = std::fs::metadata(&cache_path).map(|m| m.len()).unwrap_or(0);

                if let Err(e) = cache.evict_inode(entry.inode) {
                    tracing::warn!(
                        target: "fuse::eviction_worker",
                        inode = entry.inode,
                        error = %e,
                        "Failed to evict cache entry"
                    );
                    continue;
                }

                freed += file_size;
                tracing::debug!(
                    target: "fuse::eviction_worker",
                    inode = entry.inode,
                    name = %entry.name,
                    size = file_size,
                    "Evicted cache entry"
                );
            }

            // Recalculate actual size after eviction
            cache.recalculate_size();

            tracing::info!(
                target: "fuse::eviction_worker",
                freed_mb = freed / (1024 * 1024),
                new_size_mb = cache.current_size() / (1024 * 1024),
                "Eviction complete"
            );
        }
    })
}
