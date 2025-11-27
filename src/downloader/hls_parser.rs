use anyhow::{Result, anyhow};
use hex;
use reqwest::{Client, StatusCode, Url};
use std::time::Duration;
use tokio::time::sleep;

pub const KEY_LEN: usize = 16;
pub const MAX_RETRIES: usize = 5;

// Stores segment information, including index
pub struct Segment {
    pub path: String,
    pub index: usize,
}

// Stores encryption information
#[derive(Debug, Clone)]
pub struct EncryptionInfo {
    pub key_url: Url,
    pub method: String,
    pub key_bytes: Option<[u8; KEY_LEN]>,
    pub iv_bytes: Option<[u8; KEY_LEN]>,
}

/// Downloads and parses the M3U8 file
pub async fn download_and_parse_m3u3(
    playlist_url: &Url,
    send_log: &impl Fn(String),
) -> Result<(Vec<Segment>, Option<EncryptionInfo>)> {
    let client = Client::new();
    let response = client
        .get(playlist_url.as_str())
        .send()
        .await?
        .error_for_status()?;
    let body = response.text().await?;

    let mut segments = Vec::new();
    let mut encryption_info: Option<EncryptionInfo> = None;
    let mut current_segment_index = 0;

    for line in body.lines() {
        let line = line.trim();

        if line.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            if let Some(seq_str) = line.split(':').nth(1) {
                if let Ok(seq) = seq_str.parse::<usize>() {
                    current_segment_index = seq;
                    send_log(format!(
                        "-> Detected #EXT-X-MEDIA-SEQUENCE: {}, segment index starts here.",
                        current_segment_index
                    ));
                }
            }
        } else if line.starts_with("#EXT-X-KEY") {
            let content = line.trim_start_matches("#EXT-X-KEY:").trim();
            let key_parts: Vec<&str> = content.split(',').collect();

            let mut key_url: Option<Url> = None;
            let mut method: Option<String> = None;
            let mut iv_bytes: Option<[u8; KEY_LEN]> = None;

            for part in key_parts {
                let part = part.trim();
                if part.starts_with("METHOD=") {
                    method = Some(part.trim_start_matches("METHOD=").to_string());
                } else if part.starts_with("URI=") {
                    let uri_str = part.trim_start_matches("URI=").trim_matches('"');
                    key_url = Some(playlist_url.join(uri_str)?);
                } else if part.starts_with("IV=") {
                    let iv_hex = part.trim_start_matches("IV=").trim_start_matches("0x");
                    if iv_hex.len() == KEY_LEN * 2 {
                        match hex::decode(iv_hex) {
                            Ok(bytes) if bytes.len() == KEY_LEN => {
                                let mut iv = [0u8; KEY_LEN];
                                iv.copy_from_slice(&bytes);
                                iv_bytes = Some(iv);
                                send_log(format!(
                                    "  Explicit IV found in M3U8: [{} bytes]",
                                    hex::encode(iv).len() / 2
                                ));
                            }
                            _ => send_log("⚠️ Warning: Failed to parse IV bytes.".to_string()),
                        }
                    } else {
                        send_log(format!(
                            "⚠️ Warning: Invalid IV length or format: {}",
                            iv_hex
                        ));
                    }
                }
            }

            if let (Some(url), Some(m)) = (key_url, method) {
                if m != "AES-128" {
                    return Err(anyhow!(
                        "Only AES-128 encryption is currently supported, detected {}",
                        m
                    ));
                }
                encryption_info = Some(EncryptionInfo {
                    key_url: url,
                    method: m,
                    key_bytes: None,
                    iv_bytes,
                });
            } else {
                send_log(
                    "⚠️ Warning: Detected #EXT-X-KEY tag, but URI or METHOD attributes are missing. Skipping encryption."
                        .to_string(),
                );
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            segments.push(Segment {
                path: line.to_string(),
                index: current_segment_index,
            });
            current_segment_index += 1;
        }
    }

    if segments.is_empty() {
        return Err(anyhow!("No media segments (.ts) found in the M3U8 file."));
    }

    Ok((segments, encryption_info))
}

/// Downloads the key file
pub async fn download_key_file(key_url: &Url, send_log: &impl Fn(String)) -> Result<[u8; KEY_LEN]> {
    let client = Client::new();
    for attempt in 0..MAX_RETRIES {
        match client.get(key_url.as_str()).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    let key_bytes = response.bytes().await?;
                    if key_bytes.len() != KEY_LEN {
                        return Err(anyhow!(
                            "Key file length error: Expected 16 bytes, got {}",
                            key_bytes.len()
                        ));
                    }
                    let mut key = [0u8; KEY_LEN];
                    key.copy_from_slice(&key_bytes);
                    return Ok(key);
                } else if response.status() == StatusCode::TOO_MANY_REQUESTS
                    && attempt < MAX_RETRIES - 1
                {
                    let delay = 2u64.pow(attempt as u32);
                    send_log(format!(
                        "⚠️ Warning: Received 429 error while downloading key. Retrying in {} seconds (Attempt {})...",
                        delay,
                        attempt + 1
                    ));
                    sleep(Duration::from_secs(delay)).await;
                    continue;
                } else {
                    return Err(anyhow!(
                        "Key download failed, status code: {}",
                        response.status()
                    ));
                }
            }
            Err(e) => {
                if attempt < MAX_RETRIES - 1 {
                    let delay = 2u64.pow(attempt as u32);
                    send_log(format!(
                        "⚠️ Warning: Connection error while downloading key: {}. Retrying in {} seconds (Attempt {})...",
                        e,
                        delay,
                        attempt + 1
                    ));
                    sleep(Duration::from_secs(delay)).await;
                    continue;
                } else {
                    return Err(anyhow!("Key download failed, connection error: {}", e));
                }
            }
        }
    }
    unreachable!()
}
