use std::path::Path;

use super::error::{PlatformError, PlatformResult};
use super::provider::{ProviderConfig, SyncProvider, UpdateOptions};
use super::types::{FileMetadataPlatform, LocalFileState, PlaceholderEntry};

/// Linux FUSE 2 implementation of `SyncProvider`.
///
/// This is a stub with TODO markers. The actual implementation will:
/// - Use FUSE 2 to present a virtual filesystem at the sync path
/// - Use [`super::cache::CacheManager`] for local data caching
/// - Handle read/write through the cache layer
pub struct LinuxFuseProvider {
    // TODO: FUSE session handle (fuse2rs::Session or similar)
    // TODO: CacheManager instance for local file caching
    // TODO: mount point path
    // TODO: background thread handle for FUSE event loop
}

impl LinuxFuseProvider {
    pub fn new() -> Self {
        Self {}
    }
}

impl SyncProvider for LinuxFuseProvider {
    fn start(&mut self, _config: &ProviderConfig) -> PlatformResult<()> {
        todo!("FUSE 2: mount filesystem at config.sync_path, start FUSE event loop in background thread, initialize cache layer")
    }

    fn stop(&mut self) -> PlatformResult<()> {
        todo!("FUSE 2: unmount filesystem, stop FUSE event loop, flush dirty cache entries")
    }

    fn unregister(&mut self) -> PlatformResult<()> {
        todo!("FUSE 2: unmount, clean up cache directory, remove any persistent registration (e.g. xdg autostart)")
    }

    fn get_file_state(&self, _path: &Path) -> LocalFileState {
        todo!("FUSE 2: check cache state for path — if cached return exists+hydrated, if metadata-only return exists+not-hydrated")
    }

    fn create_placeholders(
        &self,
        _parent: &Path,
        _entries: &mut [PlaceholderEntry],
    ) -> PlatformResult<()> {
        todo!("FUSE 2: populate directory listing in FUSE inode table / metadata cache, no actual files on disk")
    }

    fn update_placeholder(
        &self,
        _path: &Path,
        _meta: &FileMetadataPlatform,
        _etag: &str,
        _options: UpdateOptions,
    ) -> PlatformResult<()> {
        todo!("FUSE 2: update metadata in cache DB, if dehydrate requested then evict cached data")
    }

    fn convert_to_placeholder(
        &self,
        _path: &Path,
        _etag: &str,
        _is_directory: bool,
    ) -> PlatformResult<()> {
        todo!("FUSE 2: register existing file in cache DB with given etag, mark as tracked")
    }

    fn create_placeholder(&self, _parent: &Path, _entry: &PlaceholderEntry) -> PlatformResult<()> {
        todo!("FUSE 2: add inode entry in FUSE metadata, no data on disk until accessed")
    }

    fn delete_placeholder(&self, _path: &Path) -> PlatformResult<()> {
        todo!("FUSE 2: remove inode from FUSE, evict from cache, remove metadata entry")
    }

    fn notify_change(&self, _path: &Path) -> PlatformResult<()> {
        // No-op on Linux — FUSE handles change visibility automatically
        Ok(())
    }

    fn set_error_state(&self, _path: &Path, _has_error: bool) -> PlatformResult<()> {
        todo!("FUSE 2: set user.cloudreve.sync_error xattr on cached file, or store in metadata DB")
    }

    fn is_supported() -> PlatformResult<bool>
    where
        Self: Sized,
    {
        // TODO: Check if FUSE is available (e.g., /dev/fuse exists and fusermount is in PATH)
        Err(PlatformError::NotSupported)
    }

    fn provider_id(&self) -> Option<&str> {
        todo!("FUSE 2: return a unique provider identifier stored in the cache metadata")
    }
}
