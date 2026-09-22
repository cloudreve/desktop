use super::InventoryDb;
use crate::inventory::{
    ConflictState, FileMetadata, MetadataEntry,
};
use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::sql_types::Text;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::inventory::schema::file_metadata::{self, dsl as file_metadata_dsl};

impl InventoryDb {
    /// Insert file metadata entries in bulk, refreshing rows that already exist.
    ///
    /// The same local path is re-enumerated on every mount, remount and folder expansion, so a
    /// plain `INSERT` aborts the entire batch with `UNIQUE constraint failed:
    /// file_metadata.local_path` as soon as one of the paths is already known. The caller only
    /// logs that error while the placeholders have already been handed to the OS, which leaves
    /// the visible tree and the inventory disagreeing and makes later operations fail with
    /// `Path not exist: ent: file not found`.
    ///
    /// `created_at` and `conflict_state` are deliberately left untouched for existing rows:
    /// re-enumerating a path must not reset its age nor clear a conflict that still awaits the
    /// user's decision.
    pub fn batch_insert(&self, entries: &[MetadataEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Collapse repeated paths so a single batch never touches the same row twice.
        let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
        let mut upserts: Vec<(NewFileMetadata, FileMetadataChangeset)> =
            Vec::with_capacity(entries.len());
        for entry in entries {
            if !seen.insert(entry.local_path.as_str()) {
                continue;
            }
            let mut changeset = FileMetadataChangeset::from_entry(entry)?;
            // Leaving this as None keeps the column out of the SET clause: re-enumerating a
            // path must not clear a conflict that is still waiting for the user's decision.
            changeset.conflict_state = None;
            upserts.push((NewFileMetadata::try_from(entry)?, changeset));
        }

        // A single multi-row `INSERT ... ON CONFLICT` is not expressible in Diesel's SQLite
        // backend, so the rows are upserted one by one inside one transaction (which is still
        // a single fsync per batch).
        let mut conn = self.connection()?;
        (&mut *conn)
            .transaction::<(), diesel::result::Error, _>(|tx_conn| {
                for (row, changeset) in &upserts {
                    diesel::insert_into(file_metadata::table)
                        .values(row)
                        .on_conflict(file_metadata::local_path)
                        .do_update()
                        .set(changeset)
                        .execute(tx_conn)?;
                }
                Ok(())
            })
            .context("Failed to batch upsert inventory metadata")?;
        Ok(())
    }

    pub fn nuke_drive(&self, drive: &str) -> Result<()> {
        let mut conn = self.connection()?;
        diesel::delete(
            file_metadata_dsl::file_metadata.filter(file_metadata_dsl::drive_id.eq(drive)),
        )
        .execute(&mut conn)
        .context("Failed to delete inventory rows for drive")?;
        Ok(())
    }

    /// Insert a new file metadata entry
    pub fn insert(&self, entry: &MetadataEntry) -> Result<usize> {
        let mut conn = self.connection()?;
        let new_entry = NewFileMetadata::try_from(entry)?;
        diesel::insert_into(file_metadata::table)
            .values(&new_entry)
            .execute(&mut conn)
            .context("Failed to insert inventory metadata")
    }

    /// Update an existing file metadata entry by local path
    pub fn update(&self, entry: &MetadataEntry) -> Result<bool> {
        let mut conn = self.connection()?;
        let changeset = FileMetadataChangeset::from_entry(entry)?;
        let rows_affected = diesel::update(
            file_metadata_dsl::file_metadata
                .filter(file_metadata_dsl::local_path.eq(&entry.local_path)),
        )
        .set(changeset)
        .execute(&mut conn)
        .context("Failed to update inventory metadata")?;
        Ok(rows_affected > 0)
    }

    /// Insert or update a file metadata entry (upsert based on local_path)
    pub fn upsert(&self, entry: &MetadataEntry) -> Result<usize> {
        let mut conn = self.connection()?;
        let insert_data = NewFileMetadata::try_from(entry)?;
        let update_data = FileMetadataChangeset::from_entry(entry)?;

        diesel::insert_into(file_metadata::table)
            .values(&insert_data)
            .on_conflict(file_metadata::local_path)
            .do_update()
            .set(update_data)
            .execute(&mut conn)
            .context("Failed to upsert inventory metadata")
    }

    /// Query file metadata by local path
    pub fn query_by_path(&self, path: &str) -> Result<Option<FileMetadata>> {
        let mut conn = self.connection()?;
        let row = file_metadata_dsl::file_metadata
            .filter(file_metadata_dsl::local_path.eq(path))
            .first::<FileMetadataRow>(&mut conn)
            .optional()
            .context("Failed to query inventory metadata by path")?;

        row.map(FileMetadata::try_from).transpose()
    }

    /// Query file metadata by id
    pub fn query_by_id(&self, id: i64) -> Result<Option<FileMetadata>> {
        let mut conn = self.connection()?;
        let row = file_metadata_dsl::file_metadata
            .filter(file_metadata_dsl::id.eq(id))
            .first::<FileMetadataRow>(&mut conn)
            .optional()
            .context("Failed to query inventory metadata by id")?;

        row.map(FileMetadata::try_from).transpose()
    }

    /// Batch delete file metadata by local path
    pub fn batch_delete_by_path(&self, paths: Vec<&str>) -> Result<bool> {
        if paths.is_empty() {
            return Ok(false);
        }

        let affected = {
            let mut conn = self.connection()?;
            (&mut *conn)
                .transaction::<i64, diesel::result::Error, _>(|tx_conn| {
                    let mut total: i64 = 0;
                    for path in &paths {
                        total += diesel::delete(
                            file_metadata_dsl::file_metadata
                                .filter(file_metadata_dsl::local_path.eq(path)),
                        )
                        .execute(tx_conn)? as i64;

                        let prefix = format!("{}/%", path);
                        total += diesel::delete(
                            file_metadata_dsl::file_metadata
                                .filter(file_metadata_dsl::local_path.like(&prefix)),
                        )
                        .execute(tx_conn)? as i64;
                    }
                    Ok(total)
                })
                .context("Failed to batch delete inventory metadata")?
        }; // conn is dropped here, releasing it back to the pool

        // Delete upload sessions - now safe to acquire a new connection
        self.batch_delete_upload_session_by_path(&paths)?;
        Ok(affected > 0)
    }

    /// Get total count of entries in the database
    pub fn count(&self) -> Result<i64> {
        let mut conn = self.connection()?;
        file_metadata_dsl::file_metadata
            .count()
            .get_result(&mut conn)
            .context("Failed to count inventory metadata")
    }

    /// Clear all entries from the database
    pub fn clear(&self) -> Result<()> {
        let mut conn = self.connection()?;
        diesel::delete(file_metadata::table)
            .execute(&mut conn)
            .context("Failed to clear inventory metadata")?;
        Ok(())
    }

    /// Rename or move a file/folder and update all its descendants.
    /// Uses two UPDATE queries: one for the exact path, one for descendants.
    /// Only replaces the prefix portion to avoid issues with duplicate path segments.
    ///
    /// Returns the number of rows updated.
    pub fn rename_path(&self, old_path: &str, new_path: &str) -> Result<usize> {
        if old_path == new_path {
            return Ok(0);
        }

        let mut conn = self.connection()?;
        let old_prefix = format!("{}{}", old_path, std::path::MAIN_SEPARATOR);
        let new_prefix = format!("{}{}", new_path, std::path::MAIN_SEPARATOR);
        let descendant_like = format!("{}%", old_prefix);

        let total = (&mut *conn)
            .transaction::<usize, diesel::result::Error, _>(|tx_conn| {
                let exact = diesel::update(
                    file_metadata_dsl::file_metadata
                        .filter(file_metadata_dsl::local_path.eq(old_path)),
                )
                .set((file_metadata_dsl::local_path.eq(new_path),))
                .execute(tx_conn)?;

                let descendants = diesel::sql_query(
                    "UPDATE file_metadata \
                     SET local_path = ? || substr(local_path, length(?) + 1) \
                     WHERE local_path LIKE ?",
                )
                .bind::<Text, _>(&new_prefix)
                .bind::<Text, _>(&old_prefix)
                .bind::<Text, _>(&descendant_like)
                .execute(tx_conn)?;

                Ok(exact + descendants)
            })
            .context("Failed to rename metadata path")?;

        Ok(total)
    }

    /// Mark a file as conflicted by setting its conflict_state.
    /// Pass `None` to clear the conflict state.
    ///
    /// Returns true if a row was updated.
    pub fn mark_as_conflicted(&self, path: &str, state: Option<ConflictState>) -> Result<bool> {
        let mut conn = self.connection()?;
        let state_str = state.map(|s| s.as_str().to_string());
        let rows_affected = diesel::update(
            file_metadata_dsl::file_metadata.filter(file_metadata_dsl::local_path.eq(path)),
        )
        .set(file_metadata_dsl::conflict_state.eq(state_str))
        .execute(&mut conn)
        .context("Failed to update conflict state")?;
        Ok(rows_affected > 0)
    }
}

// =========================================================================
// Row Types
// =========================================================================

#[derive(Queryable)]
struct FileMetadataRow {
    id: i64,
    drive_id: String,
    is_folder: bool,
    local_path: String,
    created_at: i64,
    updated_at: i64,
    etag: String,
    metadata: String,
    props: Option<String>,
    permissions: String,
    shared: bool,
    size: i64,
    conflict_state: Option<String>,
}

#[derive(Insertable)]
#[diesel(table_name = file_metadata)]
struct NewFileMetadata {
    drive_id: String,
    is_folder: bool,
    local_path: String,
    created_at: i64,
    updated_at: i64,
    etag: String,
    metadata: String,
    props: Option<String>,
    permissions: String,
    shared: bool,
    size: i64,
    conflict_state: Option<String>,
}

#[derive(AsChangeset)]
#[diesel(table_name = file_metadata)]
struct FileMetadataChangeset {
    drive_id: String,
    is_folder: bool,
    updated_at: i64,
    etag: String,
    metadata: String,
    props: Option<String>,
    permissions: String,
    shared: bool,
    size: i64,
    /// Use Option<Option<String>> so that:
    /// - Some(None) explicitly sets conflict_state to NULL
    /// - Some(Some(value)) sets it to a value
    conflict_state: Option<Option<String>>,
}

impl TryFrom<FileMetadataRow> for FileMetadata {
    type Error = anyhow::Error;

    fn try_from(row: FileMetadataRow) -> Result<Self> {
        let metadata_map: HashMap<String, String> =
            serde_json::from_str(&row.metadata).context("Failed to deserialize metadata column")?;
        let props_value = match row.props {
            Some(json) => {
                Some(serde_json::from_str(&json).context("Failed to deserialize props column")?)
            }
            None => None,
        };
        let conflict_state = row
            .conflict_state
            .as_deref()
            .and_then(ConflictState::from_str);

        Ok(FileMetadata {
            id: row.id,
            drive_id: Uuid::parse_str(&row.drive_id).context("Failed to parse drive_id column")?,
            is_folder: row.is_folder,
            local_path: row.local_path,
            created_at: row.created_at,
            updated_at: row.updated_at,
            etag: row.etag,
            metadata: metadata_map,
            props: props_value,
            permissions: row.permissions,
            shared: row.shared,
            size: row.size,
            conflict_state,
        })
    }
}

impl TryFrom<&MetadataEntry> for NewFileMetadata {
    type Error = anyhow::Error;

    fn try_from(entry: &MetadataEntry) -> Result<Self> {
        Ok(Self {
            drive_id: entry.drive_id.to_string(),
            is_folder: entry.is_folder,
            local_path: entry.local_path.clone(),
            created_at: entry.created_at,
            updated_at: entry.updated_at,
            etag: entry.etag.clone(),
            metadata: serde_json::to_string(&entry.metadata)
                .context("Failed to serialize metadata map")?,
            props: entry
                .props
                .as_ref()
                .map(|p| serde_json::to_string(p))
                .transpose()
                .context("Failed to serialize props field")?,
            permissions: entry.permissions.clone(),
            shared: entry.shared,
            size: entry.size,
            conflict_state: entry.conflict_state.map(|s| s.as_str().to_string()),
        })
    }
}

impl FileMetadataChangeset {
    fn from_entry(entry: &MetadataEntry) -> Result<Self> {
        Ok(Self {
            drive_id: entry.drive_id.to_string(),
            is_folder: entry.is_folder,
            updated_at: entry.updated_at,
            etag: entry.etag.clone(),
            metadata: serde_json::to_string(&entry.metadata)
                .context("Failed to serialize metadata map")?,
            props: entry
                .props
                .as_ref()
                .map(|p| serde_json::to_string(p))
                .transpose()
                .context("Failed to serialize props field")?,
            permissions: entry.permissions.clone(),
            shared: entry.shared,
            size: entry.size,
            // Use Some(...) to always update the column, even when clearing to NULL
            conflict_state: Some(entry.conflict_state.map(|s| s.as_str().to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_entry(local_path: &str, etag: &str, size: i64) -> MetadataEntry {
        MetadataEntry::new(Uuid::new_v4(), local_path, false)
            .with_etag(etag)
            .with_size(size)
            .with_created_at(1_700_000_000)
            .with_updated_at(1_700_000_000)
    }

    fn test_db() -> (tempfile::TempDir, InventoryDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = InventoryDb::with_path(dir.path().join("meta.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn batch_insert_refreshes_existing_paths_instead_of_failing() {
        let (_dir, db) = test_db();
        let path = r"C:\drive\个人\软件\linux\wps";

        db.batch_insert(&[
            metadata_entry(path, "etag-1", 10),
            metadata_entry(r"C:\drive\个人", "etag-1", 0),
        ])
        .unwrap();

        // Re-enumerating a folder that is already known must refresh the entry instead of
        // aborting the batch, which is what used to happen on the second enumeration.
        db.batch_insert(&[metadata_entry(path, "etag-2", 42)])
            .unwrap();

        let row = db.query_by_path(path).unwrap().expect("row must exist");
        assert_eq!(row.etag, "etag-2");
        assert_eq!(row.size, 42);
        assert_eq!(row.created_at, 1_700_000_000, "created_at is preserved");
        assert_eq!(db.count().unwrap(), 2, "no duplicate rows");
    }

    #[test]
    fn batch_insert_keeps_pending_conflict_state() {
        let (_dir, db) = test_db();
        let path = r"C:\drive\doc.txt";

        db.batch_insert(&[metadata_entry(path, "etag-1", 10)])
            .unwrap();
        db.mark_as_conflicted(path, Some(ConflictState::Pending))
            .unwrap();

        db.batch_insert(&[metadata_entry(path, "etag-2", 11)])
            .unwrap();

        let row = db.query_by_path(path).unwrap().unwrap();
        assert_eq!(row.etag, "etag-2");
        assert_eq!(row.conflict_state, Some(ConflictState::Pending));
    }
}
