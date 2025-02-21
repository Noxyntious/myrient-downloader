use eframe::egui;
use futures_util::StreamExt;
use myrient_filter::{FilterOptions, Rom, RomLister};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;

enum AppUpdate {
    SystemsLoaded(Vec<String>),
    RomsLoaded(Vec<Rom>),
    SetStatus(String),
    SetLoading(bool),
    DownloadProgress(String, f32),
}

struct MyrientApp {
    filter_options: FilterOptions,
    selected_system: String,
    available_systems: Vec<String>,
    download_path: String,
    status_message: String,
    roms: Vec<Rom>,
    loading: bool,
    rx: std::sync::mpsc::Receiver<AppUpdate>,
    tx: std::sync::mpsc::Sender<AppUpdate>,
    download_progress: HashMap<String, f32>,
    has_active_downloads: bool,
    show_filter_options: bool,
    show_system_selection: bool,
    show_download_settings: bool,
    current_path: Vec<String>,
    needs_directory_load: bool,
    selected_roms: HashSet<String>,
    search_term: String,
    max_concurrent_downloads: usize,
    download_queue: Vec<Rom>,
    active_downloads: usize,
    show_queue_window: bool,
}

async fn download_rom(
    rom: &Rom,
    download_path: &Path,
    tx: std::sync::mpsc::Sender<AppUpdate>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = tx.send(AppUpdate::SetStatus(format!(
        "Starting download of {}",
        rom.filename
    )));

    // Create client and get response
    let client = reqwest::Client::new();
    let response = client.get(&rom.url).send().await?;

    // Check content type for text response
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("text") {
        let text = response.text().await?;
        let _ = tx.send(AppUpdate::SetStatus(format!(
            "Error: Received text response for {}, check console",
            rom.filename
        )));
        println!("{}", text);
        return Err("Received text response instead of file".into());
    }

    // Get file size
    let total_size = response.content_length().unwrap_or(0);
    if total_size == 0 {
        return Err("Unknown file size".into());
    }

    // Create download directory and file
    tokio::fs::create_dir_all(download_path).await?;
    let dest = download_path.join(&rom.filename);
    let mut file = tokio::fs::File::create(&dest).await?;

    // Download with progress updates
    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        file.write_all(chunk.as_ref()).await?;
        downloaded += chunk.len() as u64;

        // Update progress
        let progress = downloaded as f32 / total_size as f32;
        println!(
            "Download progress for {}: {:.2}%",
            rom.filename,
            progress * 100.0
        );
        let _ = tx.send(AppUpdate::DownloadProgress(rom.filename.clone(), progress));
    }

    // Handle zip extraction if needed
    // This probably doesn't work on Windows, but we don't like Windows anyways
    if dest.extension().unwrap_or_default() == "zip" {
        let _ = tx.send(AppUpdate::SetStatus(format!("Extracting {}", rom.filename)));

        let dest_dir = dest.with_extension("");

        let status = std::process::Command::new("unzip")
            .arg("-o") // overwrite files without prompting
            .arg(&dest)
            .arg("-d")
            .arg(&dest_dir)
            .status()?;

        if !status.success() {
            let _ = tx.send(AppUpdate::SetStatus(format!(
                "Failed to extract {}",
                rom.filename
            )));
            return Err("Failed to unzip file".into());
        }

        // Clean up zip file
        tokio::fs::remove_file(&dest).await?;

        // Move extracted files to final location
        let filename_stripped = dest.file_stem().unwrap().to_str().unwrap();
        let resulting_folder = download_path.join(filename_stripped);

        if resulting_folder.exists() {
            let mut entries = tokio::fs::read_dir(&resulting_folder).await?;
            while let Some(entry) = entries.next_entry().await? {
                let entry_path = entry.path();
                let entry_filename = entry_path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let new_path = download_path.join(&entry_filename);

                let _ = tx.send(AppUpdate::SetStatus(format!(
                    "Moving extracted file: {}",
                    entry_filename
                )));

                tokio::fs::copy(&entry_path, &new_path).await?;
            }

            // Clean up extraction directory
            tokio::fs::remove_dir_all(&dest_dir).await?;
        }
    }

    // Signal completion
    let _ = tx.send(AppUpdate::DownloadProgress(rom.filename.clone(), -1.0));
    let _ = tx.send(AppUpdate::SetStatus(format!(
        "Successfully downloaded and processed {}",
        rom.filename
    )));

    Ok(())
}

impl Default for MyrientApp {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            filter_options: FilterOptions {
                region_limit: true,
                region: "USA".to_string(),
                smart_filters: true,
                exclude_patterns: Vec::new(),
                latest_revision: true,
            },
            selected_system: String::new(),
            available_systems: Vec::new(),
            download_path: "downloads".to_string(),
            status_message: String::new(),
            roms: Vec::new(),
            loading: false,
            rx,
            tx,
            download_progress: HashMap::new(),
            has_active_downloads: false,
            show_filter_options: false,
            show_system_selection: false,
            show_download_settings: false,
            current_path: Vec::new(),
            needs_directory_load: true,
            selected_roms: HashSet::new(),
            search_term: String::new(),
            max_concurrent_downloads: 8,
            download_queue: Vec::new(),
            active_downloads: 0,
            show_queue_window: false,
        }
    }
}

impl MyrientApp {
    fn start_download(tx: std::sync::mpsc::Sender<AppUpdate>, rom: Rom, download_path: PathBuf) {
        let _ = tx.send(AppUpdate::DownloadProgress(rom.filename.clone(), 0.0));

        std::thread::spawn(move || {
            let rt = Runtime::new().unwrap();
            rt.block_on(async {
                if let Err(e) = download_rom(&rom, &download_path, tx.clone()).await {
                    let _ = tx.send(AppUpdate::SetStatus(format!(
                        "Failed to download {}: {}",
                        rom.filename, e
                    )));
                    let _ = tx.send(AppUpdate::DownloadProgress(rom.filename.clone(), -1.0));
                }
            });
        });
    }
}

impl eframe::App for MyrientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.rx.try_recv() {
            match update {
                AppUpdate::SystemsLoaded(systems) => self.available_systems = systems,
                AppUpdate::RomsLoaded(roms) => self.roms = roms,
                AppUpdate::SetStatus(msg) => self.status_message = msg,
                AppUpdate::SetLoading(loading) => self.loading = loading,
                AppUpdate::DownloadProgress(filename, progress) => {
                    if progress < 0.0 {
                        self.download_progress.remove(&filename);
                        if self.active_downloads > 0 {
                            self.active_downloads -= 1;
                        }

                        // Start next download from queue if any
                        if !self.download_queue.is_empty() && !self.loading {
                            let next_rom = self.download_queue.remove(0);
                            self.download_progress
                                .insert(next_rom.filename.clone(), 0.0);
                            MyrientApp::start_download(
                                self.tx.clone(),
                                next_rom,
                                PathBuf::from(&self.download_path),
                            );
                        }

                        if self.download_progress.is_empty() {
                            self.has_active_downloads = false;
                        }
                    } else {
                        self.download_progress.insert(filename, progress);
                        self.has_active_downloads = true;
                    }
                }
            }
            ctx.request_repaint();
        }
        if self.has_active_downloads {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Myrient ROM Downloader");

            ui.horizontal(|ui| {
                // Filter options
                ui.vertical(|ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.heading("Filter Options");
                            if ui
                                .button(if self.show_filter_options { "v" } else { ">" })
                                .clicked()
                            {
                                self.show_filter_options = !self.show_filter_options;
                            }
                        });
                        if self.show_filter_options {
                            ui.checkbox(&mut self.filter_options.region_limit, "Region Limit");
                            ui.horizontal(|ui| {
                                ui.label("Region:");
                                ui.text_edit_singleline(&mut self.filter_options.region);
                            });
                            ui.checkbox(&mut self.filter_options.smart_filters, "Smart Filters");
                            ui.checkbox(
                                &mut self.filter_options.latest_revision,
                                "Latest Revision Only",
                            );

                            ui.label("Exclude Patterns:");
                            let mut patterns_to_remove = Vec::new();
                            for (i, pattern) in
                                self.filter_options.exclude_patterns.iter_mut().enumerate()
                            {
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(pattern);
                                    if ui.button("❌").clicked() {
                                        patterns_to_remove.push(i);
                                    }
                                });
                            }
                            for i in patterns_to_remove.iter().rev() {
                                self.filter_options.exclude_patterns.remove(*i);
                            }
                            ui.horizontal(|ui| {
                                let mut selected_pattern = String::new();
                                egui::ComboBox::from_label("Add Predefined Pattern")
                                    .selected_text(&selected_pattern)
                                    .show_ui(ui, |ui| {
                                        let patterns = [
                                            "Pirate",
                                            "Beta",
                                            "Proto",
                                            "Enhancement Chip",
                                            "Tech Demo",
                                            "Competition Cart",
                                            "Sample",
                                            "Aftermarket",
                                            "Demo",
                                            "Unl",
                                        ];
                                        for pattern in patterns {
                                            if ui
                                                .selectable_value(
                                                    &mut selected_pattern,
                                                    pattern.to_string(),
                                                    pattern,
                                                )
                                                .clicked()
                                            {
                                                self.filter_options
                                                    .exclude_patterns
                                                    .push(pattern.to_string());
                                            }
                                        }
                                    });
                                if ui.button("Add Custom Pattern").clicked() {
                                    self.filter_options.exclude_patterns.push(String::new());
                                }
                            });
                        }
                    });
                });

                // System selection
                ui.vertical(|ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.heading("System Selection");
                            if ui
                                .button(if self.show_system_selection { "v" } else { ">" })
                                .clicked()
                            {
                                self.show_system_selection = !self.show_system_selection;
                            }
                        });

                        if self.show_system_selection {
                            // Show current path
                            ui.horizontal(|ui| {
                                if ui.button("📁 Root").clicked() {
                                    self.current_path.clear();
                                    self.available_systems.clear(); // Clear systems when returning to root
                                    self.selected_system = String::new(); // Clear selection
                                    self.needs_directory_load = true; // Set flag to load root directory
                                    self.roms.clear();
                                }

                                // Show path breadcrumbs
                                // That's the > thing between directories btw
                                let current_path = self.current_path.clone();
                                for (i, dir) in current_path.iter().enumerate() {
                                    ui.label(">");
                                    if ui.button(dir).clicked() {
                                        self.current_path.truncate(i + 1);
                                        self.available_systems.clear();
                                        self.needs_directory_load = true;
                                        self.roms.clear();
                                    }
                                }
                            });

                            // Load directory contents if empty
                            if self.needs_directory_load && !self.loading {
                                self.needs_directory_load = false; // Reset flag immediately
                                let tx = self.tx.clone();
                                let filter_options = self.filter_options.clone();
                                let current_path = if self.current_path.is_empty() {
                                    String::new()
                                } else {
                                    self.current_path.join("/")
                                };

                                tx.send(AppUpdate::SetLoading(true)).unwrap();
                                std::thread::spawn(move || {
                                    let rt = Runtime::new().unwrap();
                                    rt.block_on(async {
                                        let lister = RomLister::new(filter_options);
                                        match lister.list_directories(Some(&current_path)).await {
                                            Ok(dirs) => {
                                                tx.send(AppUpdate::SystemsLoaded(dirs)).unwrap();
                                                tx.send(AppUpdate::SetStatus(
                                                    "Directories loaded successfully".to_string(),
                                                ))
                                                .unwrap();
                                            }
                                            Err(e) => {
                                                tx.send(AppUpdate::SetStatus(format!(
                                                    "Error loading directories: {}",
                                                    e
                                                )))
                                                .unwrap();
                                            }
                                        }
                                        tx.send(AppUpdate::SetLoading(false)).unwrap();
                                    });
                                });
                            }

                            // Display directories
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let systems = self.available_systems.clone();
                                for system in &systems {
                                    ui.horizontal(|ui| {
                                        if ui.button(format!("📁 {}", system)).clicked() {
                                            self.current_path.push(system.clone());
                                            self.available_systems.clear();
                                            self.selected_system = system.clone(); // Set selected system when entering it
                                            self.needs_directory_load = true;

                                            // Load ROMs when entering a system directory
                                            let tx = self.tx.clone();
                                            let system = system.clone();
                                            let filter_options = self.filter_options.clone();

                                            let full_path = self.current_path
                                                [..self.current_path.len() - 1]
                                                .join("/");
                                            let path_clone = full_path.clone();

                                            tx.send(AppUpdate::SetLoading(true)).unwrap();
                                            std::thread::spawn(move || {
                                                let rt = Runtime::new().unwrap();
                                                rt.block_on(async {
                                                    let lister = RomLister::new(filter_options);
                                                    match lister
                                                        .list_roms(&system, &path_clone)
                                                        .await
                                                    {
                                                        Ok(roms) => {
                                                            tx.send(AppUpdate::RomsLoaded(roms))
                                                                .unwrap();
                                                            tx.send(AppUpdate::SetStatus(
                                                                "ROMs loaded successfully"
                                                                    .to_string(),
                                                            ))
                                                            .unwrap();
                                                        }
                                                        Err(e) => {
                                                            tx.send(AppUpdate::SetStatus(format!(
                                                                "Error loading ROMs: {}",
                                                                e
                                                            )))
                                                            .unwrap();
                                                        }
                                                    }
                                                    tx.send(AppUpdate::SetLoading(false)).unwrap();
                                                });
                                            });
                                        }
                                    });
                                }
                            });
                        }
                    });
                });

                // Download settings
                ui.vertical(|ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.heading("Download Settings");
                            if ui
                                .button(if self.show_download_settings {
                                    "v"
                                } else {
                                    ">"
                                })
                                .clicked()
                            {
                                self.show_download_settings = !self.show_download_settings;
                            }
                        });
                        if self.show_download_settings {
                            ui.horizontal(|ui| {
                                ui.label("Download Path:");
                                ui.text_edit_singleline(&mut self.download_path);
                                if ui.button("Browse").clicked() {
                                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                        self.download_path = path.display().to_string();
                                    }
                                }
                            });
                        }
                    });
                });
            });

            // Display ROM list
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.heading("Available ROMs");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Download Selected").clicked() && !self.loading {
                            let selected_roms: Vec<Rom> = self
                                .roms
                                .iter()
                                .filter(|rom| self.selected_roms.contains(&rom.filename))
                                .cloned()
                                .collect();

                            for rom in selected_roms {
                                if self.active_downloads < self.max_concurrent_downloads {
                                    self.active_downloads += 1;
                                    self.download_progress.insert(rom.filename.clone(), 0.0);
                                    MyrientApp::start_download(
                                        self.tx.clone(),
                                        rom,
                                        PathBuf::from(&self.download_path),
                                    );
                                } else {
                                    self.download_queue.push(rom.clone());
                                    self.tx
                                        .send(AppUpdate::SetStatus(format!(
                                            "Queued {} for download",
                                            rom.filename
                                        )))
                                        .unwrap();
                                }
                            }
                        }
                        ui.add_space(8.0);
                        if ui.button("Deselect All").clicked() {
                            self.selected_roms.clear();
                        }
                        ui.add_space(8.0);
                        if ui.button("Select All").clicked() {
                            self.selected_roms =
                                self.roms.iter().map(|rom| rom.filename.clone()).collect();
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Search:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search_term)
                            .desired_width(ui.available_width()),
                    );
                });
                let available_height = ui.available_height() - 20.0;
                egui::ScrollArea::vertical()
                    .max_height(available_height)
                    .show(ui, |ui| {
                        // Filter ROMs based on search term
                        let filtered_roms: Vec<&Rom> = self
                            .roms
                            .iter()
                            .filter(|rom| {
                                if self.search_term.is_empty() {
                                    true
                                } else {
                                    rom.filename
                                        .to_lowercase()
                                        .contains(&self.search_term.to_lowercase())
                                }
                            })
                            .collect();
                        let filtered_count = filtered_roms.len();
                        let total_count = self.roms.len();

                        // Display filtered ROMs
                        for rom in filtered_roms {
                            ui.horizontal(|ui| {
                                let mut is_selected = self.selected_roms.contains(&rom.filename);
                                if ui.checkbox(&mut is_selected, "").changed() {
                                    if is_selected {
                                        self.selected_roms.insert(rom.filename.clone());
                                    } else {
                                        self.selected_roms.remove(&rom.filename);
                                    }
                                }
                                ui.label(&rom.filename);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if let Some(&progress) =
                                            self.download_progress.get(&rom.filename)
                                        {
                                            ui.add(
                                                egui::ProgressBar::new(progress)
                                                    .show_percentage()
                                                    .desired_width(100.0),
                                            );
                                        } else if ui.button("Download").clicked() && !self.loading {
                                            if self.active_downloads < self.max_concurrent_downloads
                                            {
                                                self.active_downloads += 1;
                                                self.download_progress
                                                    .insert(rom.filename.clone(), 0.0);
                                                MyrientApp::start_download(
                                                    self.tx.clone(),
                                                    rom.clone(),
                                                    PathBuf::from(&self.download_path),
                                                );
                                            } else {
                                                self.download_queue.push(rom.clone());
                                                self.tx
                                                    .send(AppUpdate::SetStatus(format!(
                                                        "Queued {} for download",
                                                        rom.filename
                                                    )))
                                                    .unwrap();
                                            }
                                        }
                                    },
                                );
                            });
                        }

                        if !self.search_term.is_empty() {
                            ui.separator();
                            ui.label(format!("Showing {}/{} ROMs", filtered_count, total_count));
                        }
                    });
            });
        });
        if self.show_queue_window {
            egui::Window::new("Download Queue")
                .open(&mut self.show_queue_window)
                .resizable(true)
                .default_size([400.0, 300.0])
                .show(ctx, |ui| {
                    ui.heading("Active Downloads");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("active_downloads_scroll") // do this so it doesnt do Da Complainy
                        .show(ui, |ui| {
                            for (filename, progress) in &self.download_progress {
                                ui.horizontal(|ui| {
                                    ui.label(filename);
                                    ui.add(
                                        egui::ProgressBar::new(*progress)
                                            .show_percentage()
                                            .desired_width(100.0),
                                    );
                                });
                            }
                        });

                    ui.heading(format!("Queued Downloads ({})", self.download_queue.len()));
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("queued_downloads_scroll")
                        .show(ui, |ui| {
                            let mut remove_indices = Vec::new();
                            for (i, rom) in self.download_queue.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(&rom.filename);
                                    if ui.button("❌").clicked() {
                                        remove_indices.push(i);
                                    }
                                });
                            }
                            // Remove cancelled downloads
                            for &i in remove_indices.iter().rev() {
                                self.download_queue.remove(i);
                            }
                        });

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Active Downloads: {}/{}",
                            self.active_downloads, self.max_concurrent_downloads
                        ));

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Clear Queue").clicked() {
                                self.download_queue.clear();
                                let _ = self.tx.send(AppUpdate::SetStatus(
                                    "Cleared download queue".to_string(),
                                ));
                            }
                        });
                    });
                });
        }
        // Show bottom status bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(format!(
                        "Myrient ROM Downloader {} | myrient-filter {}",
                        env!("CARGO_PKG_VERSION"),
                        "0.1.0" // this is just a place holder until i figure out a better solution
                    ));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if ui.button("📋 Queue").clicked() {
                            self.show_queue_window = !self.show_queue_window;
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.loading {
                        ui.spinner();
                    }
                    if !self.status_message.is_empty() {
                        ui.label(&self.status_message);
                    }
                });
            });
        });
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        vsync: true,
        multisampling: 4,
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Myrient ROM Downloader",
        options,
        Box::new(|_cc| Ok(Box::new(MyrientApp::default()))),
    )
}
