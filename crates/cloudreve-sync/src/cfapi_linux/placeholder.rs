use std::fs;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinState {
    #[default]
    Unspecified,
    Pinned,
    Unpinned,
    Excluded,
    Inherit,
}

#[derive(Debug, Clone)]
pub struct LocalFileInfo {
    pub exists: bool,
    pub is_directory: bool,
    pub file_size: Option<u64>,
    pub last_modified: Option<SystemTime>,
}

impl LocalFileInfo {
    pub fn missing() -> Self {
        Self {
            exists: false,
            is_directory: false,
            file_size: None,
            last_modified: None,
        }
    }

    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        match fs::metadata(path) {
            Ok(meta) => Ok(Self {
                exists: true,
                is_directory: meta.is_dir(),
                file_size: meta.is_file().then_some(meta.len()),
                last_modified: meta.modified().ok(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::missing()),
            Err(e) => Err(e),
        }
    }

    pub fn in_sync(&self) -> bool {
        false
    }

    pub fn is_placeholder(&self) -> bool {
        false
    }

    pub fn partial_on_disk(&self) -> bool {
        false
    }

    pub fn pinned(&self) -> PinState {
        PinState::Unspecified
    }

    pub fn is_directory(&self) -> bool {
        self.is_directory
    }

    pub fn is_folder_populated(&self) -> bool {
        self.exists && self.is_directory
    }
}

#[derive(Debug, Clone)]
pub struct OpenOptions;

impl OpenOptions {
    pub fn new() -> Self {
        Self
    }

    pub fn exclusive(self) -> Self {
        self
    }

    pub fn write_access(self) -> Self {
        self
    }

    pub fn delete_access(self) -> Self {
        self
    }

    pub fn foreground(self) -> Self {
        self
    }

    pub fn open_win32(self, path: impl AsRef<Path>) -> std::io::Result<Placeholder> {
        Ok(Placeholder {
            path: path.as_ref().to_path_buf(),
        })
    }

    pub fn open(self, path: impl AsRef<Path>) -> std::io::Result<Placeholder> {
        Ok(Placeholder {
            path: path.as_ref().to_path_buf(),
        })
    }

    pub async fn open_win32_with_retry(
        self,
        path: impl AsRef<Path>,
    ) -> std::io::Result<Placeholder> {
        Ok(Placeholder {
            path: path.as_ref().to_path_buf(),
        })
    }

    pub async fn open_with_retry(self, path: impl AsRef<Path>) -> std::io::Result<Placeholder> {
        Ok(Placeholder {
            path: path.as_ref().to_path_buf(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Placeholder {
    #[allow(dead_code)]
    path: PathBuf,
}

impl Placeholder {
    pub fn hydrate(&mut self, _range: impl RangeBounds<u64>) -> std::io::Result<()> {
        Ok(())
    }

    pub fn dehydrate(&mut self, _range: impl RangeBounds<u64>) -> std::io::Result<()> {
        Ok(())
    }

    pub fn mark_in_sync(&mut self, _in_sync: bool, _usn: Option<i64>) -> std::io::Result<()> {
        Ok(())
    }
}
