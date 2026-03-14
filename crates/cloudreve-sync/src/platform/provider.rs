use std::path::{Path, PathBuf};

use super::error::PlatformResult;
use super::types::{FileMetadataPlatform, LocalFileState, PlaceholderEntry};

/// Configuration for initializing a sync provider.
pub struct ProviderConfig {
    pub sync_path: PathBuf,
    pub display_name: String,
    pub icon_path: Option<String>,
    /// Opaque provider ID string (replaces `SyncRootId` as a platform-agnostic identifier).
    pub provider_id: String,
    pub instance_url: String,
    pub user_id: String,
    pub remote_path: String,
    pub recycle_bin_uri: String,
}

/// Options for updating a placeholder.
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub dehydrate: bool,
    pub mark_no_children: bool,
}

/// Core trait abstracting platform-specific sync filesystem operations.
///
/// On Windows, this wraps the Cloud Filter API (cfapi).
/// On Linux, this will wrap FUSE 2 with a local cache layer.
pub trait SyncProvider: Send + Sync {
    /// Register and connect the sync root. Called when a drive is started.
    fn start(&mut self, config: &ProviderConfig) -> PlatformResult<()>;

    /// Disconnect from the sync root. Called during shutdown.
    fn stop(&mut self) -> PlatformResult<()>;

    /// Unregister the sync root and clean up. Called when a drive is deleted.
    fn unregister(&mut self) -> PlatformResult<()>;

    /// Query local file state at the given path.
    /// Replaces `LocalFileInfo::from_path()`.
    fn get_file_state(&self, path: &Path) -> LocalFileState;

    /// Create placeholders in a directory from a list of entries.
    /// Replaces `ticket::FetchPlaceholders::pass_with_placeholder()`.
    fn create_placeholders(
        &self,
        parent: &Path,
        entries: &mut [PlaceholderEntry],
    ) -> PlatformResult<()>;

    /// Update a placeholder's metadata and sync state.
    fn update_placeholder(
        &self,
        path: &Path,
        meta: &FileMetadataPlatform,
        etag: &str,
        options: UpdateOptions,
    ) -> PlatformResult<()>;

    /// Convert an existing (non-placeholder) file to a tracked placeholder.
    fn convert_to_placeholder(
        &self,
        path: &Path,
        etag: &str,
        is_directory: bool,
    ) -> PlatformResult<()>;

    /// Create a new placeholder file or directory.
    fn create_placeholder(&self, parent: &Path, entry: &PlaceholderEntry) -> PlatformResult<()>;

    /// Delete a placeholder and notify the platform.
    fn delete_placeholder(&self, path: &Path) -> PlatformResult<()>;

    /// Notify the desktop environment of a file change.
    /// (SHChangeNotify on Windows, no-op on most other platforms.)
    fn notify_change(&self, path: &Path) -> PlatformResult<()>;

    /// Set or clear sync error state on a file.
    /// (PKEY_LastSyncError on Windows, xattr on Linux.)
    fn set_error_state(&self, path: &Path, has_error: bool) -> PlatformResult<()>;

    /// Check whether the platform supports this sync provider.
    fn is_supported() -> PlatformResult<bool>
    where
        Self: Sized;

    /// Get the opaque provider ID string.
    /// On Windows this is the serialized SyncRootId; on Linux it could be an identifier stored in xattr.
    fn provider_id(&self) -> Option<&str>;
}
