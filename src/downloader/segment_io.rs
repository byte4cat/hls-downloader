use anyhow::{Result, anyhow};
use egui::Context as EguiContext;
use futures::FutureExt; // For FutureExt::map on JoinHandle
use futures::stream::{self, StreamExt};
use reqwest::{Client, StatusCode, Url};
use std::fs::File;
use std::io::{self};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::sleep;

// 引入解密和 HLS 相關類型
use super::DownloadMessage;
use super::hls_parser::{EncryptionInfo, KEY_LEN, MAX_RETRIES, Segment};
use crate::downloader::ffmpeg_embed::FFmpegHandle;

// Decryption imports
use aes::Aes128;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use block_padding::Pkcs7;
use cbc::Decryptor;

type Aes128CbcDec = Decryptor<Aes128>;

/// Concurrently downloads all segments and returns their temporary paths, updating progress via MPSC.
pub async fn download_segments_concurrently(
    base_url: &Url,
    segments: Vec<Segment>,
    encryption_info: Option<EncryptionInfo>,
    key_bytes: Option<[u8; KEY_LEN]>,
    total_segments: usize,
    max_concurrent_downloads: usize,
    temp_dir_path: PathBuf,
    sender: mpsc::Sender<DownloadMessage>,
    ctx: EguiContext,
) -> Result<Vec<PathBuf>> {
    let client = Client::new();
    let completed_counter = Arc::new(AtomicUsize::new(0));

    // 使用 tokio::sync::Mutex 解決跨 .await 持有鎖的問題
    let last_progress_log = Arc::new(Mutex::new(String::new()));

    // A. Start the progress update task
    let total_segments_f = total_segments as f32;
    let completed_counter_clone = completed_counter.clone();

    // Update progress bar every 200ms
    let progress_handle = tokio::spawn({
        let sender = sender.clone();
        let ctx = ctx.clone();
        let last_progress_log_clone = last_progress_log.clone();

        async move {
            loop {
                sleep(Duration::from_millis(200)).await;
                let current =
                    completed_counter_clone.load(std::sync::atomic::Ordering::SeqCst) as f32;
                let progress = current / total_segments_f * 0.99; // Leave a little for merging/FFmpeg

                sender.send(DownloadMessage::Progress(progress)).await.ok();

                let progress_msg = format!(
                    "📦 Segment progress: {}/{} ({:.2}%)",
                    current as usize,
                    total_segments,
                    (current / total_segments_f) * 100.0
                );

                // 實作去重邏輯
                let mut last_log_guard = last_progress_log_clone.lock().await;

                // 只有當新訊息與上次發送的訊息不同時，才發送並更新紀錄
                if *last_log_guard != progress_msg {
                    sender
                        .send(DownloadMessage::Log(progress_msg.clone()))
                        .await
                        .ok();
                    *last_log_guard = progress_msg;
                }

                ctx.request_repaint();
            }
        }
    });

    // 2. Concurrent Download Logic
    let results: Vec<std::result::Result<PathBuf, anyhow::Error>> = stream::iter(segments)
        .map(|segment| {
            let client = client.clone();
            let base_url = base_url.clone();
            let completed_counter_clone = completed_counter.clone();
            let key_bytes_clone = key_bytes.clone();
            let encryption_info_clone = encryption_info.clone();
            let temp_dir_path_clone = temp_dir_path.clone();
            let segment_url = base_url.join(&segment.path).unwrap();
            let segment_index = segment.index;

            tokio::spawn(async move {
                let temp_filename = format!("temp_segment_{:08}.ts", segment_index);
                let temp_path = temp_dir_path_clone.join(&temp_filename);

                // Download segment
                download_and_process_segment(
                    client,
                    segment_url.as_str(),
                    &temp_path,
                    segment_index,
                    key_bytes_clone,
                    encryption_info_clone,
                )
                .await?;

                // Update segment counter
                let _ =
                    completed_counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

                Ok(temp_path)
            })
            .map(|join_result| {
                // Flatten Result<Result<T, E>, JoinError> to Result<T, E>
                join_result
                    .map_err(|e| anyhow!("Task Join Error: {}", e))
                    .flatten()
            })
        })
        .buffer_unordered(max_concurrent_downloads)
        .collect()
        .await;

    // Stop the progress update task
    progress_handle.abort();

    // 3. Collect and process results
    let mut downloaded_paths = Vec::new();
    for res in results {
        let path = res?; // Unwrap the single Result<PathBuf, anyhow::Error>
        downloaded_paths.push(path);
    }

    if downloaded_paths.len() != total_segments {
        return Err(anyhow!(
            "Concurrent download failed, not all segments were downloaded."
        ));
    }

    // Sort by index (Note: This relies on the index format "temp_segment_000000XX.ts")
    downloaded_paths.sort_by_key(|p| {
        p.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .replace("temp_segment_", "")
            .parse::<usize>()
            .unwrap_or(0)
    });

    Ok(downloaded_paths)
}

/// Downloads, decrypts, and saves a single segment to the specified temporary path
async fn download_and_process_segment(
    client: Client,
    url: &str,
    path: &Path,
    index: usize,
    key_bytes: Option<[u8; KEY_LEN]>,
    encryption_info: Option<EncryptionInfo>,
) -> Result<usize> {
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 0..MAX_RETRIES {
        let result = client.get(url).send().await;

        match result {
            Ok(response) => {
                let status = response.status();

                if status.is_success() {
                    let encrypted_bytes = response.bytes().await?;
                    let segment_size = encrypted_bytes.len();

                    // --- Decryption Logic ---
                    let decrypted_bytes = match (key_bytes, encryption_info) {
                        (Some(key), Some(info)) => {
                            let iv: [u8; KEY_LEN] = if let Some(explicit_iv) = info.iv_bytes {
                                explicit_iv
                            } else {
                                let mut iv = [0u8; KEY_LEN];
                                let sequence_number = (index as u32).to_be_bytes();
                                iv[12..].copy_from_slice(&sequence_number);
                                iv
                            };
                            let cipher = Aes128CbcDec::new(&key.into(), &iv.into());
                            let data = encrypted_bytes.to_vec();
                            cipher.decrypt_padded_vec_mut::<Pkcs7>(&data).map_err(|e| {
                                anyhow!("Segment {} decryption failed: {:?}", index, e)
                            })?
                        }
                        _ => encrypted_bytes.to_vec(),
                    };
                    // --- Write to file ---
                    let mut file = tokio::fs::File::create(path).await?;
                    file.write_all(&decrypted_bytes).await?;
                    return Ok(segment_size);
                }

                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    if attempt == MAX_RETRIES - 1 {
                        last_error = Some(anyhow!(
                            "Segment {} download failed, status code: {}",
                            index,
                            status
                        ));
                        break;
                    }
                    let actual_delay = (2u64.pow(attempt as u32)).max(3);
                    sleep(Duration::from_secs(actual_delay)).await;
                    continue;
                } else {
                    return Err(anyhow!(
                        "Segment {} download failed, status code: {}",
                        index,
                        status
                    ));
                }
            }
            Err(e) => {
                if attempt == MAX_RETRIES - 1 {
                    last_error = Some(anyhow!(
                        "Segment {} download failed, connection error: {}",
                        index,
                        e
                    ));
                    break;
                }
                let actual_delay = (2u64.pow(attempt as u32)).max(3);
                sleep(Duration::from_secs(actual_delay)).await;
                continue;
            }
        }
    }
    match last_error {
        Some(e) => Err(e),
        None => Err(anyhow!(
            "Segment {} download failed, maximum retries reached ({} times).",
            index,
            MAX_RETRIES
        )),
    }
}

/// Concatenates all temporary downloaded segments in order into a single output file.
pub fn concatenate_segments(segment_paths: &[PathBuf], output_path: &Path) -> Result<()> {
    let mut output_file = File::create(output_path)?;
    for path in segment_paths {
        let mut segment_file = File::open(path)?;
        io::copy(&mut segment_file, &mut output_file)?;
    }
    Ok(())
}

/// Uses FFmpeg to remux the temporary TS file to the desired output format.
pub fn run_ffmpeg_remux(input_path: &Path, output_path: &Path) -> Result<()> {
    let ff = FFmpegHandle::ensure()?;
    let ff_path = ff.path();
    let output = Command::new(ff_path)
        .arg("-i")
        .arg(input_path)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-y")
        .arg(output_path)
        .output()?;

    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "FFmpeg execution failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
