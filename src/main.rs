use anyhow::Result;
use eframe::{App, Frame, NativeOptions, egui, run_native};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

mod downloader;
use downloader::{DEFAULT_CONCURRENT_DOWNLOADS, DownloadMessage, run_hls_download_core};

// ------------------------------------------------------------------------
// 0. Egui Application Structure (App)
// ------------------------------------------------------------------------

struct HlsDownloaderApp {
    // Input fields
    m3u8_url: String,
    output_filename: String,
    output_location: String,
    concurrent_downloads: u8,
    output_format: String, // Output format field

    // Interface state
    is_downloading: bool,
    progress: f32, // 0.0 to 1.0
    logs: Vec<String>,

    // Toki Runtime and Channel (MPSC)
    runtime: Arc<Runtime>,
    // Persistent Sender for GUI commands (like file dialog response)
    sender: mpsc::Sender<DownloadMessage>,
    // Persistent Receiver for GUI commands (Polled by update)
    gui_receiver: mpsc::Receiver<DownloadMessage>,
    // Temporary receiver for the active download task (recreated on each start)
    download_receiver: Option<mpsc::Receiver<DownloadMessage>>,
}

impl Default for HlsDownloaderApp {
    fn default() -> Self {
        let runtime = Arc::new(Runtime::new().expect("Failed to create tokio runtime"));
        // 創建一個常駐的通道，用於處理 UI 相關的非下載任務（例如檔案對話框）
        let (sender, gui_receiver) = mpsc::channel(10);

        Self {
            m3u8_url: "".to_string(),
            output_filename: "".to_string(),
            output_location: "".to_string(),
            concurrent_downloads: DEFAULT_CONCURRENT_DOWNLOADS as u8,
            output_format: "mp4".to_string(),

            is_downloading: false,
            progress: 0.0,
            logs: vec!["Application started.".to_string()],

            runtime,
            sender,                  // 常駐 Sender
            gui_receiver,            // 常駐 Receiver
            download_receiver: None, // 暫時的下載 Receiver
        }
    }
}

impl App for HlsDownloaderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        // --- Process messages from background (channel polling) ---

        // 1. Poll the PERSISTENT GUI Receiver (處理檔案選擇結果)
        while let Ok(msg) = self.gui_receiver.try_recv() {
            if let DownloadMessage::OutputPathSelected(path) = msg {
                self.output_location = path;
                ctx.request_repaint();
            }
        }

        // 2. Poll the TEMPORARY Download Receiver (處理下載進度、日誌和結束)
        if let Some(receiver) = self.download_receiver.as_mut() {
            let mut finished = false;
            let mut message_count = 0; // 訊息計數器

            // The Egui thread must use try_recv(), it cannot block.
            while let Ok(msg) = receiver.try_recv() {
                match msg {
                    DownloadMessage::Log(s) => self.logs.push(s),
                    DownloadMessage::Progress(p) => self.progress = p,
                    DownloadMessage::Finished(res) => {
                        self.is_downloading = false;
                        finished = true; // Set the flag

                        match res {
                            Ok(_) => self
                                .logs
                                .push("✅ Download task completed successfully!".to_string()),
                            Err(e) => self.logs.push(format!("❌ Task failed: {}", e)),
                        }
                    }
                    // ⚠️ 注意: OutputPathSelected 已經被 persistent gui_receiver 處理，這裡不需要。
                    DownloadMessage::OutputPathSelected(_) => { /* Ignore, handled by gui_receiver */
                    }
                }

                // Request repaint to update the interface
                ctx.request_repaint();

                // 讓出控制權的邏輯 (解決 Hyprland 假死問題)
                message_count += 1;
                if message_count >= 10 {
                    // 處理 10 條訊息後
                    std::thread::sleep(Duration::from_millis(1));
                    message_count = 0; // 重置計數
                }
            }

            // Handle outside the mutable borrow scope
            if finished {
                // 使用新的欄位名稱
                self.download_receiver = None;
            }
        }
        // ---------------------------------------

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("HLS Stream Downloader");
            ui.separator();

            // 1. Input Block
            ui.add_enabled_ui(!self.is_downloading, |ui| {
                // 使用 Grid 確保標籤和輸入框垂直對齊
                egui::Grid::new("input_grid")
                    .num_columns(2) // 兩欄: 標籤 和 Widget
                    .spacing([20.0, 10.0]) // [水平間距, 垂直間距]
                    .striped(true) // 可選：增加條紋背景以提高可讀性
                    .show(ui, |ui| {
                        // --- M3U8 URL ---
                        ui.label("M3U8 URL:"); // 第一欄: 標籤
                        ui.text_edit_singleline(&mut self.m3u8_url); // 第二欄: 輸入框
                        ui.end_row();

                        // --- Output Filename (標籤與輸入框平行) ---
                        ui.label("Output Filename:"); // 第一欄: 標籤
                        ui.text_edit_singleline(&mut self.output_filename);
                        ui.end_row();

                        ui.label("Output Location:"); // 第一欄: 標籤
                        ui.horizontal(|ui| {
                            // 第二欄: 輸入框 + 按鈕
                            ui.add(egui::TextEdit::singleline(&mut self.output_location));

                            // 新增 "Browse" 按鈕和 rfd 邏輯
                            if ui.button("Browse...").clicked() {
                                let current_location = self.output_location.clone();
                                // 使用 self.sender (現在已在結構體中定義)
                                let sender_clone = self.sender.clone();

                                // 由於 rfd::FileDialog::save_file() 是阻塞的，必須在 blocking thread 中運行
                                self.runtime.handle().clone().spawn_blocking(move || {
                                    if let Some(path) = rfd::FileDialog::new()
                                        .set_directory(&current_location)
                                        .pick_folder()
                                    {
                                        let full_path = path.to_string_lossy().into_owned();
                                        // 使用 blocking_send 傳回結果給 GUI
                                        let _ = sender_clone.blocking_send(
                                            DownloadMessage::OutputPathSelected(full_path),
                                        );
                                    }
                                });
                            }
                        });
                        ui.end_row();

                        // --- Concurrent Downloads & Output Format (放在同一行，但屬於 Grid 的單元格) ---
                        // 這裡我們需要將兩個控制項擠入 Grid 的第二個單元格
                        ui.label("Concurrent Downloads / Format:"); // 佔用第一欄的標籤

                        ui.horizontal(|ui| {
                            // 1. Concurrent Downloads
                            ui.add(
                                egui::DragValue::new(&mut self.concurrent_downloads)
                                    .speed(1.0)
                                    .clamp_range(1..=16)
                                    .prefix("x "),
                            );

                            ui.separator(); // 視覺分隔符

                            // 2. Output Format (Dropdown)
                            let formats = ["mp4", "mkv", "webm", "ts"];
                            ui.label("Format:"); // 在水平佈局中再次加入標籤

                            egui::ComboBox::from_label("")
                                .selected_text(&self.output_format)
                                .width(70.0)
                                .show_ui(ui, |ui| {
                                    for format in formats {
                                        ui.selectable_value(
                                            &mut self.output_format,
                                            format.to_string(),
                                            format,
                                        );
                                    }
                                });
                        });
                        ui.end_row();
                    });
            });

            // 2. Button and Progress Bar
            ui.add_space(10.0);
            let download_btn =
                ui.add_enabled(!self.is_downloading, egui::Button::new("🚀 Start Download"));

            if download_btn.clicked() {
                // Clear state and start the task
                self.start_download_task(ctx.clone());
            }

            ui.add_space(10.0);
            ui.add(egui::ProgressBar::new(self.progress).show_percentage());

            // 3. Log Output Block
            ui.add_space(15.0);
            ui.label("Log Output:");
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(250.0)
                .show(ui, |ui| {
                    // Display latest logs at the bottom
                    for log in self.logs.iter() {
                        let text = egui::RichText::new(log);
                        // Color based on log content (simplified)
                        let colored_text = if log.starts_with("❌") {
                            text.color(egui::Color32::RED)
                        } else if log.starts_with("✅")
                            || log.starts_with("📦")
                            || log.starts_with("🔑")
                        {
                            text.color(egui::Color32::GREEN)
                        } else if log.starts_with("⚠️") {
                            text.color(egui::Color32::YELLOW)
                        } else {
                            text.color(egui::Color32::WHITE)
                        };
                        ui.label(colored_text);
                    }
                });
        });
    }
}

// ------------------------------------------------------------------------
// 1. Egui/Tokio Startup and Bridging
// ------------------------------------------------------------------------

impl HlsDownloaderApp {
    fn start_download_task(&mut self, ctx: egui::Context) {
        // Parameter check
        let url_str = self.m3u8_url.trim();
        if url_str.is_empty() || url_str.starts_with("Enter M3U8 URL...") {
            self.logs
                .push("⚠️ Please enter a valid M3U8 URL.".to_string());
            return;
        }

        // Set initial state
        self.is_downloading = true;
        self.progress = 0.0;
        self.logs.clear();
        self.logs.push("Preparing to start download...".to_string());

        let url = url_str.to_string();
        let filename = self.output_filename.clone();
        let location = self.output_location.clone();
        let concurrency = self.concurrent_downloads as usize;
        let format = self.output_format.clone();

        // 創建一個新的 MPSC 通道，專門用於這個下載任務的狀態更新
        let (download_sender, download_receiver) = mpsc::channel(100);
        self.download_receiver = Some(download_receiver); // 儲存這個臨時 Receiver

        let runtime_handle = self.runtime.handle().clone();

        // Start the background task, moving all core logic here
        runtime_handle.spawn(async move {
            let result = run_hls_download_core(
                url,
                location,
                filename,
                concurrency,
                format,
                download_sender.clone(), // 使用下載專用的 Sender
                ctx.clone(),
            )
            .await;

            // Send the final finished message regardless of success or failure
            let final_message = match result {
                Ok(_) => DownloadMessage::Finished(Ok(())),
                Err(e) => DownloadMessage::Finished(Err(e.to_string())),
            };
            // 使用下載專用的 Sender
            download_sender.send(final_message).await.ok();
            ctx.request_repaint();
        });
    }
}

// ------------------------------------------------------------------------
// 3. Eframe Main Entry (with Font Setup)
// ------------------------------------------------------------------------

// 1. 在編譯時嵌入字體文件
// 確保 'NotoSansCJKtc-Regular.otf' 檔案存在於專案根目錄或指定的相對路徑
const CJK_FONT_DATA: &[u8] = include_bytes!("./assets/fonts/NotoSansCJKtc-Regular.otf");

fn main() -> Result<(), eframe::Error> {
    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([500.0, 650.0]),
        ..Default::default()
    };

    run_native(
        "HLS Downloader",
        options,
        // Egui initialization for font setup
        Box::new(|cc| {
            // --- CJK Font Embedding Setup ---

            let mut fonts = egui::FontDefinitions::default();

            // 2. 從嵌入的位元組資料 (static &[u8]) 創建 FontData
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_static(CJK_FONT_DATA));

            // 3. 優先使用 'cjk' 字體作為所有文字的預設字體
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());

            cc.egui_ctx.set_fonts(fonts);

            // -----------------------------

            // Return the App instance
            Box::<HlsDownloaderApp>::default()
        }),
    )
}
