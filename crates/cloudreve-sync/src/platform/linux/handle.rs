use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// An open file descriptor tracked by the FUSE filesystem.
#[derive(Debug)]
pub struct OpenFile {
    pub inode: u64,
    pub flags: i32,
    /// Cache file handle, opened lazily on first read/write.
    pub cache_file: Option<std::fs::File>,
}

/// Table of open file handles, mapping fh (file handle) -> OpenFile.
pub struct HandleTable {
    handles: DashMap<u64, OpenFile>,
    next_handle: AtomicU64,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            handles: DashMap::new(),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Allocate a new file handle for the given inode.
    pub fn open(&self, inode: u64, flags: i32) -> u64 {
        let fh = self.next_handle.fetch_add(1, Ordering::SeqCst);
        self.handles.insert(
            fh,
            OpenFile {
                inode,
                flags,
                cache_file: None,
            },
        );
        fh
    }

    /// Get the inode associated with a file handle.
    pub fn get_inode(&self, fh: u64) -> Option<u64> {
        self.handles.get(&fh).map(|h| h.inode)
    }

    /// Check if an inode has any open handles.
    pub fn is_open(&self, inode: u64) -> bool {
        self.handles.iter().any(|h| h.inode == inode)
    }

    /// Release (close) a file handle.
    pub fn release(&self, fh: u64) -> Option<OpenFile> {
        self.handles.remove(&fh).map(|(_, h)| h)
    }

    /// Get the number of open handles.
    pub fn len(&self) -> usize {
        self.handles.len()
    }
}
