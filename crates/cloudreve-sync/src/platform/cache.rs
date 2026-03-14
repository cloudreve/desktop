//! Cache layer design for non-cfapi platforms (e.g., Linux FUSE 2).
//!
//! FUSE has no built-in placeholder/hydration concept, so a local cache layer is needed:
//!
//! 1. A cache directory mirrors the virtual mount with real file data
//! 2. On read: serve from cache or fetch from server
//! 3. On write: write to cache, mark dirty, queue background upload
//! 4. LRU eviction when cache exceeds size limit
//!
//! ## Read path
//! ```text
//! FUSE read(path, offset, size)
//!   -> CacheManager::get_state(path)
//!   -> if FullyCached: read from cache_path
//!   -> if NotCached/Partial:
//!      -> allocate cache space (evict LRU if needed)
//!      -> fetch data from Cloudreve server (range request)
//!      -> write to cache file
//!      -> mark_cached(range)
//!      -> return data to FUSE
//! ```
//!
//! ## Write path (non-blocking for user)
//! ```text
//! FUSE write(path, offset, data)
//!   -> write to cache file immediately (fast, local I/O)
//!   -> mark_dirty(path, range)
//!   -> return success to user immediately
//!   -> Background worker: pick up dirty files, upload via existing uploader module
//!   -> On upload complete: mark_clean(path)
//! ```
//!
//! ## Eviction
//! ```text
//! Background evictor runs when current_size() > max_size * 0.9
//!   -> sort cache entries by last access time
//!   -> evict oldest entries that are NOT dirty
//!   -> never evict dirty files (they have un-uploaded changes)
//! ```

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::error::PlatformResult;

/// State of a cached file entry.
#[derive(Debug, Clone)]
pub enum CacheEntryState {
    /// No data cached on disk — metadata only.
    NotCached,
    /// Some byte ranges are cached.
    PartiallyCached { ranges: Vec<Range<u64>> },
    /// All data is present in cache.
    FullyCached,
    /// Modified locally, pending upload to server.
    Dirty { ranges: Vec<Range<u64>> },
}

/// Configuration for the cache layer.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub cache_dir: PathBuf,
    /// Maximum cache size in bytes (e.g., 10 GB).
    pub max_size_bytes: u64,
    pub eviction_policy: EvictionPolicy,
}

/// Cache eviction policy.
#[derive(Debug, Clone)]
pub enum EvictionPolicy {
    /// Least recently used.
    Lru,
    /// LRU but don't evict files accessed within this duration.
    LruWithMinAge(Duration),
}

/// Trait for managing the local file cache.
///
/// This is the core abstraction for FUSE-based sync providers that need to
/// maintain local copies of remote files.
pub trait CacheManager: Send + Sync {
    /// Get the current cache state for a path.
    fn get_state(&self, path: &Path) -> CacheEntryState;

    /// Get the local cache file path where real data is stored.
    fn cache_path(&self, path: &Path) -> PathBuf;

    /// Allocate space for caching a file, evicting if needed.
    /// Returns the path where data should be written.
    fn allocate(&self, path: &Path, size: u64) -> PlatformResult<PathBuf>;

    /// Mark a byte range as cached after successful download.
    fn mark_cached(&self, path: &Path, range: Range<u64>) -> PlatformResult<()>;

    /// Mark a byte range as dirty after a local write.
    fn mark_dirty(&self, path: &Path, range: Range<u64>) -> PlatformResult<()>;

    /// List all dirty files and their modified ranges (for background upload worker).
    fn list_dirty(&self) -> Vec<(PathBuf, Vec<Range<u64>>)>;

    /// Clear dirty flag after successful upload.
    fn mark_clean(&self, path: &Path) -> PlatformResult<()>;

    /// Evict a file from cache (delete cached data, keep metadata entry).
    fn evict(&self, path: &Path) -> PlatformResult<()>;

    /// Get total cache size currently on disk.
    fn current_size(&self) -> u64;
}
