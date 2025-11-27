# HLS Stream Downloader

## Overview

hls_downloader is a lightweight application developed using the $\text{Rust}$ language and the $\text{egui}$ Graphical User Interface ($\text{GUI}$) framework. It is specifically designed for downloading $\text{HTTP Live Streaming (HLS)}$ streams (.m3u8 playlists). The application provides an intuitive interface and supports multi-threaded concurrent downloading, ensuring efficient and reliable video acquisition.

## Key Features

- Intuitive GUI: Utilizes the $\text{egui}$ framework to provide a single-window operational interface.
- HLS Download: Accepts a .m3u8 link and downloads all segmented files.
- Concurrency Control: Users can set the maximum number of concurrent downloads to optimize speed and resource usage (default range 1-16).
- Output Settings: Customizable output filename and path.
- Format Selection: Supports merging the final video file into several common formats (e.g., mp4, mkv, webm, ts).
- Real-time Progress: Displays the download progress bar and percentage.
- Log Output: Provides a scrollable log area to display key information and errors during the download process in real-time.
- Open Folder: After setting the download path, users can directly click a button to open the target directory.

## Building and Running

This project relies on the $\text{Rust}$ compilation environment.

### Running the Project

In the root directory of the project, execute the following command:

```sh
cargo run
```


### Building Executable

If you wish to build a standalone executable file, you can run:

```sh
cargo build --release
# The executable will be found in target/release/hls_downloader (or .exe)
```



## Usage Guide

After launching the application, you will see a single window containing the following controls:

1. **M3U8 URL**: Enter the complete URL of the HLS stream's master playlist (usually ending in .m3u8).
2. **Output Filename**: Set the name for the final merged video file (without the extension).
3. **Output Location**: Set the directory where the final video file will be saved.
    - You can use the `Browse`... button to open the native file dialog to select a folder.
    - When the path is not empty, you can use the `Open Folder` button to quickly open the target folder.
4. Concurrent Downloads: Adjust the number of concurrent threads used for downloading video segments.
5. **Format**: Select the output format for the final video file (e.g., mp4).
6. 🚀 **Start Download**: Click this button to begin the download process.
7. **Progress Bar**: Displays the overall download progress.
8. **Log Output**: Displays detailed logs of the download, decryption, and merging processes.
