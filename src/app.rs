use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, ScrollArea, Vec2};

use crate::deploy::{
    build_dry_run_plan, start_deployment, DeployMode, DeployOptions, DeployState, LogLevel,
    UtilityAction,
};
use crate::disk::{enumerate_disks, format_bytes_human, DiskInfo, PartitionScheme};
use crate::elevate::{is_elevated, relaunch_as_admin};
use crate::image::{
    inspect_image_source, pick_folder, pick_image_file, ImageInspectionResult, SourceKind,
};
use crate::vss::{query_live_os_info, LiveOsInfo};

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub theme: String, // "system", "dark", "light"
    pub last_source_path: String,
    pub default_volume_label: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            last_source_path: String::new(),
            default_volume_label: "WinToUSB".to_string(),
        }
    }
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = PathBuf::from(appdata).join("WinToUSB");
            let _ = fs::create_dir_all(&p);
            p.join("config.json")
        } else {
            PathBuf::from("config.json")
        }
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(content) = fs::read_to_string(path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, content);
        }
    }
}

pub struct WinToUsbApp {
    config: AppConfig,
    system_visuals: egui::Visuals,
    is_admin: bool,

    // Mode & Options
    options: DeployOptions,
    disks: Vec<DiskInfo>,
    disk_error: Option<String>,
    live_os: Option<LiveOsInfo>,
    live_os_error: Option<String>,

    // Image Inspection Async
    inspecting_image: bool,
    inspection_result: Option<ImageInspectionResult>,
    inspection_error: Option<String>,
    inspection_rx: Option<Receiver<Result<ImageInspectionResult, String>>>,

    // Confirmation Safety State
    confirm_disk_text: String,
    confirm_wipe_checked: bool,

    // Deployment Worker State
    deploy_state: Arc<Mutex<DeployState>>,

    // UI Status & Modals
    status_text: String,
    show_guide: bool,
}

impl WinToUsbApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config = AppConfig::load();
        let system_visuals = cc.egui_ctx.style().visuals.clone();

        match config.theme.as_str() {
            "dark" => cc.egui_ctx.set_visuals(egui::Visuals::dark()),
            "light" => cc.egui_ctx.set_visuals(egui::Visuals::light()),
            _ => cc.egui_ctx.set_visuals(system_visuals.clone()),
        }

        let is_admin = is_elevated();
        let mut options = DeployOptions::default();
        options.source_path = config.last_source_path.clone();
        options.volume_label = config.default_volume_label.clone();

        let (disks, disk_error) = match enumerate_disks() {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        };

        // Pre-select first safe USB target if present
        if let Some(usb) = disks.iter().find(|d| d.is_safe_usb_target()) {
            options.target_disk_number = usb.number;
        } else if let Some(first) = disks.iter().find(|d| d.number != 0 && !d.is_system && !d.is_boot) {
            options.target_disk_number = first.number;
        }

        let (live_os, live_os_error) = match query_live_os_info() {
            Ok(info) => (Some(info), None),
            Err(e) => (None, Some(e)),
        };

        let status_text = format!(
            "WinToUSB v1.0.0 siap. {} | {} disk fisik terdeteksi.",
            if is_admin {
                "Hak Administrator Aktif"
            } else {
                "Mode Standar (Elevasi UAC diperlukan untuk menulis disk)"
            },
            disks.len()
        );

        Self {
            config,
            system_visuals,
            is_admin,
            options,
            disks,
            disk_error,
            live_os,
            live_os_error,
            inspecting_image: false,
            inspection_result: None,
            inspection_error: None,
            inspection_rx: None,
            confirm_disk_text: String::new(),
            confirm_wipe_checked: false,
            deploy_state: Arc::new(Mutex::new(DeployState::default())),
            status_text,
            show_guide: false,
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        match self.config.theme.as_str() {
            "dark" => ctx.set_visuals(egui::Visuals::dark()),
            "light" => ctx.set_visuals(egui::Visuals::light()),
            _ => ctx.set_visuals(self.system_visuals.clone()),
        }
    }

    fn refresh_disks(&mut self) {
        match enumerate_disks() {
            Ok(list) => {
                self.disks = list;
                self.disk_error = None;
                // Keep current selection if still valid, else pick first safe USB
                if !self
                    .disks
                    .iter()
                    .any(|d| d.number == self.options.target_disk_number)
                {
                    if let Some(usb) = self.disks.iter().find(|d| d.is_safe_usb_target()) {
                        self.options.target_disk_number = usb.number;
                    } else {
                        self.options.target_disk_number = 0;
                    }
                    self.confirm_disk_text.clear();
                    self.confirm_wipe_checked = false;
                }
                self.status_text =
                    format!("Daftar disk diperbarui ({} disk terdeteksi).", self.disks.len());
            }
            Err(e) => {
                self.disk_error = Some(e.clone());
                self.status_text = format!("Gagal membaca daftar disk: {}", e);
            }
        }
    }

    fn trigger_image_inspection(&mut self, ctx: &egui::Context) {
        let path = self.options.source_path.trim().to_string();
        if path.is_empty() || self.inspecting_image {
            return;
        }
        self.inspecting_image = true;
        self.inspection_error = None;
        self.inspection_result = None;
        self.status_text = "Menganalisis image sumber melalui DISM...".to_string();

        let (tx, rx) = channel();
        self.inspection_rx = Some(rx);
        let ctx_clone = ctx.clone();

        std::thread::spawn(move || {
            let res = inspect_image_source(&path);
            let _ = tx.send(res);
            ctx_clone.request_repaint();
        });
    }

    fn poll_image_inspection(&mut self) {
        if let Some(ref rx) = self.inspection_rx {
            if let Ok(res) = rx.try_recv() {
                self.inspecting_image = false;
                self.inspection_rx = None;
                match res {
                    Ok(info) => {
                        self.options.is_split_swm = info.is_split_swm;
                        if let Some(first) = info.editions.first() {
                            self.options.wim_index = first.index;
                            if first.is_windows_11 {
                                self.options.bypass_win11_checks = true;
                            }
                        }
                        self.status_text = format!(
                            "Analisis selesai: {} edisi Windows ditemukan.",
                            info.editions.len()
                        );
                        self.inspection_result = Some(info);
                    }
                    Err(e) => {
                        self.status_text = format!("Analisis image gagal: {}", e);
                        self.inspection_error = Some(e);
                    }
                }
            }
        }
    }

    fn selected_disk(&self) -> Option<&DiskInfo> {
        self.disks
            .iter()
            .find(|d| d.number == self.options.target_disk_number)
    }

    fn needs_destructive_confirmation(&self) -> bool {
        match self.options.mode {
            DeployMode::WindowsToGo | DeployMode::LiveOsClone => {
                self.options.scheme.is_destructive()
            }
            DeployMode::WindowsInstallerUsb => true,
            DeployMode::DiskVhdUtility => {
                self.options.utility_action != UtilityAction::CreateStandaloneVhdx
            }
        }
    }

    fn available_drive_letters_on_selected_disk(&self) -> Vec<char> {
        let mut letters = Vec::new();
        if let Some(d) = self.selected_disk() {
            for p in &d.partitions {
                if let Some(ch) = p.drive_letter {
                    if ('E'..='Z').contains(&ch) && !letters.contains(&ch) {
                        letters.push(ch);
                    }
                }
            }
        }
        letters
    }
}

impl eframe::App for WinToUsbApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx);
        self.poll_image_inspection();

        let is_dark = ctx.style().visuals.dark_mode;
        let is_running = self
            .deploy_state
            .lock()
            .map(|s| s.running)
            .unwrap_or(false);

        if is_running || self.inspecting_image {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Semantic adaptive colors
        let brand_color = if is_dark {
            Color32::from_rgb(70, 195, 255)
        } else {
            Color32::from_rgb(0, 105, 195)
        };
        let ok_color = if is_dark {
            Color32::from_rgb(80, 220, 110)
        } else {
            Color32::from_rgb(0, 135, 40)
        };
        let warn_color = if is_dark {
            Color32::from_rgb(255, 200, 60)
        } else {
            Color32::from_rgb(175, 105, 0)
        };
        let err_color = if is_dark {
            Color32::from_rgb(255, 95, 95)
        } else {
            Color32::from_rgb(195, 25, 25)
        };

        // Top Header Bar
        egui::TopBottomPanel::top("header_panel").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("💾 WinToUSB").size(20.0).strong().color(brand_color));
                ui.label(
                    RichText::new("v1.0.0 | Windows To Go, Live OS Cloner & Bootable USB Suite")
                        .color(ui.visuals().weak_text_color()),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("ℹ️ Panduan").clicked() {
                        self.show_guide = !self.show_guide;
                    }

                    let theme_btn = match self.config.theme.as_str() {
                        "dark" => "🌙 Gelap",
                        "light" => "☀️ Terang",
                        _ => "🖥️ Sistem",
                    };
                    if ui
                        .button(theme_btn)
                        .on_hover_text("Ganti tema tampilan (Sistem / Gelap / Terang)")
                        .clicked()
                    {
                        self.config.theme = match self.config.theme.as_str() {
                            "system" => "dark".to_string(),
                            "dark" => "light".to_string(),
                            _ => "system".to_string(),
                        };
                        self.config.save();
                        self.apply_theme(ctx);
                    }

                    if ui
                        .add_enabled(!is_running, egui::Button::new("🔄 Segarkan Disk"))
                        .clicked()
                    {
                        self.refresh_disks();
                    }

                    ui.separator();

                    if self.is_admin {
                        ui.label(
                            RichText::new("🛡️ Administrator")
                                .color(ok_color)
                                .strong(),
                        );
                    } else {
                        if ui
                            .button(
                                RichText::new("🛡️ Jalankan sebagai Admin (UAC)")
                                    .color(warn_color)
                                    .strong(),
                            )
                            .on_hover_text(
                                "Hak Administrator diperlukan untuk DiskPart, DISM, VSS, dan BCDBoot.",
                            )
                            .clicked()
                        {
                            match relaunch_as_admin() {
                                Ok(()) => std::process::exit(0),
                                Err(e) => self.status_text = e,
                            }
                        }
                        ui.label(RichText::new("⚠️ Bukan Admin").color(err_color).strong());
                    }
                });
            });

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);

            // Mode Tabs
            ui.horizontal_wrapped(|ui| {
                for mode in [
                    DeployMode::WindowsToGo,
                    DeployMode::LiveOsClone,
                    DeployMode::WindowsInstallerUsb,
                    DeployMode::DiskVhdUtility,
                ] {
                    let selected = self.options.mode == mode;
                    let btn = egui::SelectableLabel::new(
                        selected,
                        RichText::new(mode.title_id()).strong(),
                    );
                    if ui.add_enabled(!is_running, btn).clicked() {
                        self.options.mode = mode;
                        self.confirm_wipe_checked = false;
                    }
                }
            });
            ui.add_space(4.0);
        });

        // Bottom Status Bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&self.status_text)
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new("Mesin: DISM + DiskPart + BCDBoot + VSS")
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
        });

        // Interactive Guide Modal
        if self.show_guide {
            egui::Window::new("ℹ️ Panduan & Arsitektur WinToUSB v1.0.0")
                .collapsible(false)
                .resizable(true)
                .default_size(Vec2::new(580.0, 440.0))
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ScrollArea::vertical().show(ui, |ui| {
                        ui.heading("Panduan Penggunaan WinToUSB");
                        ui.add_space(6.0);

                        ui.group(|ui| {
                            ui.label(RichText::new("1. Windows To Go (ISO / WIM / ESD / VHDX)").strong());
                            ui.label(
                                "• Memasang Windows lengkap ke flashdisk / SSD eksternal sehingga bisa langsung booting sebagai OS portabel di PC mana saja.\n\
                                 • Pilih 'Langsung ke Partisi' untuk performa tercepat, atau 'Mode .VHDX' agar seluruh OS tersimpan rapi dalam 1 berkas virtual disk.",
                            );
                        });
                        ui.add_space(6.0);

                        ui.group(|ui| {
                            ui.label(RichText::new("2. Kloning OS Aktif (Live Hot-Clone via VSS)").strong());
                            ui.label(
                                "• Membuat snapshot Volume Shadow Copy (VSS) dari drive C:\\ saat Windows sedang menyala, tanpa mematikan PC!\n\
                                 • Otomatis mengecualikan pagefile.sys, hiberfil.sys, dan file sementara agar muat di USB eksternal.",
                            );
                        });
                        ui.add_space(6.0);

                        ui.group(|ui| {
                            ui.label(RichText::new("3. Skema Partisi & Dual-Boot").strong());
                            ui.label(
                                "• GPT untuk UEFI: Standar laptop/PC modern (2015+) & Mac.\n\
                                 • MBR untuk BIOS & UEFI (Dual Boot): Partisi FAT32 aktif + NTFS pada MBR sehingga satu USB bisa boot di BIOS lama maupun UEFI modern!",
                            );
                        });
                        ui.add_space(6.0);

                        ui.group(|ui| {
                            ui.label(RichText::new("4. Perlindungan Keamanan Anti-Salah-Format").strong());
                            ui.label(
                                "• Disk 0, Disk Sistem, Disk Boot, dan drive internal SATA/NVMe dikunci secara permanen.\n\
                                 • Gunakan panel 'Preview Rencana Eksekusi (Dry-Run)' sebelum memulai untuk melihat perintah DiskPart/DISM/BCDBoot persis.",
                            );
                        });

                        ui.add_space(10.0);
                        if ui.button("Tutup Panduan").clicked() {
                            self.show_guide = false;
                        }
                    });
                });
        }

        // Right Panel: Live Execution Progress & Logs
        egui::SidePanel::right("progress_log_panel")
            .min_width(340.0)
            .default_width(390.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading(RichText::new("📊 Progres & Log Eksekusi").size(16.0).strong());
                ui.separator();

                let (
                    st_running,
                    st_finished,
                    st_success,
                    st_step,
                    st_total,
                    st_title,
                    st_pct,
                    st_elapsed,
                    st_err,
                    st_logs,
                    cancel_flag,
                ) = {
                    let st = self.deploy_state.lock().unwrap();
                    let elapsed = if st.running {
                        st.started_at.map(|s| s.elapsed().as_secs()).unwrap_or(0)
                    } else {
                        st.finished_elapsed_secs.unwrap_or(0)
                    };
                    (
                        st.running,
                        st.finished,
                        st.success,
                        st.current_step,
                        st.total_steps,
                        st.step_title.clone(),
                        st.progress_pct,
                        elapsed,
                        st.error_message.clone(),
                        st.logs.clone(),
                        st.cancel_flag.clone(),
                    )
                };

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let status_badge = if st_running {
                            RichText::new("⏳ SEDANG BERJALAN").color(brand_color).strong()
                        } else if st_finished && st_success {
                            RichText::new("✅ SELESAI SUKSES").color(ok_color).strong()
                        } else if st_finished {
                            RichText::new("❌ GAGAL / DIBATALKAN").color(err_color).strong()
                        } else {
                            RichText::new("🟢 SIAGA").color(ui.visuals().text_color()).strong()
                        };
                        ui.label(status_badge);

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("{:02}:{:02}", st_elapsed / 60, st_elapsed % 60))
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        });
                    });

                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Langkah {}/{}: {}",
                            st_step, st_total, st_title
                        ))
                        .size(12.5),
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::ProgressBar::new((st_pct / 100.0).clamp(0.0, 1.0))
                            .show_percentage()
                            .animate(st_running),
                    );

                    if let Some(ref err) = st_err {
                        ui.add_space(4.0);
                        ui.label(RichText::new(err).size(12.0).color(err_color));
                    }

                    if st_running {
                        ui.add_space(6.0);
                        if ui
                            .button(RichText::new("🛑 Batalkan Proses").color(err_color).strong())
                            .clicked()
                        {
                            cancel_flag.store(true, Ordering::Relaxed);
                            self.status_text =
                                "Permintaan pembatalan dikirim; membersihkan resource...".to_string();
                        }
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("📜 Log Operasi").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("🧹 Bersihkan").clicked() && !st_running {
                            if let Ok(mut st) = self.deploy_state.lock() {
                                st.logs.clear();
                            }
                        }
                        if ui.small_button("📋 Salin Log").clicked() {
                            let joined: String = st_logs
                                .iter()
                                .map(|(ts, lvl, msg)| format!("[{}][{}] {}", ts, lvl.tag(), msg))
                                .collect::<Vec<_>>()
                                .join("\n");
                            ui.ctx().copy_text(joined);
                            self.status_text = "Log eksekusi disalin ke clipboard.".to_string();
                        }
                    });
                });

                ui.separator();

                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if st_logs.is_empty() {
                            ui.label(
                                RichText::new(
                                    "Belum ada log eksekusi. Konfigurasi sumber & target di sebelah kiri lalu jalankan.",
                                )
                                .size(12.0)
                                .color(ui.visuals().weak_text_color()),
                            );
                        } else {
                            for (ts, lvl, msg) in &st_logs {
                                let col = match lvl {
                                    LogLevel::Ok => ok_color,
                                    LogLevel::Warn => warn_color,
                                    LogLevel::Error => err_color,
                                    LogLevel::Cmd => brand_color,
                                    LogLevel::Info => ui.visuals().text_color(),
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        RichText::new(format!("[{}][{}]", ts, lvl.tag()))
                                            .monospace()
                                            .size(11.0)
                                            .color(col)
                                            .strong(),
                                    );
                                    ui.label(RichText::new(msg).monospace().size(11.5));
                                });
                            }
                        }
                    });
            });

        // Central Panel: 4-Step Wizard
        egui::CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // =========================================================================
                // STEP 1: SOURCE / MODE SPECIFIC CONFIGURATION
                // =========================================================================
                ui.heading(RichText::new("1️⃣ Sumber Sistem / Operasi").size(16.0).strong());
                ui.add_space(4.0);

                match self.options.mode {
                    DeployMode::WindowsToGo | DeployMode::WindowsInstallerUsb => {
                        ui.group(|ui| {
                            let hint = if self.options.mode == DeployMode::WindowsInstallerUsb {
                                "Pilih berkas ISO Windows Setup (.iso)"
                            } else {
                                "Pilih berkas Windows (.iso, .wim, .esd, .swm, atau .vhd/.vhdx)"
                            };
                            ui.label(hint);
                            ui.horizontal(|ui| {
                                let resp = ui.add_enabled(
                                    !is_running,
                                    egui::TextEdit::singleline(&mut self.options.source_path)
                                        .desired_width(ui.available_width() - 255.0)
                                        .hint_text("Contoh: C:\\ISO\\Win11.iso"),
                                );
                                if resp.changed() {
                                    self.config.last_source_path = self.options.source_path.clone();
                                    self.config.save();
                                }

                                if ui
                                    .add_enabled(!is_running, egui::Button::new("📂 Pilih File..."))
                                    .clicked()
                                {
                                    if let Some(picked) = pick_image_file() {
                                        self.options.source_path = picked.clone();
                                        self.config.last_source_path = picked;
                                        self.config.save();
                                        self.trigger_image_inspection(ctx);
                                    }
                                }

                                let btn_lbl = if self.inspecting_image {
                                    "⏳ Menganalisis..."
                                } else {
                                    "🔍 Analisis Image"
                                };
                                if ui
                                    .add_enabled(
                                        !is_running
                                            && !self.inspecting_image
                                            && !self.options.source_path.trim().is_empty(),
                                        egui::Button::new(btn_lbl),
                                    )
                                    .clicked()
                                {
                                    self.trigger_image_inspection(ctx);
                                }
                            });

                            if let Some(ref err) = self.inspection_error {
                                ui.add_space(4.0);
                                ui.label(RichText::new(format!("❌ {}", err)).color(err_color));
                            }

                            if let Some(ref info) = self.inspection_result {
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!(
                                        "Sumber: {} ({}) — {}",
                                        info.source_kind.label(),
                                        format_bytes_human(info.image_file_size_bytes),
                                        info.source_path
                                    ))
                                    .color(ok_color)
                                    .strong(),
                                );
                                if info.source_path != self.options.source_path.trim() {
                                    ui.label(
                                        RichText::new(
                                            "⚠️ Path sumber berubah setelah analisis terakhir; klik 'Analisis Image' lagi untuk memperbarui daftar edisi.",
                                        )
                                        .size(12.0)
                                        .color(warn_color),
                                    );
                                }

                                for note in &info.notes {
                                    ui.label(
                                        RichText::new(format!("💡 {}", note))
                                            .size(12.0)
                                            .color(warn_color),
                                    );
                                }

                                if self.options.mode == DeployMode::WindowsToGo
                                    && !info.editions.is_empty()
                                {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new("Pilih Edisi Windows:").strong());
                                    egui::Grid::new("editions_grid")
                                        .striped(true)
                                        .min_col_width(50.0)
                                        .show(ui, |ui| {
                                            ui.label(RichText::new("Pilih").strong());
                                            ui.label(RichText::new("Index").strong());
                                            ui.label(RichText::new("Nama Edisi").strong());
                                            ui.label(RichText::new("Arsitektur").strong());
                                            ui.label(RichText::new("Versi").strong());
                                            ui.label(RichText::new("Ukuran").strong());
                                            ui.end_row();

                                            for ed in &info.editions {
                                                let selected = self.options.wim_index == ed.index;
                                                if ui.radio(selected, "").clicked() && !is_running {
                                                    self.options.wim_index = ed.index;
                                                    if ed.is_windows_11 {
                                                        self.options.bypass_win11_checks = true;
                                                    }
                                                }
                                                ui.label(format!("#{}", ed.index));
                                                ui.label(RichText::new(&ed.name).strong());
                                                ui.label(&ed.architecture);
                                                ui.label(if ed.version.is_empty() {
                                                    "-"
                                                } else {
                                                    &ed.version
                                                });
                                                ui.label(&ed.size_display);
                                                ui.end_row();
                                            }
                                        });
                                }
                            }
                        });
                    }
                    DeployMode::LiveOsClone => {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("Sumber: {}", SourceKind::CurrentOs.label())).strong());
                                if ui.small_button("🔄 Cek Ulang OS").clicked() {
                                    match query_live_os_info() {
                                        Ok(i) => {
                                            self.live_os = Some(i);
                                            self.live_os_error = None;
                                        }
                                        Err(e) => self.live_os_error = Some(e),
                                    }
                                }
                            });

                            if let Some(ref info) = self.live_os {
                                ui.label(format!(
                                    "• OS: {} (Build {}, {})",
                                    info.caption, info.version, info.architecture
                                ));
                                ui.label(format!(
                                    "• Mode Firmware Host: {} | Drive Sistem: {}",
                                    info.firmware_type, info.system_drive
                                ));
                                ui.label(format!(
                                    "• Ruang Terpakai: {} dari {} | Minimal USB Disarankan: {}",
                                    info.used_display(),
                                    info.total_display(),
                                    format_bytes_human(info.recommended_min_usb_bytes())
                                ));
                                ui.label(
                                    RichText::new(
                                        "💡 Menggunakan Volume Shadow Copy (VSS) agar file sistem yang sedang terbuka tetap tersalin konsisten.",
                                    )
                                    .size(12.0)
                                    .color(ui.visuals().weak_text_color()),
                                );
                            } else if let Some(ref e) = self.live_os_error {
                                ui.label(RichText::new(format!("❌ {}", e)).color(err_color));
                            }
                        });
                    }
                    DeployMode::DiskVhdUtility => {
                        ui.group(|ui| {
                            ui.label(RichText::new("Pilih Operasi Utilitas:").strong());
                            ui.radio_value(
                                &mut self.options.utility_action,
                                UtilityAction::FormatSinglePartition,
                                "🧹 Format Bersih USB (1 Partisi: NTFS / FAT32 / exFAT)",
                            );
                            ui.radio_value(
                                &mut self.options.utility_action,
                                UtilityAction::PrepareBootableLayout,
                                "📐 Buat Partisi Bootable (EFI FAT32 + Windows NTFS)",
                            );
                            ui.radio_value(
                                &mut self.options.utility_action,
                                UtilityAction::CreateStandaloneVhdx,
                                "📦 Buat File Virtual Disk (.VHDX) di Drive USB",
                            );

                            ui.add_space(6.0);
                            match self.options.utility_action {
                                UtilityAction::FormatSinglePartition => {
                                    ui.horizontal(|ui| {
                                        ui.label("Sistem Berkas:");
                                        ui.radio_value(
                                            &mut self.options.utility_fs_type,
                                            "ntfs".to_string(),
                                            "NTFS",
                                        );
                                        ui.radio_value(
                                            &mut self.options.utility_fs_type,
                                            "fat32".to_string(),
                                            "FAT32",
                                        );
                                        ui.radio_value(
                                            &mut self.options.utility_fs_type,
                                            "exfat".to_string(),
                                            "exFAT",
                                        );
                                        ui.separator();
                                        ui.checkbox(&mut self.options.utility_use_gpt, "Gunakan GPT (default MBR jika off)");
                                    });
                                }
                                UtilityAction::PrepareBootableLayout => {
                                    ui.label("Pilih skema partisi bootable pada Bagian 2 di bawah.");
                                }
                                UtilityAction::CreateStandaloneVhdx => {
                                    let letters = self.available_drive_letters_on_selected_disk();
                                    ui.horizontal(|ui| {
                                        ui.label("Simpan ke Drive:");
                                        if letters.is_empty() {
                                            ui.label(
                                                RichText::new("(Tidak ada huruf drive pada disk terpilih)")
                                                    .color(warn_color),
                                            );
                                        } else {
                                            for ch in letters {
                                                ui.radio_value(
                                                    &mut self.options.utility_vhdx_target_letter,
                                                    Some(ch),
                                                    format!("{}:\\", ch),
                                                );
                                            }
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Nama File:");
                                        ui.text_edit_singleline(&mut self.options.utility_vhdx_filename);
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Ukuran (GB):");
                                        ui.add(egui::Slider::new(&mut self.options.vhdx_size_gb, 16..=512));
                                        ui.checkbox(&mut self.options.vhdx_dynamic, "Dynamic (Expandable)");
                                    });
                                }
                            }
                        });
                    }
                }

                ui.add_space(12.0);

                // =========================================================================
                // STEP 2: TARGET USB DRIVE & PARTITION SCHEME
                // =========================================================================
                ui.heading(RichText::new("2️⃣ Target Drive USB & Skema Partisi").size(16.0).strong());
                ui.add_space(4.0);

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Daftar Disk Fisik:").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_enabled(
                                !is_running,
                                egui::Checkbox::new(
                                    &mut self.options.allow_non_usb_target,
                                    "Izinkan Disk Non-USB Eksternal (Lanjutan)",
                                ),
                            )
                            .on_hover_text(
                                "Disk 0, Disk Sistem, dan Disk Boot tetap terkunci permanen demi keamanan.",
                            );
                        });
                    });

                    if let Some(ref e) = self.disk_error {
                        ui.label(RichText::new(format!("❌ {}", e)).color(err_color));
                    }

                    let mut has_usb = false;
                    for d in &self.disks {
                        if d.is_safe_usb_target() {
                            has_usb = true;
                        }
                        let is_selected = self.options.target_disk_number == d.number;
                        let is_locked = d.is_system || d.is_boot || d.number == 0 || (!self.options.allow_non_usb_target && !d.bus_type.eq_ignore_ascii_case("USB"));

                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(
                                        !is_running && !is_locked,
                                        egui::RadioButton::new(is_selected, ""),
                                    )
                                    .clicked()
                                {
                                    self.options.target_disk_number = d.number;
                                    self.confirm_disk_text.clear();
                                    self.confirm_wipe_checked = false;
                                }

                                ui.label(
                                    RichText::new(format!("Disk {} — {}", d.number, d.friendly_name))
                                        .strong()
                                        .size(14.0),
                                );

                                ui.label(format!("[{} | {}]", d.size_display(), d.partition_style));

                                let bus_badge = if d.bus_type.eq_ignore_ascii_case("USB") {
                                    RichText::new("[USB]").color(ok_color).strong()
                                } else {
                                    RichText::new(format!("[{}]", d.bus_type))
                                        .color(warn_color)
                                        .strong()
                                };
                                ui.label(bus_badge);

                                if d.is_system || d.is_boot || d.number == 0 {
                                    ui.label(
                                        RichText::new("🔒 DILINDUNGI (Sistem/Utama)")
                                            .color(err_color)
                                            .strong(),
                                    );
                                } else if is_locked {
                                    ui.label(
                                        RichText::new("🔒 Non-USB Terkunci")
                                            .color(warn_color),
                                    );
                                }
                            });

                            if !d.partitions.is_empty() {
                                let part_summary: Vec<String> = d
                                    .partitions
                                    .iter()
                                    .map(|p| {
                                        let dl = p
                                            .drive_letter
                                            .map(|c| format!("{}:", c))
                                            .unwrap_or_else(|| format!("Part#{}", p.partition_number));
                                        let fs = if p.file_system.is_empty() {
                                            p.part_type.as_str()
                                        } else {
                                            p.file_system.as_str()
                                        };
                                        format!("{} ({} {})", dl, fs, format_bytes_human(p.size_bytes))
                                    })
                                    .collect();
                                ui.label(
                                    RichText::new(format!("   Partisi: {}", part_summary.join("  |  ")))
                                        .size(11.5)
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                        });
                    }

                    if !has_usb {
                        ui.label(
                            RichText::new(
                                "ℹ️ Tidak ada flashdisk/SSD USB eksternal yang aman terdeteksi saat ini. Colokkan USB ≥ 16 GB lalu klik 'Segarkan Disk'.",
                            )
                            .color(warn_color)
                            .size(12.5),
                        );
                    }

                    // Partition Scheme & VHDX mode controls
                    let show_scheme = self.options.mode != DeployMode::DiskVhdUtility
                        || self.options.utility_action == UtilityAction::PrepareBootableLayout;

                    if show_scheme {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(RichText::new("Skema Partisi:").strong());

                        let schemes: Vec<PartitionScheme> = match self.options.mode {
                            DeployMode::WindowsInstallerUsb => {
                                vec![PartitionScheme::GptUefi, PartitionScheme::MbrBios]
                            }
                            DeployMode::DiskVhdUtility => vec![
                                PartitionScheme::GptUefi,
                                PartitionScheme::MbrBios,
                                PartitionScheme::MbrBiosAndUefi,
                            ],
                            _ => vec![
                                PartitionScheme::GptUefi,
                                PartitionScheme::MbrBios,
                                PartitionScheme::MbrBiosAndUefi,
                                PartitionScheme::KeepExisting,
                            ],
                        };

                        for sc in schemes {
                            let label = if self.options.mode == DeployMode::WindowsInstallerUsb {
                                match sc {
                                    PartitionScheme::GptUefi => "GPT (UEFI Only)",
                                    PartitionScheme::MbrBios => "MBR (BIOS & UEFI FAT32 Standar)",
                                    _ => sc.label_id(),
                                }
                            } else {
                                sc.label_id()
                            };
                            ui.add_enabled(
                                !is_running,
                                egui::RadioButton::new(self.options.scheme == sc, label),
                            )
                            .clicked()
                            .then(|| {
                                self.options.scheme = sc;
                                self.confirm_wipe_checked = false;
                            });
                            ui.label(
                                RichText::new(format!("   {}", sc.description_id()))
                                    .size(11.5)
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }

                        if self.options.scheme == PartitionScheme::KeepExisting {
                            let avail = self.available_drive_letters_on_selected_disk();
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label("Huruf Partisi Sistem (EFI/Boot):");
                                if avail.is_empty() {
                                    ui.label(RichText::new("(Tidak ada huruf partisi)").color(warn_color));
                                } else {
                                    for ch in &avail {
                                        ui.radio_value(
                                            &mut self.options.existing_sys_letter,
                                            Some(*ch),
                                            format!("{}:\\", ch),
                                        );
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Huruf Partisi Windows Target:");
                                for ch in &avail {
                                    ui.radio_value(
                                        &mut self.options.existing_win_letter,
                                        Some(*ch),
                                        format!("{}:\\", ch),
                                    );
                                }
                            });
                        }
                    }

                    // VHDX Installation Mode for WTG and Live Clone
                    if matches!(
                        self.options.mode,
                        DeployMode::WindowsToGo | DeployMode::LiveOsClone
                    ) {
                        ui.add_space(6.0);
                        ui.separator();
                        ui.label(RichText::new("Mode Penempatan Sistem:").strong());
                        ui.horizontal(|ui| {
                            ui.add_enabled(
                                !is_running,
                                egui::RadioButton::new(
                                    !self.options.use_vhdx,
                                    "📁 Langsung ke Partisi NTFS (Legacy/Cepat)",
                                ),
                            )
                            .clicked()
                            .then(|| self.options.use_vhdx = false);

                            ui.add_enabled(
                                !is_running,
                                egui::RadioButton::new(
                                    self.options.use_vhdx,
                                    "📦 Virtual Disk (.VHDX Native Boot)",
                                ),
                            )
                            .clicked()
                            .then(|| self.options.use_vhdx = true);
                        });

                        if self.options.use_vhdx {
                            ui.horizontal(|ui| {
                                ui.label("Ukuran VHDX (GB):");
                                ui.add_enabled(
                                    !is_running,
                                    egui::Slider::new(&mut self.options.vhdx_size_gb, 16..=512),
                                );
                                ui.add_enabled(
                                    !is_running,
                                    egui::Checkbox::new(
                                        &mut self.options.vhdx_dynamic,
                                        "Dynamic (Expandable)",
                                    ),
                                );
                            });
                        }
                    }
                });

                ui.add_space(12.0);

                // =========================================================================
                // STEP 3: ADVANCED OPTIONS & DRY-RUN PREVIEW
                // =========================================================================
                ui.heading(RichText::new("3️⃣ Opsi Lanjutan & Preview Rencana").size(16.0).strong());
                ui.add_space(4.0);

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Label Volume USB:");
                        let r = ui.add_enabled(
                            !is_running,
                            egui::TextEdit::singleline(&mut self.options.volume_label)
                                .desired_width(180.0),
                        );
                        if r.changed() {
                            self.config.default_volume_label = self.options.volume_label.clone();
                            self.config.save();
                        }
                    });

                    if matches!(
                        self.options.mode,
                        DeployMode::WindowsToGo | DeployMode::LiveOsClone
                    ) {
                        ui.add_enabled(
                            !is_running,
                            egui::Checkbox::new(
                                &mut self.options.bypass_win11_checks,
                                "🚀 Bypass Syarat Windows 11 (TPM 2.0, SecureBoot, RAM 4GB, Akun Lokal BypassNRO)",
                            ),
                        );
                        ui.add_enabled(
                            !is_running,
                            egui::Checkbox::new(
                                &mut self.options.enable_wtg_tweaks,
                                "🔌 Aktifkan Mode PortableOperatingSystem=1, Driver Boot USB (UASP/XHCI), & Nonaktifkan Hibernasi",
                            ),
                        );
                        ui.add_enabled(
                            !is_running,
                            egui::Checkbox::new(
                                &mut self.options.disable_winre,
                                "🧹 Hapus WinRE offline (Winre.wim) untuk hemat ~500 MB ruang flashdisk",
                            ),
                        );

                        ui.horizontal(|ui| {
                            let mut drv_txt = self.options.driver_folder.clone().unwrap_or_default();
                            ui.label("Suntikkan Driver (.inf):");
                            if ui
                                .add_enabled(
                                    !is_running,
                                    egui::TextEdit::singleline(&mut drv_txt)
                                        .desired_width(240.0)
                                        .hint_text("(Opsional) Folder berisi driver .inf"),
                                )
                                .changed()
                            {
                                self.options.driver_folder = if drv_txt.trim().is_empty() {
                                    None
                                } else {
                                    Some(drv_txt)
                                };
                            }
                            if ui
                                .add_enabled(!is_running, egui::Button::new("📂 Pilih Folder"))
                                .clicked()
                            {
                                if let Some(folder) = pick_folder() {
                                    self.options.driver_folder = Some(folder);
                                }
                            }
                            if self.options.driver_folder.is_some()
                                && ui.add_enabled(!is_running, egui::Button::new("✖")).clicked()
                            {
                                self.options.driver_folder = None;
                            }
                        });
                    } else if self.options.mode == DeployMode::WindowsInstallerUsb {
                        ui.add_enabled(
                            !is_running,
                            egui::Checkbox::new(
                                &mut self.options.bypass_win11_checks,
                                "🚀 Sertakan autounattend.xml Bypass Syarat Windows 11 (TPM/SecureBoot/RAM)",
                            ),
                        );
                    }

                    ui.add_space(6.0);

                    // Dry-Run Preview Collapsing Section
                    let selected_disk_opt = self.selected_disk().cloned();
                    egui::CollapsingHeader::new("🔍 Preview Rencana Eksekusi (Dry-Run — Tanpa Efek ke Disk)")
                        .default_open(false)
                        .show(ui, |ui| {
                            if let Some(ref td) = selected_disk_opt {
                                match build_dry_run_plan(
                                    &self.options,
                                    td,
                                    self.live_os.as_ref(),
                                    None,
                                ) {
                                    Ok(plan) => {
                                        for w in &plan.warnings {
                                            ui.label(RichText::new(format!("⚠️ {}", w)).size(12.0).color(warn_color));
                                        }
                                        for step in &plan.summary_steps {
                                            ui.label(RichText::new(step).size(12.0));
                                        }
                                        ui.add_space(4.0);
                                        ui.label(RichText::new("Skrip DiskPart:").strong().size(12.0));
                                        ui.monospace(&plan.diskpart_script);

                                        if let Some(ref vs) = plan.vhdx_script {
                                            ui.label(RichText::new("Skrip VHDX:").strong().size(12.0));
                                            ui.monospace(vs);
                                        }

                                        if !plan.dism_commands.is_empty() {
                                            ui.label(RichText::new("Perintah DISM / Salin:").strong().size(12.0));
                                            for dc in &plan.dism_commands {
                                                ui.monospace(dc);
                                            }
                                        }

                                        ui.label(RichText::new("Perintah BCDBoot:").strong().size(12.0));
                                        ui.monospace(&plan.bcdboot_command);

                                        if !plan.registry_tweaks.is_empty() {
                                            ui.label(RichText::new("Tweak Offline Registry:").strong().size(12.0));
                                            for t in &plan.registry_tweaks {
                                                ui.label(RichText::new(format!("• {}", t)).size(11.5));
                                            }
                                        }
                                    }
                                    Err(reason) => {
                                        ui.label(RichText::new(format!("⚠️ {}", reason)).color(warn_color));
                                    }
                                }
                            } else {
                                ui.label(
                                    RichText::new("Pilih disk target terlebih dahulu untuk melihat rencana eksekusi.")
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                        });
                });

                ui.add_space(12.0);

                // =========================================================================
                // STEP 4: SAFETY CONFIRMATION & EXECUTE
                // =========================================================================
                ui.heading(RichText::new("4️⃣ Konfirmasi & Eksekusi").size(16.0).strong());
                ui.add_space(4.0);

                ui.group(|ui| {
                    let selected_disk_opt = self.selected_disk().cloned();
                    let plan_validation = selected_disk_opt.as_ref().ok_or_else(|| {
                        "Pilih drive USB target yang aman pada Langkah 2.".to_string()
                    }).and_then(|td| {
                        build_dry_run_plan(&self.options, td, self.live_os.as_ref(), None).map(|_| td)
                    });

                    let destructive = self.needs_destructive_confirmation();
                    let mut confirmed = !destructive;

                    match plan_validation {
                        Ok(td) => {
                            if destructive {
                                ui.label(
                                    RichText::new(format!(
                                        "⚠️ PERINGATAN PENGHAPUSAN DATA: Operasi ini akan memformat ulang Disk {} ({}, {}).",
                                        td.number,
                                        td.friendly_name,
                                        td.size_display()
                                    ))
                                    .color(err_color)
                                    .strong(),
                                );

                                ui.horizontal(|ui| {
                                    ui.label(format!("Ketik angka '{}' untuk konfirmasi:", td.number));
                                    ui.add_enabled(
                                        !is_running,
                                        egui::TextEdit::singleline(&mut self.confirm_disk_text)
                                            .desired_width(60.0),
                                    );
                                });

                                ui.add_enabled(
                                    !is_running,
                                    egui::Checkbox::new(
                                        &mut self.confirm_wipe_checked,
                                        format!(
                                            "Saya mengerti seluruh data di Disk {} ({}) akan dihapus permanen",
                                            td.number, td.friendly_name
                                        ),
                                    ),
                                );

                                let num_matches =
                                    self.confirm_disk_text.trim() == td.number.to_string();
                                confirmed = num_matches && self.confirm_wipe_checked;
                            } else {
                                ui.label(
                                    RichText::new("✅ Rencana siap dijalankan tanpa memformat seluruh disk fisik.")
                                        .color(ok_color),
                                );
                            }

                            if !self.is_admin {
                                ui.label(
                                    RichText::new(
                                        "⚠️ Anda belum menjalankan WinToUSB sebagai Administrator. Klik tombol 'Jalankan sebagai Admin (UAC)' di kanan atas terlebih dahulu.",
                                    )
                                    .color(warn_color)
                                    .size(12.0),
                                );
                            }

                            ui.add_space(6.0);
                            let can_start = !is_running && confirmed && self.is_admin;
                            let start_btn = egui::Button::new(
                                RichText::new("🚀 Mulai Proses Pembuatan USB")
                                    .size(15.0)
                                    .strong(),
                            )
                            .min_size(Vec2::new(260.0, 36.0));

                            if ui.add_enabled(can_start, start_btn).clicked() {
                                self.status_text = "Deployment sedang berjalan...".to_string();
                                start_deployment(
                                    self.options.clone(),
                                    self.deploy_state.clone(),
                                    ctx.clone(),
                                );
                            }
                        }
                        Err(reason) => {
                            ui.label(RichText::new(format!("🚫 {}", reason)).color(warn_color));
                            ui.add_space(4.0);
                            ui.add_enabled(
                                false,
                                egui::Button::new("🚀 Mulai Proses Pembuatan USB")
                                    .min_size(Vec2::new(260.0, 36.0)),
                            );
                        }
                    }
                });

                ui.add_space(16.0);
            });
        });
    }
}
