use super::inode::{InodeCacheState, InodeDb};
use crate::platform::cache::{CacheEntryState, CacheManager};
use crate::platform::error::{PlatformError, PlatformResult};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Cache manager implementation backed by inode-keyed files on disk.
///
/// Cache files are stored at `<cache_dir>/<inode_hex>` (flat, no subdirectories).
/// This means cache files survive file renames since inodes don't change.
pub struct CacheManagerImpl {
    cache_dir: PathBuf,
    inode_db: Arc<InodeDb>,
    max_size_bytes: u64,
    current_size: AtomicU64,
}

impl CacheManagerImpl {
    pub fn new(
        cache_dir: PathBuf,
        inode_db: Arc<InodeDb>,
        max_size_bytes: u64,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;

        // Calculate current cache size from existing files
        let mut size = 0u64;
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    size += meta.len();
                }
            }
        }

        Ok(Self {
            cache_dir,
            inode_db,
            max_size_bytes,
            current_size: AtomicU64::new(size),
        })
    }

    /// Get the cache file path for a given inode.
    pub fn cache_path_for_inode(&self, inode: u64) -> PathBuf {
        self.cache_dir.join(format!("{:016x}", inode))
    }

    /// Try to read data from cache without blocking. Returns None on cache miss.
    pub fn try_read(&self, inode: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        let entry = self.inode_db.get(inode)?;
        match entry.cache_state {
            InodeCacheState::Cached | InodeCacheState::Dirty => {}
            InodeCacheState::NotCached => return None,
        }

        let path = self.cache_path_for_inode(inode);
        let file = std::fs::File::open(&path).ok()?;

        use std::io::{Read, Seek, SeekFrom};
        let mut file = file;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read(&mut buf).ok()?;
        buf.truncate(n);

        // Update access time in background (non-critical)
        let _ = self.inode_db.touch_accessed(inode);

        Some(buf)
    }

    /// Write data to cache file at the given offset.
    pub fn write_to_cache(&self, inode: u64, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        let path = self.cache_path_for_inode(inode);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)?;

        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;

        // Update current size tracking
        if let Ok(meta) = std::fs::metadata(&path) {
            // This is approximate; we don't track previous size per-file
            self.current_size
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            let _ = meta; // suppress unused warning
        }

        Ok(())
    }

    /// Delete a cache file and update size tracking.
    pub fn evict_inode(&self, inode: u64) -> anyhow::Result<()> {
        let path = self.cache_path_for_inode(inode);
        if path.exists() {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            std::fs::remove_file(&path)?;
            self.current_size.fetch_sub(size, Ordering::Relaxed);
        }
        self.inode_db
            .set_cache_state(inode, InodeCacheState::NotCached)?;
        Ok(())
    }

    /// Mark an inode as fully cached after download completes.
    pub fn mark_inode_cached(&self, inode: u64) -> anyhow::Result<()> {
        self.inode_db
            .set_cache_state(inode, InodeCacheState::Cached)?;
        Ok(())
    }

    /// Mark an inode as dirty after local write.
    pub fn mark_inode_dirty(&self, inode: u64) -> anyhow::Result<()> {
        self.inode_db
            .set_cache_state(inode, InodeCacheState::Dirty)?;
        Ok(())
    }

    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    pub fn inode_db(&self) -> &Arc<InodeDb> {
        &self.inode_db
    }

    /// Recalculate current cache size from disk (for accuracy after restarts).
    pub fn recalculate_size(&self) {
        let mut size = 0u64;
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    size += meta.len();
                }
            }
        }
        self.current_size.store(size, Ordering::SeqCst);
    }
}

impl CacheManager for CacheManagerImpl {
    fn get_state(&self, path: &Path) -> CacheEntryState {
        let entry = match self.inode_db.find_by_path(path) {
            Some(e) => e,
            None => return CacheEntryState::NotCached,
        };

        match entry.cache_state {
            InodeCacheState::NotCached => CacheEntryState::NotCached,
            InodeCacheState::Cached => CacheEntryState::FullyCached,
            InodeCacheState::Dirty => CacheEntryState::Dirty {
                ranges: vec![0..entry.size as u64],
            },
        }
    }

    fn cache_path(&self, path: &Path) -> PathBuf {
        match self.inode_db.find_by_path(path) {
            Some(entry) => self.cache_path_for_inode(entry.inode),
            None => self.cache_dir.join("__missing__"),
        }
    }

    fn allocate(&self, path: &Path, size: u64) -> PlatformResult<PathBuf> {
        let entry = self
            .inode_db
            .find_by_path(path)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        let cache_path = self.cache_path_for_inode(entry.inode);

        // Create or truncate the cache file with the expected size
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&cache_path)
            .map_err(|e| PlatformError::Failed(format!("failed to allocate cache file: {}", e)))?;

        file.set_len(size)
            .map_err(|e| PlatformError::Failed(format!("failed to set cache file size: {}", e)))?;

        self.current_size.fetch_add(size, Ordering::Relaxed);
        Ok(cache_path)
    }

    fn mark_cached(&self, path: &Path, _range: Range<u64>) -> PlatformResult<()> {
        let entry = self
            .inode_db
            .find_by_path(path)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        self.inode_db
            .set_cache_state(entry.inode, InodeCacheState::Cached)
            .map_err(|e| PlatformError::Failed(e.to_string()))
    }

    fn mark_dirty(&self, path: &Path, _range: Range<u64>) -> PlatformResult<()> {
        let entry = self
            .inode_db
            .find_by_path(path)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        self.inode_db
            .set_cache_state(entry.inode, InodeCacheState::Dirty)
            .map_err(|e| PlatformError::Failed(e.to_string()))
    }

    fn list_dirty(&self) -> Vec<(PathBuf, Vec<Range<u64>>)> {
        self.inode_db
            .list_dirty_older_than(0)
            .into_iter()
            .filter_map(|entry| {
                let path = self.inode_db.resolve_path(entry.inode)?;
                Some((path, vec![0..entry.size as u64]))
            })
            .collect()
    }

    fn mark_clean(&self, path: &Path) -> PlatformResult<()> {
        let entry = self
            .inode_db
            .find_by_path(path)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        self.inode_db
            .set_cache_state(entry.inode, InodeCacheState::Cached)
            .map_err(|e| PlatformError::Failed(e.to_string()))
    }

    fn evict(&self, path: &Path) -> PlatformResult<()> {
        let entry = self
            .inode_db
            .find_by_path(path)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        self.evict_inode(entry.inode)
            .map_err(|e| PlatformError::Failed(e.to_string()))
    }

    fn current_size(&self) -> u64 {
        self.current_size.load(Ordering::Relaxed)
    }
}
