use std::io;

pub trait WriteAt {
    fn write_at(&self, data: &[u8], offset: u64) -> io::Result<()>;
}
