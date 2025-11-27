// Best-practice embedded FFmpeg extraction/verification utility.
// - compressed payload embedded per-platform (zstd compressed)
// - extracted to user cache dir under a checksumed folder
// - verifies checksum, executable bit, and optional "ffmpeg -version" probe
// - extracts only on first-run / when checksum changes

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow};
use dirs::cache_dir;
use sha2::{Digest, Sha256};
use zstd::stream::copy_decode;

// Per-platform embedded compressed bytes (zstd). Replace asset paths with your actual files.
// Provide one compressed file per platform in your assets dir, e.g. assets/bin/linux/ffmpeg.zst
#[cfg(target_os = "linux")]
const COMPRESSED_FFMPEG: &[u8] = include_bytes!("../assets/bin/linux/ffmpeg.zst");
#[cfg(target_os = "macos")]
const COMPRESSED_FFMPEG: &[u8] = include_bytes!("../assets/bin/macos/ffmpeg.zst");
#[cfg(target_os = "windows")]
const COMPRESSED_FFMPEG: &[u8] = include_bytes!("../assets/bin/windows/ffmpeg.zst");

// The name we'll write the extracted executable as
#[cfg(target_os = "windows")]
const FFMPEG_FILENAME: &str = "ffmpeg.exe";
#[cfg(not(target_os = "windows"))]
const FFMPEG_FILENAME: &str = "ffmpeg";

/// Compute sha256 checksum of the compressed payload.
fn compressed_checksum() -> String {
    let mut hasher = Sha256::new();
    hasher.update(COMPRESSED_FFMPEG);
    hex::encode(hasher.finalize())
}

/// Return a platform-scoped cache directory path: <cache_dir>/hls-downloader/embedded-ffmpeg/<checksum>/
fn ffmpeg_cache_dir() -> Result<PathBuf> {
    let base = cache_dir().ok_or_else(|| anyhow!("Could not determine cache directory"))?;
    let dir = base
        .join("hls-downloader/embedded-ffmpeg")
        .join(compressed_checksum());
    Ok(dir)
}

/// Ensure an executable bit on unix platforms. No-op on Windows.
fn set_executable_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path)?.permissions();
        // Owner + group + others exec bits as 0o755
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
    }

    #[cfg(windows)]
    {
        // Windows does not have unix perms. We'll just return Ok(()).
        let _ = path;
        Ok(())
    }
}

/// Probe the extracted binary by running `ffmpeg -version` and ensure it runs.
fn probe_ffmpeg(exec_path: &Path) -> Result<()> {
    let out = Command::new(exec_path)
        .arg("-version")
        .output()
        .map_err(|e| anyhow!("Failed to spawn ffmpeg for probe: {}", e))?;

    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "ffmpeg probe failed: exit {} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Extracts the embedded compressed payload into `target_path`.
fn extract_to(target_path: &Path) -> Result<()> {
    // Create parent directory if missing
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Decompress zstd bytes directly into the output file
    let mut writer = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(target_path)?;

    // Use a zstd decoder reading from the bytes in memory
    let mut reader = COMPRESSED_FFMPEG;
    copy_decode(&mut reader, &mut writer).map_err(|e| anyhow!("Decompression failed: {}", e))?;

    // Ensure the writer is flushed
    writer.flush()?;

    // Set executable bit for unix
    set_executable_permissions(target_path)?;

    Ok(())
}

/// Public handle that ensures FFmpeg is extracted and returns the executable path.
pub struct FFmpegHandle {
    exec_path: PathBuf,
}

impl FFmpegHandle {
    /// Ensure ffmpeg is present in cache and valid. This extracts on first-run or when checksum changes.
    pub fn ensure() -> Result<Self> {
        let cache_dir = ffmpeg_cache_dir()?;
        let exec_path = cache_dir.join(FFMPEG_FILENAME);

        // if exec exists, do a cheap probe to ensure it's usable
        if exec_path.exists() {
            if probe_ffmpeg(&exec_path).is_ok() {
                return Ok(FFmpegHandle { exec_path });
            }
            // If probe fails, remove and re-extract
            let _ = fs::remove_file(&exec_path);
        }

        // Extract to the target path (first-run)
        extract_to(&exec_path)?;

        // Verify with probe; if it fails, remove and error out
        if let Err(e) = probe_ffmpeg(&exec_path) {
            let _ = fs::remove_file(&exec_path);
            return Err(anyhow!("ffmpeg probe after extraction failed: {}", e));
        }

        Ok(FFmpegHandle { exec_path })
    }

    /// Path to the ffmpeg executable
    pub fn path(&self) -> &Path {
        &self.exec_path
    }
}

// Optional: helper to return string path
impl std::fmt::Display for FFmpegHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.exec_path.display())
    }
}

// ---- Example usage  ----
//
// fn main() -> Result<()> {
//     let ff = FFmpegHandle::ensure()?;
//     let ff_path = ff.path();
//     // then use Command::new(ff_path) ...
//     Ok(())
// }
