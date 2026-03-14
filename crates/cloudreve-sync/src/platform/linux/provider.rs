use super::cache_impl::CacheManagerImpl;
use super::filesystem::CloudreveFS;
use super::handle::HandleTable;
use super::inode::{InodeCacheState, InodeDb, InodeEntry, ROOT_INODE, now_secs};
use super::workers;
use crate::drive::commands::MountCommand;
use crate::inventory::InventoryDb;
use crate::platform::error::{PlatformError, PlatformResult};
use crate::platform::provider::{ProviderConfig, SyncProvider, UpdateOptions};
use crate::platform::types::{FileMetadataPlatform, LocalFileState, PlaceholderEntry};
use fuser::MountOption;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Default max cache size: 10 GB
const DEFAULT_MAX_CACHE_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Global InodeDb reference for use by the sync compat layer.
/// Set when LinuxFuseProvider::start() is called.
static GLOBAL_INODE_DB: OnceLock<Arc<InodeDb>> = OnceLock::new();

/// Get the global InodeDb if one is active.
pub fn global_inode_db() -> Option<&'static Arc<InodeDb>> {
    GLOBAL_INODE_DB.get()
}

pub struct LinuxFuseProvider {
    inode_db: Option<Arc<InodeDb>>,
    cache: Option<Arc<CacheManagerImpl>>,
    session: Option<fuser::BackgroundSession>,
    upload_worker: Option<JoinHandle<()>>,
    eviction_worker: Option<JoinHandle<()>>,
    provider_id: Option<String>,
    sync_path: Option<PathBuf>,
    /// Sender to dispatch commands to the Mount's command processor.
    command_tx: Option<mpsc::UnboundedSender<MountCommand>>,
    inventory: Option<Arc<InventoryDb>>,
}

impl LinuxFuseProvider {
    pub fn new() -> Self {
        Self {
            inode_db: None,
            cache: None,
            session: None,
            upload_worker: None,
            eviction_worker: None,
            provider_id: None,
            sync_path: None,
            command_tx: None,
            inventory: None,
        }
    }

    /// Set the command sender for dispatching MountCommands.
    /// Must be called before start().
    pub fn set_command_tx(&mut self, tx: mpsc::UnboundedSender<MountCommand>) {
        self.command_tx = Some(tx);
    }

    /// Set the inventory database.
    /// Must be called before start().
    pub fn set_inventory(&mut self, inventory: Arc<InventoryDb>) {
        self.inventory = Some(inventory);
    }

    fn cache_dir(drive_id: &str) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cloudreve")
            .join("cache")
            .join(drive_id)
    }
}

impl SyncProvider for LinuxFuseProvider {
    fn start(&mut self, config: &ProviderConfig) -> PlatformResult<()> {
        let drive_id = config.provider_id.clone();
        let sync_path = config.sync_path.clone();

        // Get or create inventory
        let inventory = self
            .inventory
            .clone()
            .ok_or_else(|| PlatformError::Failed("inventory not set".to_string()))?;

        let command_tx = self
            .command_tx
            .clone()
            .ok_or_else(|| PlatformError::Failed("command_tx not set".to_string()))?;

        // Initialize InodeDb
        let inode_db = Arc::new(
            InodeDb::new(inventory.clone(), drive_id.clone())
                .map_err(|e| PlatformError::Failed(format!("failed to init InodeDb: {}", e)))?,
        );

        // Ensure root inode exists
        if inode_db.get(ROOT_INODE).is_none() {
            let now = now_secs();
            inode_db
                .insert(InodeEntry {
                    inode: ROOT_INODE,
                    parent_inode: ROOT_INODE,
                    name: String::new(),
                    is_directory: true,
                    size: 0,
                    etag: String::new(),
                    created_at: now,
                    modified_at: now,
                    accessed_at: now,
                    cache_state: InodeCacheState::NotCached,
                    pinned: false,
                    has_error: false,
                    populated: false,
                })
                .map_err(|e| {
                    PlatformError::Failed(format!("failed to insert root inode: {}", e))
                })?;
        }

        // Initialize cache manager
        let cache_dir = Self::cache_dir(&drive_id);
        let cache = Arc::new(
            CacheManagerImpl::new(cache_dir, inode_db.clone(), DEFAULT_MAX_CACHE_SIZE)
                .map_err(|e| PlatformError::Failed(format!("failed to init cache: {}", e)))?,
        );

        // Create mount directory if it doesn't exist
        std::fs::create_dir_all(&sync_path)
            .map_err(|e| PlatformError::Failed(format!("failed to create mount dir: {}", e)))?;

        // Set up global InodeDb for sync compat layer
        let _ = GLOBAL_INODE_DB.set(inode_db.clone());

        // Build FUSE filesystem
        let fs = CloudreveFS {
            inode_db: inode_db.clone(),
            cache: cache.clone(),
            handles: Arc::new(HandleTable::new()),
            command_tx: command_tx.clone(),
            drive_id: drive_id.clone(),
            tokio_rt: tokio::runtime::Handle::current(),
            sync_path: sync_path.clone(),
        };

        // FUSE mount options
        let options = vec![
            MountOption::FSName("cloudreve".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowOther,
            MountOption::DefaultPermissions,
        ];

        // Mount the FUSE filesystem
        let session = fuser::spawn_mount2(fs, &sync_path, &options)
            .map_err(|e| PlatformError::Failed(format!("failed to mount FUSE: {}", e)))?;

        tracing::info!(
            target: "fuse::provider",
            drive_id = %drive_id,
            sync_path = %sync_path.display(),
            "FUSE filesystem mounted"
        );

        // Spawn background workers
        let upload_handle =
            workers::spawn_upload_worker(inode_db.clone(), command_tx.clone(), sync_path.clone());
        let eviction_handle = workers::spawn_eviction_worker(inode_db.clone(), cache.clone());

        self.inode_db = Some(inode_db);
        self.cache = Some(cache);
        self.session = Some(session);
        self.upload_worker = Some(upload_handle);
        self.eviction_worker = Some(eviction_handle);
        self.provider_id = Some(drive_id);
        self.sync_path = Some(sync_path);
        self.inventory = Some(inventory);

        Ok(())
    }

    fn stop(&mut self) -> PlatformResult<()> {
        // Stop background workers
        if let Some(handle) = self.upload_worker.take() {
            handle.abort();
        }
        if let Some(handle) = self.eviction_worker.take() {
            handle.abort();
        }

        // Drop the FUSE session (triggers unmount)
        if let Some(session) = self.session.take() {
            drop(session);
            tracing::info!(target: "fuse::provider", "FUSE filesystem unmounted");
        }

        Ok(())
    }

    fn unregister(&mut self) -> PlatformResult<()> {
        self.stop()?;

        // Delete cache directory
        if let Some(ref provider_id) = self.provider_id {
            let cache_dir = Self::cache_dir(provider_id);
            if cache_dir.exists() {
                let _ = std::fs::remove_dir_all(&cache_dir);
            }
        }

        // Delete all inodes for this drive
        if let Some(ref inode_db) = self.inode_db {
            let _ = inode_db.delete_all();
        }

        Ok(())
    }

    fn get_file_state(&self, path: &Path) -> LocalFileState {
        let inode_db = match &self.inode_db {
            Some(db) => db,
            None => return LocalFileState::missing(),
        };

        let sync_path = match &self.sync_path {
            Some(p) => p,
            None => return LocalFileState::missing(),
        };

        // Convert absolute path to FUSE-relative path
        let relative = match path.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => return LocalFileState::missing(),
        };

        match inode_db.find_by_path(&relative) {
            Some(entry) => LocalFileState {
                exists: true,
                is_directory: entry.is_directory,
                is_placeholder: true,
                is_hydrated: entry.cache_state != InodeCacheState::NotCached,
                is_folder_populated: entry.populated,
                in_sync: entry.cache_state == InodeCacheState::Cached,
                is_pinned: entry.pinned,
                is_unpinned: false,
                size: Some(entry.size as u64),
            },
            None => LocalFileState::missing(),
        }
    }

    fn create_placeholders(
        &self,
        parent: &Path,
        entries: &mut [PlaceholderEntry],
    ) -> PlatformResult<()> {
        let inode_db = self
            .inode_db
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let sync_path = self
            .sync_path
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        // Convert parent to FUSE-relative path
        let relative_parent = match parent.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => PathBuf::from("/"),
        };

        let parent_entry = inode_db
            .find_by_path(&relative_parent)
            .ok_or(PlatformError::Failed("parent inode not found".to_string()))?;

        let mut new_entries = Vec::with_capacity(entries.len());
        let now = now_secs();

        for entry in entries.iter() {
            let name = entry.relative_name.to_str().unwrap_or_default().to_string();

            // Skip if already exists
            if inode_db.lookup_child(parent_entry.inode, &name).is_some() {
                continue;
            }

            let ino = inode_db.alloc_inode();
            new_entries.push(InodeEntry {
                inode: ino,
                parent_inode: parent_entry.inode,
                name,
                is_directory: entry.metadata.is_directory,
                size: entry.metadata.size as i64,
                etag: entry.etag.clone(),
                created_at: entry.metadata.created_at,
                modified_at: entry.metadata.modified_at,
                accessed_at: now,
                cache_state: InodeCacheState::NotCached,
                pinned: false,
                has_error: false,
                populated: false,
            });
        }

        if !new_entries.is_empty() {
            inode_db
                .insert_batch(&new_entries)
                .map_err(|e| PlatformError::Failed(format!("failed to insert inodes: {}", e)))?;
        }

        // Mark parent as populated
        let _ = inode_db.set_populated(parent_entry.inode, true);

        Ok(())
    }

    fn update_placeholder(
        &self,
        path: &Path,
        meta: &FileMetadataPlatform,
        etag: &str,
        options: UpdateOptions,
    ) -> PlatformResult<()> {
        let inode_db = self
            .inode_db
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let sync_path = self
            .sync_path
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let relative = match path.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => {
                return Err(PlatformError::Failed(
                    "path not under sync root".to_string(),
                ));
            }
        };

        let mut entry = inode_db
            .find_by_path(&relative)
            .ok_or(PlatformError::Failed("inode not found".to_string()))?;

        entry.size = meta.size as i64;
        entry.modified_at = meta.modified_at;
        entry.etag = etag.to_string();

        if options.dehydrate {
            // Evict cache file
            if let Some(ref cache) = self.cache {
                let _ = cache.evict_inode(entry.inode);
            }
            entry.cache_state = InodeCacheState::NotCached;
        }

        if options.mark_no_children {
            entry.populated = true; // Mark as populated (leaf directory)
        }

        inode_db
            .update(&entry)
            .map_err(|e| PlatformError::Failed(format!("failed to update inode: {}", e)))
    }

    fn convert_to_placeholder(
        &self,
        path: &Path,
        etag: &str,
        is_directory: bool,
    ) -> PlatformResult<()> {
        let inode_db = self
            .inode_db
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let sync_path = self
            .sync_path
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let relative = match path.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => {
                return Err(PlatformError::Failed(
                    "path not under sync root".to_string(),
                ));
            }
        };

        match inode_db.find_by_path(&relative) {
            Some(mut entry) => {
                entry.etag = etag.to_string();
                inode_db
                    .update(&entry)
                    .map_err(|e| PlatformError::Failed(format!("failed to update inode: {}", e)))
            }
            None => {
                // Create a new inode for this path
                let parent_path = relative.parent().unwrap_or_else(|| Path::new("/"));
                let parent_entry = inode_db
                    .find_by_path(parent_path)
                    .ok_or(PlatformError::Failed("parent inode not found".to_string()))?;

                let name = relative
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();

                let now = now_secs();
                let ino = inode_db.alloc_inode();
                inode_db
                    .insert(InodeEntry {
                        inode: ino,
                        parent_inode: parent_entry.inode,
                        name,
                        is_directory,
                        size: 0,
                        etag: etag.to_string(),
                        created_at: now,
                        modified_at: now,
                        accessed_at: now,
                        cache_state: InodeCacheState::Cached,
                        pinned: false,
                        has_error: false,
                        populated: false,
                    })
                    .map_err(|e| PlatformError::Failed(format!("failed to insert inode: {}", e)))
            }
        }
    }

    fn create_placeholder(&self, parent: &Path, entry: &PlaceholderEntry) -> PlatformResult<()> {
        let mut entries = vec![entry.clone()];
        self.create_placeholders(parent, &mut entries)
    }

    fn delete_placeholder(&self, path: &Path) -> PlatformResult<()> {
        let inode_db = self
            .inode_db
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let sync_path = self
            .sync_path
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let relative = match path.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => {
                return Err(PlatformError::Failed(
                    "path not under sync root".to_string(),
                ));
            }
        };

        if let Some(entry) = inode_db.find_by_path(&relative) {
            // Evict cache files for this inode and descendants
            if let Some(ref cache) = self.cache {
                let _ = cache.evict_inode(entry.inode);
            }

            inode_db
                .delete(entry.inode)
                .map_err(|e| PlatformError::Failed(format!("failed to delete inode: {}", e)))?;
        }

        Ok(())
    }

    fn notify_change(&self, _path: &Path) -> PlatformResult<()> {
        // No-op on Linux — FUSE handles visibility automatically
        Ok(())
    }

    fn set_error_state(&self, path: &Path, has_error: bool) -> PlatformResult<()> {
        let inode_db = self
            .inode_db
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let sync_path = self
            .sync_path
            .as_ref()
            .ok_or(PlatformError::Failed("not started".to_string()))?;

        let relative = match path.strip_prefix(sync_path) {
            Ok(r) => PathBuf::from("/").join(r),
            Err(_) => {
                return Err(PlatformError::Failed(
                    "path not under sync root".to_string(),
                ));
            }
        };

        if let Some(entry) = inode_db.find_by_path(&relative) {
            inode_db
                .set_error(entry.inode, has_error)
                .map_err(|e| PlatformError::Failed(format!("failed to set error: {}", e)))?;
        }

        Ok(())
    }

    fn is_supported() -> PlatformResult<bool>
    where
        Self: Sized,
    {
        // Check if /dev/fuse exists
        if !Path::new("/dev/fuse").exists() {
            return Ok(false);
        }

        // Check if fusermount3 or fusermount is in PATH
        let has_fusermount = std::process::Command::new("which")
            .arg("fusermount3")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || std::process::Command::new("which")
                .arg("fusermount")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        Ok(has_fusermount)
    }

    fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }
}
