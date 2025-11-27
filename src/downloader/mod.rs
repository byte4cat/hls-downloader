use anyhow::{Result, anyhow};
use egui::Context as EguiContext;
use reqwest::Url;
use std::path::PathBuf;
use tempfile::tempdir;
use tokio::sync::mpsc;

// 導出子模組
pub mod hls_parser;
pub mod segment_io;
pub mod util;

// 從子模組引入需要的類型和函數
use hls_parser::{download_and_parse_m3u3, download_key_file};
use segment_io::{concatenate_segments, download_segments_concurrently, run_ffmpeg_remux};
use util::PathStringLossy; // 引入 helper trait

// --- HLS related structs and constants ---
pub const DEFAULT_CONCURRENT_DOWNLOADS: u8 = 4;

// --- Egui/MPSC bridge structs and messages ---

/// Defines the event types an HLS download task can emit
#[derive(Debug)]
pub enum DownloadMessage {
    Log(String),
    Progress(f32), // 0.0 to 1.0 (overall progress)
    Finished(Result<(), String>),
    OutputPathSelected(String),
}

/// Core download logic
pub async fn run_hls_download_core(
    playlist_url_str: String,
    output_location: String,
    output_filename: String,
    max_concurrent_downloads: usize,
    output_format: String,
    sender: mpsc::Sender<DownloadMessage>,
    ctx: EguiContext,
) -> Result<()> {
    // Helper function to send log messages to the GUI
    let send_log = |msg: String| {
        let sender_clone = sender.clone();
        let ctx_clone = ctx.clone();
        tokio::spawn(async move {
            sender_clone.send(DownloadMessage::Log(msg)).await.ok();
            ctx_clone.request_repaint();
        });
    };

    // 1. Parameter Handling
    let playlist_url = Url::parse(&playlist_url_str).map_err(|e| anyhow!("Invalid URL: {}", e))?;

    let initial_filename_path = PathBuf::from(&output_filename);
    let final_format = output_format.to_lowercase();
    let needs_remuxing = final_format != "ts";

    let mut corrected_filename_only = initial_filename_path.clone();

    // Adjust filename extension logic
    if needs_remuxing {
        let current_ext = initial_filename_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("");
        if !current_ext.eq_ignore_ascii_case(&final_format) {
            let new_filename = format!(
                "{}.{}",
                initial_filename_path.file_string_lossy(),
                final_format
            );
            corrected_filename_only = PathBuf::from(new_filename);
        }
    } else {
        // If no conversion needed, ensure it's a .ts extension
        let new_filename = format!("{}.ts", initial_filename_path.file_string_lossy(),);
        corrected_filename_only = PathBuf::from(new_filename);
    }

    let final_directory = PathBuf::from(output_location);
    let final_output_path = final_directory.join(corrected_filename_only);

    send_log("📦 Creating safe temporary directory for segments...".to_string());
    let temp_dir_handle = tokio::task::spawn_blocking(|| {
        // tempdir() 是一個同步操作，需要在 blocking thread 中運行
        tempdir().map_err(|e| anyhow!("Failed to create temporary directory: {}", e))
    })
    .await
    .map_err(|e| anyhow!("Tempdir creation blocking task failed: {}", e))??;

    // 獲取該臨時目錄的路徑
    let temp_dir_path = temp_dir_handle.path().to_path_buf();

    send_log(format!(
        "-> Temporary directory set: {} (Auto-cleanup on exit)",
        temp_dir_path.display()
    ));

    let temp_ts_filename = "final_merge.ts.tmp".to_string();
    let temp_ts_path = temp_dir_path.join(&temp_ts_filename);

    send_log(format!("-> Downloading playlist: {}", playlist_url));
    send_log(format!(
        "-> Concurrent downloads: {}",
        max_concurrent_downloads
    ));
    send_log(format!("-> Final output format: {}", final_format));
    if initial_filename_path.file_name() != final_output_path.file_name() {
        send_log(format!(
            "    Note: Output filename adjusted to: {}",
            final_output_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
    }

    // 2. Download and Parse M3U8 file
    let (segments, mut encryption_info) = download_and_parse_m3u3(&playlist_url, &send_log).await?;
    let key_bytes = match encryption_info.as_mut() {
        Some(info) => {
            send_log(format!(
                "-> Encryption detected: {}. Downloading key...",
                info.method
            ));
            send_log(format!("  Key URI: {}", info.key_url));
            info.key_bytes = Some(download_key_file(&info.key_url, &send_log).await?);
            if let Some(key) = info.key_bytes {
                let key_hex = hex::encode(key);
                send_log(format!(
                    "🔑 Key (Hex): {} [{} bytes]",
                    key_hex,
                    hex::encode(key).len() / 2
                ));
            }
            info.key_bytes
        }
        None => {
            send_log("-> No #EXT-X-KEY tag detected, assuming content is unencrypted.".to_string());
            None
        }
    };

    // 3. Concurrent Segment Download
    let total_segments = segments.len();
    let downloaded_segments = download_segments_concurrently(
        &playlist_url,
        segments,
        encryption_info,
        key_bytes,
        total_segments,
        max_concurrent_downloads,
        temp_dir_path.clone(),
        sender.clone(),
        ctx.clone(),
    )
    .await?;

    // 4. Concatenate segments to a temporary TS file
    send_log(format!(
        "\n-> Concatenating segments to temporary file {}...",
        temp_ts_path.display()
    ));

    let concat_segments = downloaded_segments.clone();
    let concat_temp_ts_path = temp_ts_path.clone();

    tokio::task::spawn_blocking(move || {
        concatenate_segments(&concat_segments, &concat_temp_ts_path)
    })
    .await
    .map_err(|e| anyhow!("Concatenation blocking task failed to join: {}", e))??;

    // 5. Clean up temporary segment files
    send_log("-> Cleaning up temporary segment files...".to_string());
    for path in downloaded_segments {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            send_log(format!(
                "⚠️ Warning: Failed to delete temporary segment file {}: {}",
                path.display(),
                e
            ));
        }
    }

    final_directory.to_string_lossy().into_owned();

    // 6. Check and execute FFmpeg conversion
    if needs_remuxing {
        send_log(format!("🚀 Remuxing using FFmpeg to {}...", final_format));

        // 將 `run_ffmpeg_remux` 移入 spawn_blocking
        let ffmpeg_temp_ts_path = temp_ts_path.clone();
        let ffmpeg_final_output_path = final_output_path.clone();

        let ffmpeg_result = tokio::task::spawn_blocking(move || {
            run_ffmpeg_remux(&ffmpeg_temp_ts_path, &ffmpeg_final_output_path)
        })
        .await
        .map_err(|e| anyhow!("FFmpeg blocking task failed to join: {}", e))?; // 處理 JoinError

        match ffmpeg_result {
            Ok(()) => {
                sender.send(DownloadMessage::Progress(1.0)).await.ok();
                ctx.request_repaint();
                send_log(format!(
                    "✅ FFmpeg conversion successful! File saved as: {}",
                    final_output_path.display()
                ));
            }
            Err(e) => {
                send_log(format!(
                    "\n⚠️ FFmpeg conversion failed: {}. Please ensure FFmpeg is installed and in your PATH.",
                    e
                ));
                send_log(format!(
                    "  Original concatenated file (TS format) retained as: {}",
                    temp_ts_path.display()
                ));
            }
        }

        if let Err(e) = tokio::fs::remove_file(&temp_ts_path).await {
            send_log(format!(
                "⚠️ Warning: Failed to delete temporary concatenated file {}: {}",
                temp_ts_path.display(),
                e
            ));
        }
    } else {
        send_log(format!(
            "-> Output format is TS, renaming concatenated file to {}...",
            final_output_path.display()
        ));
        tokio::fs::rename(&temp_ts_path, &final_output_path).await?;
    }

    Ok(())
}
