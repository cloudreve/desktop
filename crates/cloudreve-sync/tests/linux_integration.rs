#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cloudreve_api::models::explorer::{FileResponse, file_type};
use cloudreve_api::models::uri::CrUri;
use cloudreve_sync::drive::placeholder::CrPlaceholder;
use cloudreve_sync::drive::utils::{
    SHCNE_ATTRIBUTES, local_path_to_cr_uri, notify_shell_change, remote_path_to_local_relative_path,
};
use cloudreve_sync::inventory::InventoryDb;
use tempfile::TempDir;
use uuid::Uuid;

fn make_file_response(file_type_value: i32, size: i64, etag: &str) -> FileResponse {
    let mut metadata = HashMap::new();
    metadata.insert("k".to_string(), "v".to_string());

    FileResponse {
        file_type: file_type_value,
        id: "id-1".to_string(),
        name: "name".to_string(),
        permission: Some("rw".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        size,
        metadata: Some(metadata),
        path: "/remote/name".to_string(),
        shared: Some(false),
        capability: None,
        owned: None,
        folder_summary: None,
        extended_info: None,
        primary_entity: Some(etag.to_string()),
    }
}

#[test]
fn linux_placeholder_file_commit_and_delete_roundtrip() -> Result<()> {
    let temp = TempDir::new()?;
    let sync_root = temp.path().join("sync");
    std::fs::create_dir_all(&sync_root)?;

    let db_path = temp.path().join("inventory.db");
    let inventory = Arc::new(InventoryDb::with_path(db_path)?);
    let drive_id = Uuid::new_v4();
    let local_file = sync_root.join("docs").join("report.txt");

    let mut placeholder = CrPlaceholder::new(local_file.clone(), sync_root.clone(), drive_id)
        .with_remote_file(&make_file_response(file_type::FILE, 123, "etag-1"));
    placeholder.commit(inventory.clone())?;

    assert!(local_file.exists());
    let saved = inventory
        .query_by_path(local_file.to_string_lossy().as_ref())?
        .expect("metadata should exist after commit");
    assert!(!saved.is_folder);
    assert_eq!(saved.size, 123);
    assert_eq!(saved.etag, "etag-1");

    placeholder.delete_placeholder(inventory.clone())?;
    assert!(!local_file.exists());
    assert!(
        inventory
            .query_by_path(local_file.to_string_lossy().as_ref())?
            .is_none()
    );

    Ok(())
}

#[test]
fn linux_placeholder_directory_commit_creates_directory_and_inventory() -> Result<()> {
    let temp = TempDir::new()?;
    let sync_root = temp.path().join("sync");
    std::fs::create_dir_all(&sync_root)?;

    let db_path = temp.path().join("inventory.db");
    let inventory = Arc::new(InventoryDb::with_path(db_path)?);
    let drive_id = Uuid::new_v4();
    let local_dir = sync_root.join("photos");

    let mut placeholder = CrPlaceholder::new(local_dir.clone(), sync_root, drive_id)
        .with_remote_file(&make_file_response(file_type::FOLDER, 0, "etag-dir"));
    placeholder.commit(inventory.clone())?;

    assert!(local_dir.is_dir());
    let saved = inventory
        .query_by_path(local_dir.to_string_lossy().as_ref())?
        .expect("directory metadata should exist after commit");
    assert!(saved.is_folder);

    Ok(())
}

#[test]
fn linux_notify_shell_change_is_noop_for_missing_path() -> Result<()> {
    let missing = PathBuf::from("/tmp/cloudreve-sync-test-missing-path");
    notify_shell_change(&missing, SHCNE_ATTRIBUTES)?;
    Ok(())
}

#[test]
fn linux_path_mapping_roundtrip() -> Result<()> {
    let root = PathBuf::from("/sync");
    let local = PathBuf::from("/sync/folder/file.txt");
    let uri = local_path_to_cr_uri(local, root, "cloudreve://base".to_string())?;
    assert_eq!(uri.path(), "/folder/file.txt");

    let remote = CrUri::new("cloudreve://base/folder/file.txt")?;
    let base = CrUri::new("cloudreve://base")?;
    let relative = remote_path_to_local_relative_path(&remote, &base)?;
    assert_eq!(relative, PathBuf::from("folder/file.txt"));

    Ok(())
}
