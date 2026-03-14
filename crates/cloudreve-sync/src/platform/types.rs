use std::ffi::OsString;

/// Platform-agnostic file metadata, replacing direct usage of cfapi's `Metadata`.
#[derive(Debug, Clone)]
pub struct FileMetadataPlatform {
    pub is_directory: bool,
    pub size: u64,
    pub created_at: i64, // unix timestamp seconds
    pub modified_at: i64, // unix timestamp seconds
}

/// Platform-agnostic local file state, replacing cfapi's `LocalFileInfo`.
#[derive(Debug, Clone)]
pub struct LocalFileState {
    pub exists: bool,
    pub is_directory: bool,
    pub is_placeholder: bool,
    /// Whether the file content is fully available locally.
    pub is_hydrated: bool,
    /// Whether a placeholder directory has been populated with children.
    pub is_folder_populated: bool,
    /// Whether the file is in sync with the cloud.
    pub in_sync: bool,
    /// Whether the file is pinned for offline availability.
    pub is_pinned: bool,
    /// Whether the file is unpinned (explicitly marked for eviction).
    pub is_unpinned: bool,
    pub size: Option<u64>,
}

impl LocalFileState {
    /// Create a state representing a missing/non-existent file.
    pub fn missing() -> Self {
        Self {
            exists: false,
            is_directory: false,
            is_placeholder: false,
            is_hydrated: false,
            is_folder_populated: false,
            in_sync: false,
            is_pinned: false,
            is_unpinned: false,
            size: None,
        }
    }
}

/// A placeholder entry to be created in the filesystem.
/// Replaces direct usage of cfapi's `PlaceholderFile`.
#[derive(Debug, Clone)]
pub struct PlaceholderEntry {
    /// File or directory name (not full path).
    pub relative_name: OsString,
    pub metadata: FileMetadataPlatform,
    pub etag: String,
    pub mark_in_sync: bool,
}

/// Trait for writing hydration data back to the platform.
/// Replaces direct usage of cfapi's `ticket::FetchData` and `WriteAt` trait.
pub trait HydrationWriter: Send + Sync {
    /// Write data at the given offset within the file being hydrated.
    fn write_at(&self, buf: &[u8], offset: u64) -> anyhow::Result<()>;
    /// Report download progress to the platform (e.g., Windows Explorer progress bar).
    fn report_progress(&self, total: u64, completed: u64) -> anyhow::Result<()>;
}
