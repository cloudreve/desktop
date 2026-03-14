use crate::inventory::InventoryDb;
use crate::inventory::schema::fuse_inodes;
use dashmap::DashMap;
use diesel::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Root inode number (FUSE convention).
pub const ROOT_INODE: u64 = 1;

/// Cache state for an inode entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InodeCacheState {
    NotCached,
    Cached,
    Dirty,
}

impl InodeCacheState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotCached => "not_cached",
            Self::Cached => "cached",
            Self::Dirty => "dirty",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "cached" => Self::Cached,
            "dirty" => Self::Dirty,
            _ => Self::NotCached,
        }
    }
}

/// In-memory representation of a FUSE inode.
#[derive(Debug, Clone)]
pub struct InodeEntry {
    pub inode: u64,
    pub parent_inode: u64,
    pub name: String,
    pub is_directory: bool,
    pub size: i64,
    pub etag: String,
    pub created_at: i64,
    pub modified_at: i64,
    pub accessed_at: i64,
    pub cache_state: InodeCacheState,
    pub pinned: bool,
    pub has_error: bool,
    pub populated: bool,
}

/// Diesel model for the fuse_inodes table.
#[derive(Queryable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = fuse_inodes)]
pub struct InodeRow {
    pub inode: i64,
    pub parent_inode: i64,
    pub name: String,
    pub is_directory: bool,
    pub size: i64,
    pub etag: String,
    pub created_at: i64,
    pub modified_at: i64,
    pub accessed_at: i64,
    pub cache_state: String,
    pub pinned: bool,
    pub has_error: bool,
    pub populated: bool,
    pub drive_id: String,
}

impl InodeRow {
    fn to_entry(&self) -> InodeEntry {
        InodeEntry {
            inode: self.inode as u64,
            parent_inode: self.parent_inode as u64,
            name: self.name.clone(),
            is_directory: self.is_directory,
            size: self.size,
            etag: self.etag.clone(),
            created_at: self.created_at,
            modified_at: self.modified_at,
            accessed_at: self.accessed_at,
            cache_state: InodeCacheState::from_str(&self.cache_state),
            pinned: self.pinned,
            has_error: self.has_error,
            populated: self.populated,
        }
    }
}

/// Inode database with SQLite persistence and DashMap hot cache.
pub struct InodeDb {
    db: Arc<InventoryDb>,
    /// inode -> InodeEntry hot cache
    entries: DashMap<u64, InodeEntry>,
    /// (parent_inode, name) -> child_inode lookup
    children: DashMap<(u64, String), u64>,
    /// Next inode number to allocate
    next_inode: AtomicU64,
    drive_id: String,
}

impl InodeDb {
    /// Create a new InodeDb, loading existing inodes from SQLite.
    pub fn new(db: Arc<InventoryDb>, drive_id: String) -> anyhow::Result<Self> {
        let inode_db = Self {
            db,
            entries: DashMap::new(),
            children: DashMap::new(),
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            drive_id,
        };
        inode_db.load_from_db()?;
        Ok(inode_db)
    }

    /// Load all inodes for this drive from SQLite into the hot cache.
    fn load_from_db(&self) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        let rows: Vec<InodeRow> = dsl::fuse_inodes
            .filter(dsl::drive_id.eq(&self.drive_id))
            .load::<InodeRow>(&mut *conn)?;

        let mut max_inode = ROOT_INODE;
        for row in rows {
            let entry = row.to_entry();
            let ino = entry.inode;
            if ino > max_inode {
                max_inode = ino;
            }
            self.children
                .insert((entry.parent_inode, entry.name.clone()), ino);
            self.entries.insert(ino, entry);
        }

        self.next_inode.store(max_inode + 1, Ordering::SeqCst);
        Ok(())
    }

    /// Allocate a new inode number.
    pub fn alloc_inode(&self) -> u64 {
        self.next_inode.fetch_add(1, Ordering::SeqCst)
    }

    /// Get an inode entry from the hot cache.
    pub fn get(&self, inode: u64) -> Option<InodeEntry> {
        self.entries.get(&inode).map(|e| e.clone())
    }

    /// Look up a child inode by parent + name.
    pub fn lookup_child(&self, parent: u64, name: &str) -> Option<InodeEntry> {
        let child_inode = self.children.get(&(parent, name.to_string()))?;
        self.get(*child_inode)
    }

    /// List all children of a directory inode.
    pub fn list_children(&self, parent: u64) -> Vec<InodeEntry> {
        let mut result = Vec::new();
        for entry in self.entries.iter() {
            if entry.parent_inode == parent && entry.inode != parent {
                result.push(entry.clone());
            }
        }
        result
    }

    /// Insert a new inode into both the hot cache and SQLite.
    pub fn insert(&self, entry: InodeEntry) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let row = InodeRow {
            inode: entry.inode as i64,
            parent_inode: entry.parent_inode as i64,
            name: entry.name.clone(),
            is_directory: entry.is_directory,
            size: entry.size,
            etag: entry.etag.clone(),
            created_at: entry.created_at,
            modified_at: entry.modified_at,
            accessed_at: entry.accessed_at,
            cache_state: entry.cache_state.as_str().to_string(),
            pinned: entry.pinned,
            has_error: entry.has_error,
            populated: entry.populated,
            drive_id: self.drive_id.clone(),
        };

        let mut conn = self.db.connection()?;
        diesel::insert_into(dsl::fuse_inodes)
            .values(&row)
            .on_conflict(dsl::inode)
            .do_update()
            .set(&row)
            .execute(&mut *conn)?;

        self.children
            .insert((entry.parent_inode, entry.name.clone()), entry.inode);
        self.entries.insert(entry.inode, entry);
        Ok(())
    }

    /// Bulk insert multiple inodes (for populating a directory).
    pub fn insert_batch(&self, entries: &[InodeEntry]) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let rows: Vec<InodeRow> = entries
            .iter()
            .map(|e| InodeRow {
                inode: e.inode as i64,
                parent_inode: e.parent_inode as i64,
                name: e.name.clone(),
                is_directory: e.is_directory,
                size: e.size,
                etag: e.etag.clone(),
                created_at: e.created_at,
                modified_at: e.modified_at,
                accessed_at: e.accessed_at,
                cache_state: e.cache_state.as_str().to_string(),
                pinned: e.pinned,
                has_error: e.has_error,
                populated: e.populated,
                drive_id: self.drive_id.clone(),
            })
            .collect();

        let mut conn = self.db.connection()?;
        for row in &rows {
            diesel::insert_into(dsl::fuse_inodes)
                .values(row)
                .on_conflict(dsl::inode)
                .do_update()
                .set(row)
                .execute(&mut *conn)?;
        }

        for entry in entries {
            self.children
                .insert((entry.parent_inode, entry.name.clone()), entry.inode);
            self.entries.insert(entry.inode, entry.clone());
        }

        Ok(())
    }

    /// Update an existing inode entry.
    pub fn update(&self, entry: &InodeEntry) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(entry.inode as i64)))
            .set((
                dsl::size.eq(entry.size),
                dsl::etag.eq(&entry.etag),
                dsl::modified_at.eq(entry.modified_at),
                dsl::accessed_at.eq(entry.accessed_at),
                dsl::cache_state.eq(entry.cache_state.as_str()),
                dsl::pinned.eq(entry.pinned),
                dsl::has_error.eq(entry.has_error),
                dsl::populated.eq(entry.populated),
            ))
            .execute(&mut *conn)?;

        self.entries.insert(entry.inode, entry.clone());
        Ok(())
    }

    /// Update just the cache state of an inode.
    pub fn set_cache_state(&self, inode: u64, state: InodeCacheState) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(inode as i64)))
            .set(dsl::cache_state.eq(state.as_str()))
            .execute(&mut *conn)?;

        if let Some(mut entry) = self.entries.get_mut(&inode) {
            entry.cache_state = state;
        }
        Ok(())
    }

    /// Set the error state of an inode.
    pub fn set_error(&self, inode: u64, has_error: bool) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(inode as i64)))
            .set(dsl::has_error.eq(has_error))
            .execute(&mut *conn)?;

        if let Some(mut entry) = self.entries.get_mut(&inode) {
            entry.has_error = has_error;
        }
        Ok(())
    }

    /// Mark a directory as populated.
    pub fn set_populated(&self, inode: u64, populated: bool) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(inode as i64)))
            .set(dsl::populated.eq(populated))
            .execute(&mut *conn)?;

        if let Some(mut entry) = self.entries.get_mut(&inode) {
            entry.populated = populated;
        }
        Ok(())
    }

    /// Update the accessed_at timestamp for an inode.
    pub fn touch_accessed(&self, inode: u64) -> anyhow::Result<()> {
        let now = now_secs();
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(inode as i64)))
            .set(dsl::accessed_at.eq(now))
            .execute(&mut *conn)?;

        if let Some(mut entry) = self.entries.get_mut(&inode) {
            entry.accessed_at = now;
        }
        Ok(())
    }

    /// Delete an inode and all its descendants from cache and DB.
    pub fn delete(&self, inode: u64) -> anyhow::Result<()> {
        // Collect descendants first
        let mut to_delete = vec![inode];
        let mut i = 0;
        while i < to_delete.len() {
            let parent = to_delete[i];
            for entry in self.entries.iter() {
                if entry.parent_inode == parent && entry.inode != parent {
                    to_delete.push(entry.inode);
                }
            }
            i += 1;
        }

        // Delete from DB
        use fuse_inodes::dsl;
        let mut conn = self.db.connection()?;
        let inode_ids: Vec<i64> = to_delete.iter().map(|&i| i as i64).collect();
        diesel::delete(dsl::fuse_inodes.filter(dsl::inode.eq_any(&inode_ids)))
            .execute(&mut *conn)?;

        // Remove from caches
        for &ino in &to_delete {
            if let Some((_, entry)) = self.entries.remove(&ino) {
                self.children.remove(&(entry.parent_inode, entry.name));
            }
        }

        Ok(())
    }

    /// Delete all inodes for this drive.
    pub fn delete_all(&self) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        let mut conn = self.db.connection()?;
        diesel::delete(dsl::fuse_inodes.filter(dsl::drive_id.eq(&self.drive_id)))
            .execute(&mut *conn)?;

        // Clear caches - remove only entries for this drive
        let inodes_to_remove: Vec<u64> = self.entries.iter().map(|e| e.inode).collect();
        for ino in inodes_to_remove {
            if let Some((_, entry)) = self.entries.remove(&ino) {
                self.children.remove(&(entry.parent_inode, entry.name));
            }
        }

        Ok(())
    }

    /// Resolve an inode to its full path from the mount root.
    pub fn resolve_path(&self, inode: u64) -> Option<PathBuf> {
        if inode == ROOT_INODE {
            return Some(PathBuf::from("/"));
        }

        let mut components = Vec::new();
        let mut current = inode;

        loop {
            let entry = self.get(current)?;
            if current == ROOT_INODE {
                break;
            }
            components.push(entry.name.clone());
            current = entry.parent_inode;
            if current == 0 {
                break;
            }
        }

        components.reverse();
        let mut path = PathBuf::from("/");
        for c in components {
            path.push(c);
        }
        Some(path)
    }

    /// Find an inode by its absolute path relative to the mount root.
    pub fn find_by_path(&self, path: &Path) -> Option<InodeEntry> {
        let mut current = ROOT_INODE;

        for component in path.components() {
            match component {
                std::path::Component::RootDir => continue,
                std::path::Component::Normal(name) => {
                    let name_str = name.to_str()?;
                    let child = self.lookup_child(current, name_str)?;
                    current = child.inode;
                }
                _ => return None,
            }
        }

        self.get(current)
    }

    /// List dirty inodes that haven't been modified in the given duration.
    pub fn list_dirty_older_than(&self, min_age_secs: i64) -> Vec<InodeEntry> {
        let cutoff = now_secs() - min_age_secs;
        self.entries
            .iter()
            .filter(|e| e.cache_state == InodeCacheState::Dirty && e.modified_at < cutoff)
            .map(|e| e.clone())
            .collect()
    }

    /// List cached (non-dirty, non-pinned) inodes sorted by accessed_at ascending for eviction.
    pub fn list_evictable(&self) -> Vec<InodeEntry> {
        let mut entries: Vec<InodeEntry> = self
            .entries
            .iter()
            .filter(|e| e.cache_state == InodeCacheState::Cached && !e.pinned && !e.is_directory)
            .map(|e| e.clone())
            .collect();
        entries.sort_by_key(|e| e.accessed_at);
        entries
    }

    /// Rename an inode (move from old parent/name to new parent/name).
    pub fn rename(&self, inode: u64, new_parent: u64, new_name: &str) -> anyhow::Result<()> {
        use fuse_inodes::dsl;

        if let Some(old_entry) = self.get(inode) {
            self.children
                .remove(&(old_entry.parent_inode, old_entry.name));
        }

        let mut conn = self.db.connection()?;
        diesel::update(dsl::fuse_inodes.filter(dsl::inode.eq(inode as i64)))
            .set((
                dsl::parent_inode.eq(new_parent as i64),
                dsl::name.eq(new_name),
            ))
            .execute(&mut *conn)?;

        if let Some(mut entry) = self.entries.get_mut(&inode) {
            entry.parent_inode = new_parent;
            entry.name = new_name.to_string();
        }
        self.children
            .insert((new_parent, new_name.to_string()), inode);

        Ok(())
    }

    pub fn drive_id(&self) -> &str {
        &self.drive_id
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
