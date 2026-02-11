use crate::cfapi::metadata::Metadata;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PlaceholderFile {
    #[allow(dead_code)]
    relative_path: std::path::PathBuf,
}

impl PlaceholderFile {
    pub fn new(relative_path: impl AsRef<Path>) -> Self {
        Self {
            relative_path: relative_path.as_ref().to_path_buf(),
        }
    }

    pub fn has_no_children(self) -> Self {
        self
    }

    pub fn mark_in_sync(self) -> Self {
        self
    }

    pub fn overwrite(self) -> Self {
        self
    }

    pub fn block_dehydration(self) -> Self {
        self
    }

    pub fn metadata(self, _metadata: Metadata) -> Self {
        self
    }

    pub fn blob(self, _blob: Vec<u8>) -> Self {
        self
    }

    pub fn create<P: AsRef<Path>>(self, _parent: impl AsRef<Path>) -> std::io::Result<i64> {
        let _ = std::marker::PhantomData::<P>;
        Ok(0)
    }
}
