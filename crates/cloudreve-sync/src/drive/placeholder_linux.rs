//! Linux implementation of drive placeholder operations.
//!
//! Unlike the Windows implementation (`placeholder.rs`), Linux does not use CFAPI
//! virtual files. This module implements a full-sync fallback with regular files
//! and directories.

use crate::inventory::{FileMetadata, InventoryDb, MetadataEntry};
use anyhow::{Context, Result};
use chrono::DateTime;
use cloudreve_api::models::explorer::{FileResponse, file_type};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use uuid::Uuid;

/// Local file snapshot used by Linux full-sync logic.
///
/// Difference from Windows:
/// - Windows reads CFAPI placeholder state and pin state.
/// - Linux only inspects normal filesystem metadata.
#[derive(Debug, Clone)]
pub struct LocalFileInfo {
    pub exists: bool,
    pub is_directory: bool,
    pub file_size: Option<u64>,
    pub last_modified: Option<SystemTime>,
}

impl LocalFileInfo {
    /// Build a local file snapshot from a filesystem path.
    ///
    /// Difference from Windows:
    /// - Windows uses `FindFirstFileExW` + Cloud Filter state.
    /// - Linux uses plain `std::fs::metadata`.
    pub fn from_path(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(Self {
                exists: true,
                is_directory: metadata.is_dir(),
                file_size: metadata.is_file().then_some(metadata.len()),
                last_modified: metadata.modified().ok(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::missing()),
            Err(e) => Err(e.into()),
        }
    }

    /// Build a "missing file" snapshot.
    ///
    /// Difference from Windows:
    /// - Semantics are the same; no CFAPI state is attached on Linux.
    pub fn missing() -> Self {
        Self {
            exists: false,
            is_directory: false,
            file_size: None,
            last_modified: None,
        }
    }

    /// Whether the local entry is considered in-sync.
    ///
    /// Difference from Windows:
    /// - Windows reads `CF_PLACEHOLDER_STATE_IN_SYNC`.
    /// - Linux has no equivalent kernel state, so this always returns `false`
    ///   to force explicit sync decisions in upper layers.
    pub fn in_sync(&self) -> bool {
        false
    }

    /// Whether the local entry is a virtual placeholder file.
    ///
    /// Difference from Windows:
    /// - Windows can return true for CFAPI placeholders.
    /// - Linux full-sync mode has no placeholder concept, so always `false`.
    pub fn is_placeholder(&self) -> bool {
        false
    }

    /// Whether content is only partially on disk.
    ///
    /// Difference from Windows:
    /// - Windows can represent partially hydrated placeholders.
    /// - Linux full-sync mode stores normal files only, so always `false`.
    pub fn partial_on_disk(&self) -> bool {
        false
    }

    /// Whether this entry is a directory.
    ///
    /// Difference from Windows:
    /// - Same meaning; Linux value comes from normal metadata.
    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    /// Whether a directory is populated locally.
    ///
    /// Difference from Windows:
    /// - Windows checks placeholder hydration state.
    /// - Linux considers an existing directory as populated.
    pub fn is_folder_populated(&self) -> bool {
        self.exists && self.is_directory
    }
}

/// Linux full-sync placeholder adapter.
///
/// Difference from Windows:
/// - Windows implementation drives CFAPI placeholder create/convert/update flows.
/// - Linux implementation performs regular file/directory operations and inventory updates.
pub struct CrPlaceholder {
    pub local_file_info: LocalFileInfo,

    local_path: PathBuf,
    #[allow(dead_code)]
    sync_root: PathBuf,
    drive_id: Uuid,
    file_meta: Option<FileMetadata>,
}

impl CrPlaceholder {
    /// Create a placeholder adapter for a local path.
    ///
    /// Difference from Windows:
    /// - Windows captures CFAPI placeholder metadata.
    /// - Linux captures plain filesystem metadata only.
    pub fn new(local_path: impl Into<PathBuf>, sync_root: PathBuf, drive_id: Uuid) -> Self {
        let local_path = local_path.into();
        let local_file_info =
            LocalFileInfo::from_path(&local_path).unwrap_or_else(|_| LocalFileInfo::missing());
        Self {
            local_path,
            sync_root,
            drive_id,
            file_meta: None,
            local_file_info,
        }
    }

    /// Keep API parity with Windows: no-op on Linux.
    ///
    /// Difference from Windows:
    /// - Windows may dehydrate ranges after updates.
    /// - Linux full-sync mode has no dehydrated placeholder ranges.
    pub fn with_invalidate_all_range(self, _enable: bool) -> Self {
        self
    }

    /// Keep API parity with Windows: no-op on Linux.
    ///
    /// Difference from Windows:
    /// - Windows may mark placeholder directories as having no children.
    /// - Linux full-sync mode does not use placeholder population flags.
    pub fn with_mark_no_children(self, _enable: bool) -> Self {
        self
    }

    /// Attach explicit metadata to be committed.
    ///
    /// Difference from Windows:
    /// - Same purpose as Windows; later commit behavior differs.
    pub fn with_file_meta(mut self, file_meta: FileMetadata) -> Self {
        self.file_meta = Some(file_meta);
        self
    }

    /// Delete local entry and remove corresponding inventory record.
    ///
    /// Difference from Windows:
    /// - Windows also notifies shell change + handles placeholder semantics.
    /// - Linux performs plain filesystem deletion and inventory cleanup only.
    pub fn delete_placeholder(&self, inventory: Arc<InventoryDb>) -> Result<()> {
        if self.local_file_info.exists {
            if self.local_path.is_dir() {
                fs::remove_dir_all(&self.local_path).context("failed to delete local directory")?;
            } else {
                fs::remove_file(&self.local_path).context("failed to delete local file")?;
            }
        }

        let path_str = self
            .local_path
            .to_str()
            .context("failed to convert path to string")?;
        inventory
            .batch_delete_by_path(vec![path_str])
            .context("failed to delete from inventory")?;

        Ok(())
    }

    /// Commit remote metadata to local filesystem + inventory.
    ///
    /// Difference from Windows:
    /// - Windows converts/updates CFAPI placeholders with in-sync metadata.
    /// - Linux ensures a normal local file/dir exists and upserts inventory;
    ///   no virtual-file state is created.
    pub fn commit(&mut self, inventory: Arc<InventoryDb>) -> Result<()> {
        let file_meta = self
            .file_meta
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("File metadata is not set"))?;

        if !self.local_file_info.exists {
            if file_meta.is_folder {
                fs::create_dir_all(&self.local_path).context("failed to create local directory")?;
            } else {
                if let Some(parent) = self.local_path.parent() {
                    fs::create_dir_all(parent).context("failed to create parent directory")?;
                }
                let _ = fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(&self.local_path)
                    .context("failed to create local file")?;
            }
        }

        inventory
            .upsert(&MetadataEntry::from(file_meta))
            .context("failed to upsert inventory")?;

        self.local_file_info =
            LocalFileInfo::from_path(&self.local_path).unwrap_or_else(|_| LocalFileInfo::missing());
        Ok(())
    }

    /// Convert remote `FileResponse` into internal metadata payload.
    ///
    /// Difference from Windows:
    /// - Same metadata mapping logic; downstream commit path differs by platform.
    pub fn with_remote_file(mut self, file_info: &FileResponse) -> Self {
        let created_at = DateTime::parse_from_rfc3339(&file_info.created_at)
            .ok()
            .map(|dt| dt.timestamp())
            .unwrap_or_default();

        let updated_at = DateTime::parse_from_rfc3339(&file_info.updated_at)
            .ok()
            .map(|dt| dt.timestamp())
            .unwrap_or_default();

        self.file_meta = Some(FileMetadata {
            drive_id: self.drive_id,
            local_path: self.local_path.to_string_lossy().to_string(),
            is_folder: file_info.file_type == file_type::FOLDER,
            created_at,
            updated_at,
            size: file_info.size,
            etag: file_info.primary_entity.clone().unwrap_or_default(),
            id: 0,
            metadata: file_info.metadata.clone().unwrap_or_default(),
            props: None,
            permissions: file_info.permission.clone().unwrap_or_default(),
            shared: file_info.shared.unwrap_or(false),
            conflict_state: None,
        });
        self
    }

    /// Update sync error state for local shell UI.
    ///
    /// Difference from Windows:
    /// - Windows writes `PKEY_LastSyncError` into Explorer property store.
    /// - Linux has no unified equivalent, so this is a no-op.
    pub fn update_sync_error_state(&self, _set_error: bool) -> Result<()> {
        Ok(())
    }
}
