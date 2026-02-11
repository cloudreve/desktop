use anyhow::{Context, Result};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
    spawn_mount2,
};
use libc::{EIO, ENOENT};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime};

const TTL: Duration = Duration::from_secs(1);
const MOUNT_MARKER_FILE: &str = ".cloudreve_fuse_active";

struct HandleTable {
    next: AtomicU64,
    files: Mutex<HashMap<u64, File>>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            files: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, file: File) -> u64 {
        let fh = self.next.fetch_add(1, Ordering::Relaxed);
        self.files.lock().expect("lock poisoned").insert(fh, file);
        fh
    }

    fn with_file<T>(&self, fh: u64, f: impl FnOnce(&File) -> T) -> Option<T> {
        let map = self.files.lock().expect("lock poisoned");
        map.get(&fh).map(f)
    }

    fn remove(&self, fh: u64) {
        self.files.lock().expect("lock poisoned").remove(&fh);
    }
}

#[derive(Default)]
struct InodeTable {
    path_to_ino: HashMap<PathBuf, u64>,
    ino_to_path: HashMap<u64, PathBuf>,
    next: u64,
}

impl InodeTable {
    fn new(root: PathBuf) -> Self {
        let mut t = Self {
            path_to_ino: HashMap::new(),
            ino_to_path: HashMap::new(),
            next: 2,
        };
        t.path_to_ino.insert(root.clone(), 1);
        t.ino_to_path.insert(1, root);
        t
    }

    fn get_path(&self, ino: u64) -> Option<PathBuf> {
        self.ino_to_path.get(&ino).cloned()
    }

    fn get_or_insert(&mut self, path: PathBuf) -> u64 {
        if let Some(ino) = self.path_to_ino.get(&path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.path_to_ino.insert(path.clone(), ino);
        self.ino_to_path.insert(ino, path);
        ino
    }

    fn remove_path(&mut self, path: &Path) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }
}

pub struct PassthroughFs {
    inodes: RwLock<InodeTable>,
    handles: HandleTable,
}

impl PassthroughFs {
    pub fn new(root: PathBuf) -> Self {
        Self {
            inodes: RwLock::new(InodeTable::new(root.clone())),
            handles: HandleTable::new(),
        }
    }

    fn path_for_ino(&self, ino: u64) -> Option<PathBuf> {
        self.inodes.read().expect("lock poisoned").get_path(ino)
    }

    fn ino_for_path(&self, path: PathBuf) -> u64 {
        self.inodes.write().expect("lock poisoned").get_or_insert(path)
    }

    fn remove_path(&self, path: &Path) {
        self.inodes.write().expect("lock poisoned").remove_path(path);
    }

    fn attr_for_path(&self, path: &Path, ino: u64) -> std::io::Result<FileAttr> {
        let meta = fs::symlink_metadata(path)?;
        let kind = if meta.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let perm = (meta.permissions().mode() & 0o7777) as u16;
        Ok(FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime: meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ctime: meta.created().unwrap_or(SystemTime::UNIX_EPOCH),
            crtime: meta.created().unwrap_or(SystemTime::UNIX_EPOCH),
            kind,
            perm,
            nlink: meta.nlink() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: meta.rdev() as u32,
            blksize: meta.blksize() as u32,
            flags: 0,
        })
    }

    fn open_file(path: &Path, flags: i32) -> std::io::Result<File> {
        let acc = flags & libc::O_ACCMODE;
        let mut opts = OpenOptions::new();
        opts.custom_flags(flags);
        if acc == libc::O_WRONLY {
            opts.write(true);
        } else if acc == libc::O_RDWR {
            opts.read(true).write(true);
        } else {
            opts.read(true);
        }
        opts.open(path)
    }
}

impl Filesystem for PassthroughFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(mut path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        path.push(name);
        if !path.exists() {
            reply.error(ENOENT);
            return;
        }
        let ino = self.ino_for_path(path.clone());
        match self.attr_for_path(&path, ino) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(_) => reply.error(EIO),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.path_for_ino(ino) else {
            reply.error(ENOENT);
            return;
        };
        match self.attr_for_path(&path, ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(_) => reply.error(EIO),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path_for_ino(ino) else {
            reply.error(ENOENT);
            return;
        };

        let mut entries: Vec<(u64, FileType, OsString)> = Vec::new();
        entries.push((ino, FileType::Directory, OsString::from(".")));
        entries.push((1, FileType::Directory, OsString::from("..")));

        match fs::read_dir(&path) {
            Ok(dir) => {
                for entry in dir.flatten() {
                    let child_path = entry.path();
                    let child_ino = self.ino_for_path(child_path.clone());
                    let kind = match entry.file_type() {
                        Ok(t) if t.is_dir() => FileType::Directory,
                        Ok(_) => FileType::RegularFile,
                        Err(_) => FileType::RegularFile,
                    };
                    entries.push((child_ino, kind, entry.file_name()));
                }
            }
            Err(_) => {
                reply.error(EIO);
                return;
            }
        }

        for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            let full = reply.add(entry_ino, (i + 1) as i64, kind, name);
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(path) = self.path_for_ino(ino) else {
            reply.error(ENOENT);
            return;
        };
        match Self::open_file(&path, flags) {
            Ok(file) => {
                let fh = self.handles.insert(file);
                reply.opened(fh, 0);
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(mut path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        path.push(name);

        let mut opts = OpenOptions::new();
        opts.create(true).write(true).read(true).mode(mode).custom_flags(flags);
        match opts.open(&path) {
            Ok(file) => {
                let ino = self.ino_for_path(path.clone());
                let fh = self.handles.insert(file);
                match self.attr_for_path(&path, ino) {
                    Ok(attr) => reply.created(&TTL, &attr, 0, fh, 0),
                    Err(_) => reply.error(EIO),
                }
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0_u8; size as usize];
        let Some(result) = self.handles.with_file(fh, |f| f.read_at(&mut buf, offset as u64)) else {
            reply.error(ENOENT);
            return;
        };

        match result {
            Ok(read) => reply.data(&buf[..read]),
            Err(_) => reply.error(EIO),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(result) = self.handles.with_file(fh, |f| f.write_at(data, offset as u64)) else {
            reply.error(ENOENT);
            return;
        };
        match result {
            Ok(written) => reply.written(written as u32),
            Err(_) => reply.error(EIO),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.remove(fh);
        reply.ok();
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(mut path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        path.push(name);
        match fs::create_dir(&path) {
            Ok(()) => {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
                let ino = self.ino_for_path(path.clone());
                match self.attr_for_path(&path, ino) {
                    Ok(attr) => reply.entry(&TTL, &attr, 0),
                    Err(_) => reply.error(EIO),
                }
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(mut path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        path.push(name);
        match fs::remove_file(&path) {
            Ok(()) => {
                self.remove_path(&path);
                reply.ok();
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(mut path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        path.push(name);
        match fs::remove_dir(&path) {
            Ok(()) => {
                self.remove_path(&path);
                reply.ok();
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(mut old_path) = self.path_for_ino(parent) else {
            reply.error(ENOENT);
            return;
        };
        old_path.push(name);
        let Some(mut new_path) = self.path_for_ino(newparent) else {
            reply.error(ENOENT);
            return;
        };
        new_path.push(newname);

        match fs::rename(&old_path, &new_path) {
            Ok(()) => {
                self.remove_path(&old_path);
                let _ = self.ino_for_path(new_path);
                reply.ok();
            }
            Err(_) => reply.error(EIO),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_for_ino(ino) else {
            reply.error(ENOENT);
            return;
        };

        if let Some(m) = mode {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(m));
        }
        if let Some(s) = size {
            let _ = OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(s));
        }

        match self.attr_for_path(&path, ino) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(_) => reply.error(EIO),
        }
    }
}

fn move_existing_children(src_mountpoint: &Path, backend_root: &Path) -> Result<()> {
    let mut src_entries = fs::read_dir(src_mountpoint)
        .with_context(|| format!("failed to read mountpoint {}", src_mountpoint.display()))?;
    let backend_empty = fs::read_dir(backend_root)
        .with_context(|| format!("failed to read backend {}", backend_root.display()))?
        .next()
        .is_none();

    if !backend_empty {
        return Ok(());
    }

    while let Some(entry) = src_entries.next() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".cloudreve_fuse_backend_") {
            continue;
        }
        let to = backend_root.join(&name);
        fs::rename(entry.path(), &to).with_context(|| {
            format!(
                "failed to migrate {} to {}",
                entry.path().display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

fn backend_root_for(sync_path: &Path, drive_id: &str) -> PathBuf {
    let parent = sync_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".cloudreve_fuse_backend_{}", drive_id))
}

fn marker_path(backend_root: &Path) -> PathBuf {
    backend_root.join(MOUNT_MARKER_FILE)
}

fn write_mount_marker(backend_root: &Path) -> Result<()> {
    let path = marker_path(backend_root);
    let mut f = File::create(&path)
        .with_context(|| format!("failed to create mount marker {}", path.display()))?;
    let _ = writeln!(f, "pid={}", std::process::id());
    Ok(())
}

pub fn clear_mount_marker(sync_path: &Path, drive_id: &str) -> Result<()> {
    let backend_root = backend_root_for(sync_path, drive_id);
    let path = marker_path(&backend_root);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove mount marker {}", path.display()))?;
    }
    Ok(())
}

pub fn has_stale_mount_marker(sync_path: &Path, drive_id: &str) -> Result<bool> {
    let backend_root = backend_root_for(sync_path, drive_id);
    Ok(marker_path(&backend_root).exists())
}

fn restore_backend_children(backend_root: &Path, sync_path: &Path) -> Result<usize> {
    if !backend_root.exists() {
        return Ok(0);
    }

    let mut restored = 0usize;
    let entries = fs::read_dir(backend_root)
        .with_context(|| format!("failed to read backend {}", backend_root.display()))?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == MOUNT_MARKER_FILE {
            continue;
        }
        let from = entry.path();
        let to = sync_path.join(&name);

        if to.exists() {
            tracing::warn!(
                target: "drive::fuse",
                from = %from.display(),
                to = %to.display(),
                "Skip restoring backend entry because destination already exists"
            );
            continue;
        }

        fs::rename(&from, &to).with_context(|| {
            format!(
                "failed to restore {} to {}",
                from.display(),
                to.display()
            )
        })?;
        restored += 1;
    }

    Ok(restored)
}

pub fn restore_from_backend(sync_path: &Path, drive_id: &str) -> Result<usize> {
    let backend_root = backend_root_for(sync_path, drive_id);
    fs::create_dir_all(sync_path)
        .with_context(|| format!("failed to ensure sync path {}", sync_path.display()))?;
    restore_backend_children(&backend_root, sync_path)
}

pub fn mount_experimental(sync_path: &Path, drive_id: &str) -> Result<BackgroundSession> {
    fs::create_dir_all(sync_path)
        .with_context(|| format!("failed to create mountpoint {}", sync_path.display()))?;

    let backend_root = backend_root_for(sync_path, drive_id);
    fs::create_dir_all(&backend_root)
        .with_context(|| format!("failed to create backend root {}", backend_root.display()))?;

    move_existing_children(sync_path, &backend_root)?;

    let fs = PassthroughFs::new(backend_root.clone());
    let options = vec![
        MountOption::FSName(String::from("cloudreve-sync")),
        MountOption::DefaultPermissions,
    ];

    match spawn_mount2(fs, sync_path, &options)
        .with_context(|| format!("failed to mount FUSE filesystem at {}", sync_path.display()))
    {
        Ok(session) => {
            write_mount_marker(&backend_root)?;
            Ok(session)
        }
        Err(e) => {
            // Best-effort rollback to avoid leaving user data hidden in backend dir.
            if let Err(rollback_err) = restore_backend_children(&backend_root, sync_path) {
                return Err(e).context(format!(
                    "failed to rollback migrated files after mount failure: {}",
                    rollback_err
                ));
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{move_existing_children, restore_backend_children};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn migrate_children_when_backend_empty() {
        let mount = TempDir::new().expect("temp mount");
        let backend = TempDir::new().expect("temp backend");

        let mount_file = mount.path().join("a.txt");
        fs::write(&mount_file, b"hello").expect("write mount file");

        move_existing_children(mount.path(), backend.path()).expect("migrate");

        assert!(!mount_file.exists());
        assert!(backend.path().join("a.txt").exists());
    }

    #[test]
    fn skip_migration_when_backend_not_empty() {
        let mount = TempDir::new().expect("temp mount");
        let backend = TempDir::new().expect("temp backend");

        let mount_file = mount.path().join("a.txt");
        fs::write(&mount_file, b"hello").expect("write mount file");
        fs::write(backend.path().join("existing.txt"), b"x").expect("seed backend");

        move_existing_children(mount.path(), backend.path()).expect("migrate");

        assert!(mount_file.exists());
        assert!(backend.path().join("existing.txt").exists());
    }

    #[test]
    fn restore_backend_children_without_overwrite() {
        let sync = TempDir::new().expect("temp sync");
        let backend = TempDir::new().expect("temp backend");

        fs::write(backend.path().join("a.txt"), b"from-backend").expect("seed backend file");
        fs::create_dir_all(backend.path().join("dir")).expect("seed backend dir");
        fs::write(backend.path().join("dir").join("b.txt"), b"x").expect("seed backend nested");

        fs::write(sync.path().join("a.txt"), b"existing").expect("seed sync conflict file");

        let restored =
            restore_backend_children(backend.path(), sync.path()).expect("restore from backend");

        assert_eq!(restored, 1);
        assert_eq!(
            fs::read(sync.path().join("a.txt")).expect("read sync existing"),
            b"existing"
        );
        assert!(sync.path().join("dir").join("b.txt").exists());
        assert!(backend.path().join("a.txt").exists());
        assert!(!backend.path().join("dir").exists());
    }
}
