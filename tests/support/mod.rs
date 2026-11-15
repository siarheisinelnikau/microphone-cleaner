use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-fixtures")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
        assert!(self.0.starts_with(root) && self.0.file_name().is_some());
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
