use super::cache_impl::CacheManagerImpl;
use super::handle::HandleTable;
use super::inode::{InodeCacheState, InodeDb, InodeEntry, ROOT_INODE, now_secs};
use crate::drive::commands::MountCommand;
use crate::drive::sync::SyncMode;
use crate::platform::types::HydrationWriter;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use libc::{EACCES, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSYS, ENOTDIR, ENOTEMPTY};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;

const TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 512;

/// FUSE hydration writer that writes downloaded data into the cache file.
struct FuseHydrationWriter {
    inode: u64,
    cache: Arc<CacheManagerImpl>,
}

impl HydrationWriter for FuseHydrationWriter {
    fn write_at(&self, buf: &[u8], offset: u64) -> anyhow::Result<()> {
        self.cache.write_to_cache(self.inode, offset, buf)
    }

    fn report_progress(&self, _total: u64, _completed: u64) -> anyhow::Result<()> {
        // No progress UI on Linux FUSE (the read blocks until data arrives)
        Ok(())
    }
}

/// The main FUSE filesystem implementation.
pub struct CloudreveFS {
    pub(crate) inode_db: Arc<InodeDb>,
    pub(crate) cache: Arc<CacheManagerImpl>,
    pub(crate) handles: Arc<HandleTable>,
    pub(crate) command_tx: mpsc::UnboundedSender<MountCommand>,
    pub(crate) drive_id: String,
    pub(crate) tokio_rt: tokio::runtime::Handle,
    pub(crate) sync_path: PathBuf,
}

fn inode_to_attr(entry: &InodeEntry) -> FileAttr {
    let kind = if entry.is_directory {
        FileType::Directory
    } else {
        FileType::RegularFile
    };

    let perm = if entry.is_directory { 0o755 } else { 0o644 };

    let nlink = if entry.is_directory { 2 } else { 1 };

    FileAttr {
        ino: entry.inode,
        size: entry.size as u64,
        blocks: (entry.size as u64 + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
        atime: UNIX_EPOCH + Duration::from_secs(entry.accessed_at as u64),
        mtime: UNIX_EPOCH + Duration::from_secs(entry.modified_at as u64),
        ctime: UNIX_EPOCH + Duration::from_secs(entry.modified_at as u64),
        crtime: UNIX_EPOCH + Duration::from_secs(entry.created_at as u64),
        kind,
        perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

impl CloudreveFS {
    /// Resolve an inode to the absolute local path on the host filesystem.
    fn resolve_local_path(&self, inode: u64) -> Option<PathBuf> {
        let relative = self.inode_db.resolve_path(inode)?;
        // relative starts with "/" — strip it and join with sync_path
        let stripped = relative.strip_prefix("/").unwrap_or(&relative);
        Some(self.sync_path.join(stripped))
    }

    /// Dispatch a sync command for the given paths.
    fn dispatch_sync(&self, paths: Vec<PathBuf>) {
        let _ = self.command_tx.send(MountCommand::Sync {
            local_paths: paths,
            mode: SyncMode::PathOnly,
        });
    }
}

impl Filesystem for CloudreveFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        match self.inode_db.lookup_child(parent, name_str) {
            Some(entry) => {
                let attr = inode_to_attr(&entry);
                reply.entry(&TTL, &attr, 0);
            }
            None => {
                // If the parent directory hasn't been populated yet, trigger a fetch
                if let Some(parent_entry) = self.inode_db.get(parent) {
                    if parent_entry.is_directory && !parent_entry.populated {
                        if let Some(local_path) = self.resolve_local_path(parent) {
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            let _ = self.command_tx.send(MountCommand::FetchPlaceholders {
                                path: local_path,
                                response: tx,
                            });
                            // Wait for the fetch to complete
                            match rx.blocking_recv() {
                                Ok(Ok(_)) => {
                                    // Retry lookup after population
                                    if let Some(entry) =
                                        self.inode_db.lookup_child(parent, name_str)
                                    {
                                        let attr = inode_to_attr(&entry);
                                        reply.entry(&TTL, &attr, 0);
                                        return;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.inode_db.get(ino) {
            Some(entry) => {
                let attr = inode_to_attr(&entry);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let mut entry = match self.inode_db.get(ino) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if let Some(new_size) = size {
            entry.size = new_size as i64;
            // Truncate cache file if it exists
            let cache_path = self.cache.cache_path_for_inode(ino);
            if cache_path.exists() {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&cache_path) {
                    let _ = f.set_len(new_size);
                }
            }
        }

        if let Some(t) = atime {
            entry.accessed_at = match t {
                TimeOrNow::SpecificTime(t) => {
                    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
                }
                TimeOrNow::Now => now_secs(),
            };
        }

        if let Some(t) = mtime {
            entry.modified_at = match t {
                TimeOrNow::SpecificTime(t) => {
                    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
                }
                TimeOrNow::Now => now_secs(),
            };
        }

        if let Err(e) = self.inode_db.update(&entry) {
            tracing::error!(target: "fuse::setattr", ino, error = %e, "Failed to update inode");
            reply.error(EIO);
            return;
        }

        let attr = inode_to_attr(&entry);
        reply.attr(&TTL, &attr);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entry = match self.inode_db.get(ino) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if !entry.is_directory {
            reply.error(ENOTDIR);
            return;
        }

        // If directory hasn't been populated, trigger a fetch
        if !entry.populated {
            if let Some(local_path) = self.resolve_local_path(ino) {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = self.command_tx.send(MountCommand::FetchPlaceholders {
                    path: local_path,
                    response: tx,
                });
                let _ = rx.blocking_recv();
            }
        }

        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (
                entry.parent_inode.max(ROOT_INODE),
                FileType::Directory,
                "..".to_string(),
            ),
        ];

        let children = self.inode_db.list_children(ino);
        for child in children {
            let kind = if child.is_directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child.inode, kind, child.name.clone()));
        }

        for (i, (inode, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*inode, (i + 1) as i64, *kind, name) {
                break;
            }
        }

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.inode_db.get(ino) {
            Some(_) => {
                let fh = self.handles.open(ino, flags);
                reply.opened(fh, 0);
            }
            None => reply.error(ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // Fast path: try reading from cache
        if let Some(data) = self.cache.try_read(ino, offset as u64, size) {
            reply.data(&data);
            return;
        }

        // Cache miss: need to hydrate from server
        let entry = match self.inode_db.get(ino) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if entry.is_directory {
            reply.error(EISDIR);
            return;
        }

        let local_path = match self.resolve_local_path(ino) {
            Some(p) => p,
            None => {
                reply.error(EIO);
                return;
            }
        };

        // Allocate cache file for the full file
        let file_size = entry.size as u64;
        if let Err(_) = self.cache.write_to_cache(ino, 0, &[]).and_then(|_| {
            let path = self.cache.cache_path_for_inode(ino);
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&path)?;
            f.set_len(file_size)?;
            Ok(())
        }) {
            reply.error(EIO);
            return;
        }

        let writer: Box<dyn HydrationWriter> = Box::new(FuseHydrationWriter {
            inode: ino,
            cache: self.cache.clone(),
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.command_tx.send(MountCommand::FetchData {
            path: local_path,
            writer,
            range: 0..file_size,
            response: tx,
        });

        match rx.blocking_recv() {
            Ok(Ok(())) => {
                // Mark as cached
                let _ = self.cache.mark_inode_cached(ino);

                // Now read the requested range from cache
                match self.cache.try_read(ino, offset as u64, size) {
                    Some(data) => reply.data(&data),
                    None => reply.error(EIO),
                }
            }
            _ => reply.error(EIO),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let mut entry = match self.inode_db.get(ino) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if entry.is_directory {
            reply.error(EISDIR);
            return;
        }

        // Write to cache file
        if let Err(e) = self.cache.write_to_cache(ino, offset as u64, data) {
            tracing::error!(target: "fuse::write", ino, error = %e, "Failed to write to cache");
            reply.error(EIO);
            return;
        }

        // Update size if needed
        let new_end = offset as i64 + data.len() as i64;
        if new_end > entry.size {
            entry.size = new_end;
        }
        entry.modified_at = now_secs();

        // Mark dirty
        if let Err(e) = self.cache.mark_inode_dirty(ino) {
            tracing::error!(target: "fuse::write", ino, error = %e, "Failed to mark dirty");
        }
        entry.cache_state = InodeCacheState::Dirty;

        if let Err(e) = self.inode_db.update(&entry) {
            tracing::error!(target: "fuse::write", ino, error = %e, "Failed to update inode");
        }

        reply.written(data.len() as u32);
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EINVAL);
                return;
            }
        };

        // Check if already exists
        if self.inode_db.lookup_child(parent, name_str).is_some() {
            reply.error(EEXIST);
            return;
        }

        let now = now_secs();
        let ino = self.inode_db.alloc_inode();
        let entry = InodeEntry {
            inode: ino,
            parent_inode: parent,
            name: name_str.to_string(),
            is_directory: false,
            size: 0,
            etag: String::new(),
            created_at: now,
            modified_at: now,
            accessed_at: now,
            cache_state: InodeCacheState::Dirty,
            pinned: false,
            has_error: false,
            populated: false,
        };

        if let Err(e) = self.inode_db.insert(entry.clone()) {
            tracing::error!(target: "fuse::create", ino, error = %e, "Failed to insert inode");
            reply.error(EIO);
            return;
        }

        // Create empty cache file
        let cache_path = self.cache.cache_path_for_inode(ino);
        if let Err(e) = std::fs::File::create(&cache_path) {
            tracing::error!(target: "fuse::create", ino, error = %e, "Failed to create cache file");
        }

        let fh = self.handles.open(ino, _flags);
        let attr = inode_to_attr(&entry);
        reply.created(&TTL, &attr, 0, fh, 0);

        // Dispatch sync for the new file (will trigger upload)
        if let Some(local_path) = self.resolve_local_path(ino) {
            self.dispatch_sync(vec![local_path]);
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EINVAL);
                return;
            }
        };

        if self.inode_db.lookup_child(parent, name_str).is_some() {
            reply.error(EEXIST);
            return;
        }

        let now = now_secs();
        let ino = self.inode_db.alloc_inode();
        let entry = InodeEntry {
            inode: ino,
            parent_inode: parent,
            name: name_str.to_string(),
            is_directory: true,
            size: 0,
            etag: String::new(),
            created_at: now,
            modified_at: now,
            accessed_at: now,
            cache_state: InodeCacheState::NotCached,
            pinned: false,
            has_error: false,
            populated: true, // New empty directory is "populated"
        };

        if let Err(e) = self.inode_db.insert(entry.clone()) {
            tracing::error!(target: "fuse::mkdir", ino, error = %e, "Failed to insert inode");
            reply.error(EIO);
            return;
        }

        let attr = inode_to_attr(&entry);
        reply.entry(&TTL, &attr, 0);

        // Dispatch sync for the new directory
        if let Some(local_path) = self.resolve_local_path(ino) {
            self.dispatch_sync(vec![local_path]);
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let child = match self.inode_db.lookup_child(parent, name_str) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if child.is_directory {
            reply.error(EISDIR);
            return;
        }

        let local_path = self.resolve_local_path(child.inode);

        // Evict cache file
        let _ = self.cache.evict_inode(child.inode);

        // Remove from inode db
        if let Err(e) = self.inode_db.delete(child.inode) {
            tracing::error!(target: "fuse::unlink", ino = child.inode, error = %e, "Failed to delete inode");
            reply.error(EIO);
            return;
        }

        reply.ok();

        // Dispatch sync to propagate deletion
        if let Some(path) = local_path {
            self.dispatch_sync(vec![path]);
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let child = match self.inode_db.lookup_child(parent, name_str) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if !child.is_directory {
            reply.error(ENOTDIR);
            return;
        }

        // Check if directory is empty
        let children = self.inode_db.list_children(child.inode);
        if !children.is_empty() {
            reply.error(ENOTEMPTY);
            return;
        }

        let local_path = self.resolve_local_path(child.inode);

        if let Err(e) = self.inode_db.delete(child.inode) {
            tracing::error!(target: "fuse::rmdir", ino = child.inode, error = %e, "Failed to delete inode");
            reply.error(EIO);
            return;
        }

        reply.ok();

        if let Some(path) = local_path {
            self.dispatch_sync(vec![path]);
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EINVAL);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(EINVAL);
                return;
            }
        };

        let child = match self.inode_db.lookup_child(parent, name_str) {
            Some(e) => e,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let old_local_path = self.resolve_local_path(child.inode);

        // If target exists, remove it first
        if let Some(existing) = self.inode_db.lookup_child(newparent, newname_str) {
            let _ = self.cache.evict_inode(existing.inode);
            let _ = self.inode_db.delete(existing.inode);
        }

        if let Err(e) = self.inode_db.rename(child.inode, newparent, newname_str) {
            tracing::error!(target: "fuse::rename", ino = child.inode, error = %e, "Failed to rename inode");
            reply.error(EIO);
            return;
        }

        reply.ok();

        // Dispatch rename command and sync
        let new_local_path = self.resolve_local_path(child.inode);
        if let (Some(source), Some(dest)) = (old_local_path, new_local_path) {
            let _ = self.command_tx.send(MountCommand::Renamed {
                source: source.clone(),
                destination: dest.clone(),
            });
            self.dispatch_sync(vec![source, dest]);
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        // Flush is called before close; ensure data is written to cache
        let _ = self.inode_db.touch_accessed(ino);
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.release(fh);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.inode_db.get(ino) {
            Some(e) if e.is_directory => {
                let fh = self.handles.open(ino, _flags);
                reply.opened(fh, 0);
            }
            Some(_) => reply.error(ENOTDIR),
            None => reply.error(ENOENT),
        }
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.handles.release(fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: fuser::ReplyStatfs) {
        // Return reasonable defaults
        reply.statfs(
            0,          // blocks
            0,          // bfree
            0,          // bavail
            0,          // files
            0,          // ffree
            BLOCK_SIZE, // bsize
            255,        // namelen
            BLOCK_SIZE, // frsize
        );
    }

    fn access(&mut self, _req: &Request, ino: u64, _mask: i32, reply: ReplyEmpty) {
        if self.inode_db.get(ino).is_some() {
            reply.ok();
        } else {
            reply.error(ENOENT);
        }
    }
}
