pub mod ticket {
    use crate::cfapi::utility::WriteAt;
    use std::io;

    #[derive(Debug, Clone, Default)]
    pub struct FetchData;

    impl FetchData {
        pub fn report_progress(&self, _total: u64, _transferred: u64) -> io::Result<()> {
            Ok(())
        }
    }

    impl WriteAt for FetchData {
        fn write_at(&self, _data: &[u8], _offset: u64) -> io::Result<()> {
            Ok(())
        }
    }
}
