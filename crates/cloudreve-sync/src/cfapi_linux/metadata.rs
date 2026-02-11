use nt_time::FileTime;

#[derive(Debug, Clone, Copy, Default)]
pub struct Metadata;

impl Metadata {
    pub fn file() -> Self {
        Self
    }

    pub fn directory() -> Self {
        Self
    }

    pub fn created(self, _time: FileTime) -> Self {
        self
    }

    pub fn accessed(self, _time: FileTime) -> Self {
        self
    }

    pub fn written(self, _time: FileTime) -> Self {
        self
    }

    pub fn changed(self, _time: FileTime) -> Self {
        self
    }

    pub fn size(self, _size: u64) -> Self {
        self
    }

    pub fn attributes(self, _attributes: u32) -> Self {
        self
    }
}
