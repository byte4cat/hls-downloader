use std::path::PathBuf;

// Helper Trait
pub trait PathStringLossy {
    fn file_string_lossy(&self) -> String;
}

impl PathStringLossy for PathBuf {
    fn file_string_lossy(&self) -> String {
        self.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    }
}
