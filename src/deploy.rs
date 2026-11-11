use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::disk::{
    enumerate_disks, execute_diskpart_script, find_free_drive_letters,
    generate_diskpart_script, generate_single_fs_script, new_hidden_command, temp_script_path,
    validate_target_disk, DiskInfo, PartitionScheme, MIN_INSTALLER_DISK_BYTES,
    MIN_WTG_DISK_BYTES,
};
use crate::elevate::is_elevated;
use crate::image::{
    detect_source_kind, locate_install_image, mount_iso, parse_dism_progress, SourceKind,
};
use crate::vhdx::{validate_vhdx_config, VhdxAttachGuard, VhdxConfig};
use crate::vss::{
    build_vss_capture_dism_args, generate_wimscript_ini, query_live_os_info, LiveOsInfo,
    VssShadowGuard,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeployMode {
    WindowsToGo,
    LiveOsClone,
    WindowsInstallerUsb,
    DiskVhdUtility,
}

impl DeployMode {
    pub fn title_id(&self) -> &'static str {
        match self {
            Self::WindowsToGo => "🪟 Windows To Go (ISO/WIM/ESD → USB)",
            Self::LiveOsClone => "🧬 Kloning OS Aktif (Live Clone → USB)",
            Self::WindowsInstallerUsb => "💿 Windows Setup USB (Installer)",
            Self::DiskVhdUtility => "🛠️ Alat Disk & VHDX",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UtilityAction {
    FormatSinglePartition,
    PrepareBootableLayout,
    CreateStandaloneVhdx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Cmd,
    Ok,
    Warn,
    Error,
}

impl LogLevel {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Cmd => "CMD",
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Error => "ERR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeployOptions {
    pub mode: DeployMode,
    pub source_path: String,
    pub wim_index: u32,
    pub is_split_swm: bool,
    pub target_disk_number: u32,
    pub allow_non_usb_target: bool,
    pub scheme: PartitionScheme,
    pub use_vhdx: bool,
    pub vhdx_size_gb: u32,
    pub vhdx_dynamic: bool,
    pub volume_label: String,
    pub bypass_win11_checks: bool,
    pub enable_wtg_tweaks: bool,
    pub disable_winre: bool,
    pub driver_folder: Option<String>,
    pub existing_sys_letter: Option<char>,
    pub existing_win_letter: Option<char>,
    pub utility_action: UtilityAction,
    pub utility_fs_type: String,
    pub utility_use_gpt: bool,
    pub utility_vhdx_target_letter: Option<char>,
    pub utility_vhdx_filename: String,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self {
            mode: DeployMode::WindowsToGo,
            source_path: String::new(),
            wim_index: 1,
            is_split_swm: false,
            target_disk_number: 0,
            allow_non_usb_target: false,
            scheme: PartitionScheme::GptUefi,
            use_vhdx: false,
            vhdx_size_gb: 32,
            vhdx_dynamic: true,
            volume_label: "WinToUSB".to_string(),
            bypass_win11_checks: false,
            enable_wtg_tweaks: true,
            disable_winre: true,
            driver_folder: None,
            existing_sys_letter: None,
            existing_win_letter: None,
            utility_action: UtilityAction::FormatSinglePartition,
            utility_fs_type: "ntfs".to_string(),
            utility_use_gpt: true,
            utility_vhdx_target_letter: None,
            utility_vhdx_filename: "WinToGo.vhdx".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DryRunPlan {
    pub summary_steps: Vec<String>,
    pub diskpart_script: String,
    pub vhdx_script: Option<String>,
    pub dism_commands: Vec<String>,
    pub bcdboot_command: String,
    pub registry_tweaks: Vec<String>,
    pub warnings: Vec<String>,
}

pub struct DeployState {
    pub running: bool,
    pub finished: bool,
    pub success: bool,
    pub current_step: usize,
    pub total_steps: usize,
    pub step_title: String,
    pub progress_pct: f32,
    pub logs: Vec<(String, LogLevel, String)>,
    pub started_at: Option<Instant>,
    pub finished_elapsed_secs: Option<u64>,
    pub error_message: Option<String>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl Default for DeployState {
    fn default() -> Self {
        Self {
            running: false,
            finished: false,
            success: false,
            current_step: 0,
            total_steps: 1,
            step_title: "Siap dijalankan".to_string(),
            progress_pct: 0.0,
            logs: Vec::new(),
            started_at: None,
            finished_elapsed_secs: None,
            error_message: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DeployState {
    pub fn push_log(&mut self, level: LogLevel, msg: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        self.logs.push((ts, level, msg.into()));
        if self.logs.len() > 2000 {
            let drain = self.logs.len() - 2000;
            self.logs.drain(0..drain);
        }
    }
}

/// Build deterministic `bcdboot.exe` argument vector per partition scheme.
pub fn generate_bcdboot_args(
    windows_dir: &str,
    system_letter: char,
    scheme: PartitionScheme,
) -> Result<Vec<String>, String> {
    let win = windows_dir.trim();
    if win.is_empty() {
        return Err("Path direktori Windows untuk BCDBoot tidak boleh kosong.".to_string());
    }
    let sys = system_letter.to_ascii_uppercase();
    if !('E'..='Z').contains(&sys) {
        return Err(format!(
            "Huruf drive sistem BCDBoot '{}' tidak valid (harus E-Z).",
            system_letter
        ));
    }

    let fw_flag = match scheme {
        PartitionScheme::GptUefi => "UEFI",
        PartitionScheme::MbrBios => "BIOS",
        PartitionScheme::MbrBiosAndUefi | PartitionScheme::KeepExisting => "ALL",
    };

    Ok(vec![
        win.to_string(),
        "/s".to_string(),
        format!("{}:", sys),
        "/f".to_string(),
        fw_flag.to_string(),
    ])
}

/// Build DISM `/Apply-Image` arguments, including `/SWMFile` for split images.
pub fn generate_dism_apply_args(
    image_file: &str,
    index: u32,
    apply_dir: &str,
    is_split_swm: bool,
) -> Vec<String> {
    let mut args = vec![
        "/English".to_string(),
        "/Apply-Image".to_string(),
        format!("/ImageFile:{}", image_file),
    ];

    if is_split_swm {
        let p = Path::new(image_file);
        let parent = p.parent().unwrap_or_else(|| Path::new(""));
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("install");
        // Strip trailing digits if user picked install2.swm -> install*.swm
        let base = stem.trim_end_matches(|c: char| c.is_ascii_digit());
        let pattern = parent.join(format!("{}*.swm", if base.is_empty() { "install" } else { base }));
        args.push(format!("/SWMFile:{}", pattern.to_string_lossy()));
    }

    args.push(format!("/Index:{}", index.max(1)));
    args.push(format!("/ApplyDir:{}", apply_dir));
    args
}

/// Generate clean `autounattend.xml` (windowsPE pass) to bypass Win11 TPM/RAM/SecureBoot checks on Setup USB.
pub fn generate_installer_bypass_autounattend() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>3</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>4</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassCPUCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>5</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassStorageCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <OOBE>
        <HideWirelessSetupInOOBE>false</HideWirelessSetupInOOBE>
      </OOBE>
    </component>
  </settings>
</unattend>
"#
    .to_string()
}

pub fn describe_registry_tweaks(opts: &DeployOptions) -> Vec<String> {
    let mut tweaks = Vec::new();
    if matches!(
        opts.mode,
        DeployMode::WindowsToGo | DeployMode::LiveOsClone
    ) {
        if opts.enable_wtg_tweaks {
            tweaks.push(
                "HKLM\\SYSTEM\\ControlSet001\\Control\\PortableOperatingSystem = 1 (Windows To Go)"
                    .to_string(),
            );
            tweaks.push(
                "HKLM\\SYSTEM\\ControlSet001\\Services\\{USBSTOR,UASPStor,USBXHCI,USBHUB3}\\Start = 0 (Early USB Boot)"
                    .to_string(),
            );
            tweaks.push(
                "HKLM\\SYSTEM\\ControlSet001\\Control\\Power\\HibernateEnabled = 0 (Hemat ruang & umur SSD/USB)"
                    .to_string(),
            );
        }
        if opts.bypass_win11_checks {
            tweaks.push(
                "HKLM\\SYSTEM\\Setup\\LabConfig\\{BypassTPMCheck,BypassSecureBootCheck,BypassRAMCheck,BypassCPUCheck} = 1"
                    .to_string(),
            );
            tweaks.push(
                "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE\\BypassNRO = 1 (Izinkan Akun Lokal)"
                    .to_string(),
            );
        }
        if opts.disable_winre {
            tweaks.push(
                "Hapus Winre.wim offline dari target USB (hemat ~500 MB & cegah WinRE konflik)"
                    .to_string(),
            );
        }
    } else if opts.mode == DeployMode::WindowsInstallerUsb && opts.bypass_win11_checks {
        tweaks.push(
            "Menulis autounattend.xml (windowsPE LabConfig Bypass TPM/SecureBoot/RAM/CPU) ke root USB"
                .to_string(),
        );
    }
    tweaks
}

fn validate_keep_existing_letters(opts: &DeployOptions) -> Result<(char, char), String> {
    let win = opts
        .existing_win_letter
        .ok_or("Pilih huruf partisi Windows target untuk mode 'Pertahankan Partisi'.")?
        .to_ascii_uppercase();
    if !('E'..='Z').contains(&win) {
        return Err(format!("Huruf partisi Windows '{}' tidak valid (harus E-Z).", win));
    }
    let sys = opts.existing_sys_letter.unwrap_or(win).to_ascii_uppercase();
    if !('E'..='Z').contains(&sys) {
        return Err(format!("Huruf partisi Sistem '{}' tidak valid (harus E-Z).", sys));
    }
    Ok((sys, win))
}

/// Generate a complete, zero-side-effect execution plan for preview and validation.
pub fn build_dry_run_plan(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    live_os: Option<&LiveOsInfo>,
    preview_letters: Option<&[char]>,
) -> Result<DryRunPlan, String> {
    let min_bytes = match opts.mode {
        DeployMode::WindowsToGo => MIN_WTG_DISK_BYTES,
        DeployMode::LiveOsClone => live_os
            .map(|o| o.recommended_min_usb_bytes())
            .unwrap_or(MIN_WTG_DISK_BYTES),
        DeployMode::WindowsInstallerUsb => MIN_INSTALLER_DISK_BYTES,
        DeployMode::DiskVhdUtility => 1024 * 1024 * 1024, // 1 GB min for format utility
    };

    // Validate disk safety for all destructive / disk-level operations
    if opts.mode != DeployMode::DiskVhdUtility
        || opts.utility_action != UtilityAction::CreateStandaloneVhdx
    {
        if opts.scheme.is_destructive() || opts.mode == DeployMode::WindowsInstallerUsb {
            validate_target_disk(target_disk, opts.allow_non_usb_target, min_bytes)?;
        } else if target_disk.is_system || target_disk.is_boot || target_disk.number == 0 {
            return Err(format!(
                "DIBLOKIR: Disk {} ({}) adalah disk sistem/utama.",
                target_disk.number, target_disk.friendly_name
            ));
        }
    }

    let letters: Vec<char> = match preview_letters {
        Some(l) if l.len() >= 3 => l.to_vec(),
        _ => find_free_drive_letters(3, Some(0b0000_1111))?, // Fallback preview letters Z, Y, X
    };

    let mut summary_steps = Vec::new();
    let mut warnings = Vec::new();
    let mut dism_commands = Vec::new();

    if !opts.allow_non_usb_target && !target_disk.is_safe_usb_target() {
        warnings.push("Disk target terdeteksi bukan USB standar.".to_string());
    }

    match opts.mode {
        DeployMode::WindowsToGo => {
            if opts.source_path.trim().is_empty() {
                return Err("Pilih berkas sumber ISO, WIM, ESD, SWM, atau VHDX terlebih dahulu.".to_string());
            }
            let src_kind = detect_source_kind(&opts.source_path)?;

            let (sys_l, outer_win_l, diskpart_script) = if opts.scheme == PartitionScheme::KeepExisting {
                let (s, w) = validate_keep_existing_letters(opts)?;
                (
                    s,
                    w,
                    format!(
                        "; Mode Pertahankan Partisi: menggunakan Sistem {}:\\ dan Target {}:\\ tanpa clean disk.\n",
                        s, w
                    ),
                )
            } else {
                let sys = letters[0];
                let win = letters[1];
                let s_opt = if opts.scheme.needs_separate_system_partition() {
                    Some(sys)
                } else {
                    None
                };
                let dp = generate_diskpart_script(
                    target_disk.number,
                    opts.scheme,
                    s_opt,
                    Some(win),
                    if opts.scheme == PartitionScheme::MbrBiosAndUefi {
                        300
                    } else {
                        260
                    },
                    &opts.volume_label,
                )?;
                (if opts.scheme.needs_separate_system_partition() { sys } else { win }, win, dp)
            };

            let (vhdx_script, apply_letter) = if opts.use_vhdx {
                let inner_letter = letters[2];
                let vhd_path = format!("{}:\\WinToGo.vhdx", outer_win_l);
                let cfg = VhdxConfig {
                    vhd_path,
                    size_gb: opts.vhdx_size_gb,
                    expandable: opts.vhdx_dynamic,
                    label: opts.volume_label.clone(),
                };
                validate_vhdx_config(&cfg, None)?;
                let vs = crate::vhdx::generate_vhdx_create_script(&cfg, inner_letter)?;
                (Some(vs), inner_letter)
            } else {
                (None, outer_win_l)
            };

            summary_steps.push(format!(
                "1. Persiapkan Disk {} ({}) menggunakan skema {}",
                target_disk.number,
                target_disk.friendly_name,
                opts.scheme.label_id()
            ));
            if src_kind == SourceKind::Iso {
                summary_steps.push(format!("2. Mount ISO '{}' & temukan sources\\install.*", opts.source_path));
            }
            if opts.use_vhdx {
                summary_steps.push(format!(
                    "3. Buat & attach VHDX ({} GB, {}) pada {}:\\WinToGo.vhdx",
                    opts.vhdx_size_gb,
                    if opts.vhdx_dynamic { "Dynamic" } else { "Fixed" },
                    outer_win_l
                ));
            }

            let apply_dir = format!("{}:\\", apply_letter);
            if src_kind == SourceKind::VhdVhdx {
                dism_commands.push(format!(
                    "Copy-Item \"{}\" -> \"{}:\\WinToGo.vhdx\" (Direct VHD/VHDX Deploy)",
                    opts.source_path, outer_win_l
                ));
            } else {
                let sample_wim = if src_kind == SourceKind::Iso {
                    "<ISO_MOUNT>:\\sources\\install.wim"
                } else {
                    opts.source_path.as_str()
                };
                let args = generate_dism_apply_args(
                    sample_wim,
                    opts.wim_index,
                    &apply_dir,
                    opts.is_split_swm,
                );
                dism_commands.push(format!("dism.exe {}", args.join(" ")));
            }

            if let Some(drv_dir) = &opts.driver_folder {
                if !drv_dir.trim().is_empty() {
                    dism_commands.push(format!(
                        "dism.exe /Image:{} /Add-Driver /Driver:\"{}\" /Recurse",
                        apply_dir, drv_dir
                    ));
                }
            }

            let bcd_args = generate_bcdboot_args(
                &format!("{}:\\Windows", apply_letter),
                sys_l,
                opts.scheme,
            )?;
            let bcdboot_command = format!("bcdboot.exe {}", bcd_args.join(" "));

            summary_steps.push(format!("4. Terapkan Windows Edisi #{} ke {}", opts.wim_index, apply_dir));
            summary_steps.push("5. Terapkan tweak offline Registry Windows To Go / Bypass Win11".to_string());
            summary_steps.push(format!("6. Instal bootloader via {}", bcdboot_command));

            Ok(DryRunPlan {
                summary_steps,
                diskpart_script,
                vhdx_script,
                dism_commands,
                bcdboot_command,
                registry_tweaks: describe_registry_tweaks(opts),
                warnings,
            })
        }
        DeployMode::LiveOsClone => {
            let (sys_l, outer_win_l, diskpart_script) = if opts.scheme == PartitionScheme::KeepExisting {
                let (s, w) = validate_keep_existing_letters(opts)?;
                (
                    s,
                    w,
                    format!("; Pertahankan partisi {}:\\ dan {}:\\.\n", s, w),
                )
            } else {
                let sys = letters[0];
                let win = letters[1];
                let s_opt = if opts.scheme.needs_separate_system_partition() {
                    Some(sys)
                } else {
                    None
                };
                let dp = generate_diskpart_script(
                    target_disk.number,
                    opts.scheme,
                    s_opt,
                    Some(win),
                    if opts.scheme == PartitionScheme::MbrBiosAndUefi { 300 } else { 260 },
                    &opts.volume_label,
                )?;
                (if opts.scheme.needs_separate_system_partition() { sys } else { win }, win, dp)
            };

            let (vhdx_script, apply_letter) = if opts.use_vhdx {
                let inner = letters[2];
                let cfg = VhdxConfig {
                    vhd_path: format!("{}:\\WinToGo.vhdx", outer_win_l),
                    size_gb: opts.vhdx_size_gb,
                    expandable: opts.vhdx_dynamic,
                    label: opts.volume_label.clone(),
                };
                validate_vhdx_config(&cfg, None)?;
                (Some(crate::vhdx::generate_vhdx_create_script(&cfg, inner)?), inner)
            } else {
                (None, outer_win_l)
            };

            let cap_args = build_vss_capture_dism_args(
                "<TEMP>\\WinToUSB_LiveClone.wim",
                "<TEMP_VSS_SYMLINK>",
                Some("<TEMP>\\WimScript.ini"),
            );
            let apply_args = generate_dism_apply_args(
                "<TEMP>\\WinToUSB_LiveClone.wim",
                1,
                &format!("{}:\\", apply_letter),
                false,
            );
            dism_commands.push(format!("dism.exe {}", cap_args.join(" ")));
            dism_commands.push(format!("dism.exe {}", apply_args.join(" ")));

            let bcd_args = generate_bcdboot_args(
                &format!("{}:\\Windows", apply_letter),
                sys_l,
                opts.scheme,
            )?;
            let bcdboot_command = format!("bcdboot.exe {}", bcd_args.join(" "));

            summary_steps.push("1. Buat Volume Shadow Copy (VSS) drive C:\\ yang sedang berjalan".to_string());
            summary_steps.push("2. Tangkap OS aktif melalui DISM /Capture-Image (tanpa pagefile/hiberfil)".to_string());
            summary_steps.push(format!("3. Format Disk {} & terapkan OS kloning ke {}:\\", target_disk.number, apply_letter));
            summary_steps.push(format!("4. Pasang bootloader ({}) & tweak PortableOperatingSystem", bcdboot_command));

            Ok(DryRunPlan {
                summary_steps,
                diskpart_script,
                vhdx_script,
                dism_commands,
                bcdboot_command,
                registry_tweaks: describe_registry_tweaks(opts),
                warnings,
            })
        }
        DeployMode::WindowsInstallerUsb => {
            if opts.source_path.trim().is_empty() {
                return Err("Pilih berkas ISO Windows terlebih dahulu untuk membuat USB Installer.".to_string());
            }
            if detect_source_kind(&opts.source_path)? != SourceKind::Iso {
                return Err("Mode USB Installer membutuhkan berkas sumber .iso Windows.".to_string());
            }
            let win_l = letters[1];
            let dp = generate_single_fs_script(
                target_disk.number,
                opts.scheme == PartitionScheme::GptUefi,
                "fat32",
                win_l,
                &opts.volume_label,
                target_disk.size_bytes,
            )?;
            if target_disk.size_bytes > 32 * 1024 * 1024 * 1024 {
                warnings.push(
                    "Disk > 32 GB: Partisi FAT32 bootable otomatis dibatasi ke 32.000 MB agar kompatibel dengan batas format FAT32 Windows.".to_string(),
                );
            }
            summary_steps.push(format!("1. Format Disk {} ke FAT32 bootable ({}:\\)", target_disk.number, win_l));
            summary_steps.push(format!("2. Mount ISO '{}' & salin semua berkas Setup Windows", opts.source_path));
            summary_steps.push("3. Jika install.wim > 4 GB, pecah otomatis menjadi install*.swm (DISM /Split-Image)".to_string());
            if opts.bypass_win11_checks {
                summary_steps.push("4. Tambahkan autounattend.xml Bypass TPM/SecureBoot/RAM/CPU".to_string());
            }

            dism_commands.push(format!(
                "robocopy <ISO_MOUNT>:\\ {}:\\ /E (kecuali install.wim jika >4GB -> dism /Split-Image /FileSize:3800)",
                win_l
            ));

            Ok(DryRunPlan {
                summary_steps,
                diskpart_script: dp,
                vhdx_script: None,
                dism_commands,
                bcdboot_command: "(Native ISO EFI\\Boot\\bootx64.efi & bootmgr dari media ISO)".to_string(),
                registry_tweaks: describe_registry_tweaks(opts),
                warnings,
            })
        }
        DeployMode::DiskVhdUtility => match opts.utility_action {
            UtilityAction::FormatSinglePartition => {
                let drv = letters[1];
                let dp = generate_single_fs_script(
                    target_disk.number,
                    opts.utility_use_gpt,
                    &opts.utility_fs_type,
                    drv,
                    &opts.volume_label,
                    target_disk.size_bytes,
                )?;
                if opts.utility_fs_type.eq_ignore_ascii_case("fat32")
                    && target_disk.size_bytes > 32 * 1024 * 1024 * 1024
                {
                    warnings.push(
                        "FAT32 pada disk > 32 GB otomatis dibatasi ke 32.000 MB (gunakan exFAT/NTFS untuk kapasitas penuh).".to_string(),
                    );
                }
                summary_steps.push(format!(
                    "Format bersih Disk {} ({}, {}) -> {}:\\",
                    target_disk.number,
                    if opts.utility_use_gpt { "GPT" } else { "MBR" },
                    opts.utility_fs_type.to_uppercase(),
                    drv
                ));
                Ok(DryRunPlan {
                    summary_steps,
                    diskpart_script: dp,
                    vhdx_script: None,
                    dism_commands,
                    bcdboot_command: "(Tidak berlaku untuk format standar)".to_string(),
                    registry_tweaks: Vec::new(),
                    warnings,
                })
            }
            UtilityAction::PrepareBootableLayout => {
                let sys = letters[0];
                let win = letters[1];
                let scheme = if opts.scheme == PartitionScheme::KeepExisting {
                    PartitionScheme::MbrBiosAndUefi
                } else {
                    opts.scheme
                };
                let dp = generate_diskpart_script(
                    target_disk.number,
                    scheme,
                    if scheme.needs_separate_system_partition() { Some(sys) } else { None },
                    Some(win),
                    if scheme == PartitionScheme::MbrBiosAndUefi { 300 } else { 260 },
                    &opts.volume_label,
                )?;
                summary_steps.push(format!("Siapkan tata letak partisi bootable {} pada Disk {}", scheme.label_id(), target_disk.number));
                Ok(DryRunPlan {
                    summary_steps,
                    diskpart_script: dp,
                    vhdx_script: None,
                    dism_commands,
                    bcdboot_command: "(Siap digunakan bersama opsi Pertahankan Partisi)".to_string(),
                    registry_tweaks: Vec::new(),
                    warnings,
                })
            }
            UtilityAction::CreateStandaloneVhdx => {
                let drv = opts
                    .utility_vhdx_target_letter
                    .ok_or("Pilih huruf drive USB tujuan pembuatan berkas VHDX.")?;
                let fname = if opts.utility_vhdx_filename.trim().is_empty() {
                    "WinToGo.vhdx".to_string()
                } else {
                    opts.utility_vhdx_filename.trim().to_string()
                };
                let full_path = format!("{}:\\{}", drv.to_ascii_uppercase(), fname);
                let cfg = VhdxConfig {
                    vhd_path: full_path.clone(),
                    size_gb: opts.vhdx_size_gb,
                    expandable: opts.vhdx_dynamic,
                    label: opts.volume_label.clone(),
                };
                let vs = crate::vhdx::generate_vhdx_create_script(&cfg, letters[2])?;
                summary_steps.push(format!(
                    "Buat virtual disk '{}' ({} GB, {}) lalu detach bersih",
                    full_path,
                    opts.vhdx_size_gb,
                    if opts.vhdx_dynamic { "Dynamic" } else { "Fixed" }
                ));
                Ok(DryRunPlan {
                    summary_steps,
                    diskpart_script: "; Tidak memformat disk fisik\n".to_string(),
                    vhdx_script: Some(vs),
                    dism_commands,
                    bcdboot_command: "(Virtual disk siap pakai)".to_string(),
                    registry_tweaks: Vec::new(),
                    warnings,
                })
            }
        },
    }
}

fn log_state(state: &Arc<Mutex<DeployState>>, ctx: &egui::Context, level: LogLevel, msg: impl Into<String>) {
    if let Ok(mut st) = state.lock() {
        st.push_log(level, msg);
    }
    ctx.request_repaint();
}

fn set_step(
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    step: usize,
    total: usize,
    title: impl Into<String>,
    pct: f32,
) {
    if let Ok(mut st) = state.lock() {
        st.current_step = step;
        st.total_steps = total;
        st.step_title = title.into();
        st.progress_pct = pct.clamp(0.0, 100.0);
    }
    ctx.request_repaint();
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Proses dibatalkan oleh pengguna.".to_string())
    } else {
        Ok(())
    }
}

/// Run `dism.exe` with live stdout reading (`\r` and `\n` delimited) for real-time GUI progress.
fn run_dism_with_progress(
    args: &[String],
    base_pct: f32,
    span_pct: f32,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    log_state(state, ctx, LogLevel::Cmd, format!("dism.exe {}", args.join(" ")));

    let mut child = new_hidden_command("dism.exe")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Gagal menjalankan DISM.exe: {}", e))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or("Gagal menangkap stdout DISM.exe")?;

    let mut buf = [0u8; 512];
    let mut line_acc = String::new();
    let mut last_logged_pct: i32 = -10;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Proses DISM dibatalkan oleh pengguna.".to_string());
        }

        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                for ch in chunk.chars() {
                    if ch == '\r' || ch == '\n' {
                        let trimmed = line_acc.trim();
                        if !trimmed.is_empty() {
                            if let Some(dism_pct) = parse_dism_progress(trimmed) {
                                let overall = base_pct + (dism_pct / 100.0) * span_pct;
                                if let Ok(mut st) = state.lock() {
                                    st.progress_pct = overall.clamp(0.0, 99.0);
                                }
                                let p_int = dism_pct as i32;
                                if p_int >= last_logged_pct + 10 || p_int == 100 {
                                    last_logged_pct = p_int;
                                    log_state(
                                        state,
                                        ctx,
                                        LogLevel::Info,
                                        format!("DISM Progres: {:.1}%", dism_pct),
                                    );
                                } else {
                                    ctx.request_repaint();
                                }
                            } else if !trimmed.starts_with("Deployment Image")
                                && !trimmed.starts_with("Version:")
                            {
                                log_state(state, ctx, LogLevel::Info, trimmed);
                            }
                        }
                        line_acc.clear();
                    } else {
                        line_acc.push(ch);
                    }
                }
            }
            Err(e) => return Err(format!("Gagal membaca stream stdout DISM: {}", e)),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Gagal menunggu DISM.exe selesai: {}", e))?;

    if !output.status.success() {
        let err_s = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "DISM.exe gagal (exit {:?}): {}",
            output.status.code(),
            if err_s.is_empty() {
                "Periksa log atau ruang disk target."
            } else {
                &err_s
            }
        ));
    }

    Ok(())
}

/// RAII Guard for loading/unloading offline Windows Registry hives (`SYSTEM` and `SOFTWARE`).
struct OfflineHiveGuard {
    sys_key: String,
    sw_key: Option<String>,
}

impl OfflineHiveGuard {
    fn load(win_drive: char, load_software: bool) -> Result<Self, String> {
        let pid = std::process::id();
        let sys_key = format!("HKLM\\WTU_SYS_{}", pid);
        let sys_hive_path = format!("{}:\\Windows\\System32\\config\\SYSTEM", win_drive);
        if !Path::new(&sys_hive_path).exists() {
            return Err(format!(
                "Hive registry offline tidak ditemukan: {}",
                sys_hive_path
            ));
        }

        let out = new_hidden_command("reg.exe")
            .args(["load", &sys_key, &sys_hive_path])
            .output()
            .map_err(|e| format!("Gagal menjalankan reg load SYSTEM: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "reg load SYSTEM gagal: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let mut sw_key_opt = None;
        if load_software {
            let sw_key = format!("HKLM\\WTU_SW_{}", pid);
            let sw_hive_path = format!("{}:\\Windows\\System32\\config\\SOFTWARE", win_drive);
            if Path::new(&sw_hive_path).exists() {
                let sw_out = new_hidden_command("reg.exe")
                    .args(["load", &sw_key, &sw_hive_path])
                    .output()
                    .map_err(|e| format!("Gagal menjalankan reg load SOFTWARE: {}", e))?;
                if sw_out.status.success() {
                    sw_key_opt = Some(sw_key);
                } else {
                    let _ = new_hidden_command("reg.exe").args(["unload", &sys_key]).output();
                    return Err(format!(
                        "reg load SOFTWARE gagal: {}",
                        String::from_utf8_lossy(&sw_out.stderr).trim()
                    ));
                }
            }
        }

        Ok(Self {
            sys_key,
            sw_key: sw_key_opt,
        })
    }

    fn set_dword(&self, full_key: &str, name: &str, value: u32) -> Result<(), String> {
        let val_str = value.to_string();
        let out = new_hidden_command("reg.exe")
            .args([
                "add",
                full_key,
                "/v",
                name,
                "/t",
                "REG_DWORD",
                "/d",
                &val_str,
                "/f",
            ])
            .output()
            .map_err(|e| format!("Gagal menulis registry {}: {}", full_key, e))?;

        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "reg add {} gagal: {}",
                full_key,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn key_exists(&self, full_key: &str) -> bool {
        new_hidden_command("reg.exe")
            .args(["query", full_key])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn unload(&mut self) {
        if let Some(sw) = self.sw_key.take() {
            let _ = new_hidden_command("reg.exe").args(["unload", &sw]).output();
        }
        if !self.sys_key.is_empty() {
            let _ = new_hidden_command("reg.exe")
                .args(["unload", &self.sys_key])
                .output();
            self.sys_key.clear();
        }
    }
}

impl Drop for OfflineHiveGuard {
    fn drop(&mut self) {
        self.unload();
    }
}

fn apply_offline_registry_tweaks(
    win_drive: char,
    opts: &DeployOptions,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
) -> Result<(), String> {
    if !opts.enable_wtg_tweaks && !opts.bypass_win11_checks {
        return Ok(());
    }

    let mut guard = OfflineHiveGuard::load(win_drive, opts.bypass_win11_checks)?;

    if opts.enable_wtg_tweaks {
        let ctrl_key = format!("{}\\ControlSet001\\Control", guard.sys_key);
        guard.set_dword(&ctrl_key, "PortableOperatingSystem", 1)?;
        log_state(
            state,
            ctx,
            LogLevel::Ok,
            "Registry: PortableOperatingSystem = 1 (Windows To Go aktif).",
        );

        let pwr_key = format!("{}\\ControlSet001\\Control\\Power", guard.sys_key);
        let _ = guard.set_dword(&pwr_key, "HibernateEnabled", 0);

        for svc in [
            "USBSTOR",
            "UASPStor",
            "USBXHCI",
            "USBHUB3",
            "usbhub",
            "usbehci",
            "usbuhci",
            "usbohci",
        ] {
            let svc_key = format!("{}\\ControlSet001\\Services\\{}", guard.sys_key, svc);
            if guard.key_exists(&svc_key) {
                let _ = guard.set_dword(&svc_key, "Start", 0);
            }
        }
        log_state(
            state,
            ctx,
            LogLevel::Ok,
            "Registry: Driver USB Early-Boot (USBSTOR/UASPStor/XHCI) diatur ke Start=0.",
        );
    }

    if opts.bypass_win11_checks {
        let lab_key = format!("{}\\Setup\\LabConfig", guard.sys_key);
        for val_name in [
            "BypassTPMCheck",
            "BypassSecureBootCheck",
            "BypassRAMCheck",
            "BypassCPUCheck",
            "BypassStorageCheck",
        ] {
            guard.set_dword(&lab_key, val_name, 1)?;
        }

        if let Some(ref sw_key) = guard.sw_key {
            let oobe_key = format!(
                "{}\\Microsoft\\Windows\\CurrentVersion\\OOBE",
                sw_key
            );
            guard.set_dword(&oobe_key, "BypassNRO", 1)?;
        }
        log_state(
            state,
            ctx,
            LogLevel::Ok,
            "Registry: Bypass Windows 11 TPM/SecureBoot/RAM/CPU & BypassNRO diterapkan.",
        );
    }

    guard.unload();

    if opts.disable_winre {
        let winre_path = format!("{}:\\Windows\\System32\\Recovery\\Winre.wim", win_drive);
        if Path::new(&winre_path).exists() && fs::remove_file(&winre_path).is_ok() {
            log_state(
                state,
                ctx,
                LogLevel::Ok,
                "WinRE offline (Winre.wim) dihapus untuk menghemat ruang USB.",
            );
        }
    }

    Ok(())
}

fn inject_drivers_if_requested(
    win_drive: char,
    opts: &DeployOptions,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let Some(ref folder) = opts.driver_folder else {
        return Ok(());
    };
    let trimmed = folder.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if !Path::new(trimmed).exists() {
        return Err(format!("Folder driver tidak ditemukan: {}", trimmed));
    }

    let args = vec![
        "/English".to_string(),
        format!("/Image:{}:\\", win_drive),
        "/Add-Driver".to_string(),
        format!("/Driver:{}", trimmed),
        "/Recurse".to_string(),
    ];
    log_state(state, ctx, LogLevel::Cmd, format!("dism.exe {}", args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (code, stdout, stderr) = crate::disk::run_command_cancellable("dism.exe", &arg_refs, Some(cancel))?;
    if code != 0 {
        log_state(
            state,
            ctx,
            LogLevel::Warn,
            format!("Peringatan injeksi driver (kode {}): {}", code, stderr.trim()),
        );
    } else {
        let summary_line = stdout.lines().last().unwrap_or("Driver berhasil disuntikkan.");
        log_state(state, ctx, LogLevel::Ok, summary_line);
    }
    Ok(())
}

fn install_bootloader(
    win_drive: char,
    sys_drive: char,
    scheme: PartitionScheme,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let win_dir = format!("{}:\\Windows", win_drive);
    let args = generate_bcdboot_args(&win_dir, sys_drive, scheme)?;
    log_state(state, ctx, LogLevel::Cmd, format!("bcdboot.exe {}", args.join(" ")));

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (code, stdout, stderr) =
        crate::disk::run_command_cancellable("bcdboot.exe", &arg_refs, Some(cancel))?;

    if code != 0 {
        return Err(format!(
            "bcdboot.exe gagal (exit {}): {} {}",
            code,
            stdout.trim(),
            stderr.trim()
        ));
    }
    log_state(
        state,
        ctx,
        LogLevel::Ok,
        format!("Bootloader berhasil dipasang di {}:\\ ({}).", sys_drive, stdout.trim()),
    );
    Ok(())
}

fn run_deployment_inner(
    opts: &DeployOptions,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if !is_elevated() {
        return Err("Hak Administrator (UAC) diperlukan untuk memformat disk dan menjalankan DISM/BCDBoot.".to_string());
    }

    set_step(state, ctx, 1, 6, "Validasi target disk & prasyarat", 2.0);
    let disks = enumerate_disks()?;
    let target_disk = disks
        .iter()
        .find(|d| d.number == opts.target_disk_number)
        .ok_or_else(|| format!("Disk target #{} tidak ditemukan.", opts.target_disk_number))?;

    match opts.mode {
        DeployMode::WindowsToGo => execute_wtg_pipeline(opts, target_disk, state, ctx, cancel),
        DeployMode::LiveOsClone => execute_live_clone_pipeline(opts, target_disk, state, ctx, cancel),
        DeployMode::WindowsInstallerUsb => execute_installer_pipeline(opts, target_disk, state, ctx, cancel),
        DeployMode::DiskVhdUtility => execute_utility_pipeline(opts, target_disk, state, ctx, cancel),
    }
}

fn prepare_target_partitions(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    min_bytes: u64,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(char, char), String> {
    if opts.scheme == PartitionScheme::KeepExisting {
        if target_disk.is_system || target_disk.is_boot || target_disk.number == 0 {
            return Err("DIBLOKIR: Disk sistem tidak diizinkan menjadi target.".to_string());
        }
        let (s, w) = validate_keep_existing_letters(opts)?;
        log_state(
            state,
            ctx,
            LogLevel::Info,
            format!("Pertahankan partisi: Sistem={}:\\ | Windows={}:\\", s, w),
        );
        return Ok((s, w));
    }

    validate_target_disk(target_disk, opts.allow_non_usb_target, min_bytes)?;
    let free = find_free_drive_letters(2, None)?;
    let sys_l = free[0];
    let win_l = free[1];

    let dp_script = generate_diskpart_script(
        target_disk.number,
        opts.scheme,
        if opts.scheme.needs_separate_system_partition() {
            Some(sys_l)
        } else {
            None
        },
        Some(win_l),
        if opts.scheme == PartitionScheme::MbrBiosAndUefi {
            300
        } else {
            260
        },
        &opts.volume_label,
    )?;

    log_state(
        state,
        ctx,
        LogLevel::Cmd,
        format!("Menjalankan DiskPart pada Disk {} ({})...", target_disk.number, target_disk.friendly_name),
    );
    execute_diskpart_script(&dp_script, Some(cancel))?;
    // Allow Windows Storage Stack a moment to mount newly assigned volumes
    std::thread::sleep(Duration::from_millis(800));

    let effective_sys = if opts.scheme.needs_separate_system_partition() {
        sys_l
    } else {
        win_l
    };
    log_state(
        state,
        ctx,
        LogLevel::Ok,
        format!(
            "Partisi Disk {} selesai (Sistem: {}:\\ | Target: {}:\\).",
            target_disk.number, effective_sys, win_l
        ),
    );
    Ok((effective_sys, win_l))
}

fn execute_wtg_pipeline(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let kind = detect_source_kind(&opts.source_path)?;
    check_cancelled(cancel)?;

    // Mount ISO if needed
    let mut iso_guard = None;
    let (wim_path, is_swm) = match kind {
        SourceKind::Iso => {
            set_step(state, ctx, 2, 6, "Me-mount berkas ISO Windows", 8.0);
            let guard = mount_iso(&opts.source_path)?;
            let root = guard.drive_root();
            let (found_wim, swm) = locate_install_image(&root)?;
            log_state(state, ctx, LogLevel::Ok, format!("ISO dimount di {}. Image: {}", root, found_wim));
            iso_guard = Some(guard);
            (found_wim, swm)
        }
        SourceKind::WimEsd => (opts.source_path.clone(), false),
        SourceKind::Swm => (opts.source_path.clone(), true),
        SourceKind::VhdVhdx => (opts.source_path.clone(), false),
        SourceKind::CurrentOs => return Err("Gunakan mode Live Clone untuk OS aktif.".to_string()),
    };

    check_cancelled(cancel)?;
    set_step(state, ctx, 3, 6, "Mempersiapkan partisi disk USB target", 15.0);
    let (sys_l, outer_win_l) =
        prepare_target_partitions(opts, target_disk, MIN_WTG_DISK_BYTES, state, ctx, cancel)?;

    let mut vhdx_guard: Option<VhdxAttachGuard> = None;
    let apply_letter = if kind == SourceKind::VhdVhdx {
        set_step(state, ctx, 4, 6, "Menyalin virtual disk VHD/VHDX ke USB", 25.0);
        let dest_vhd = format!("{}:\\WinToGo.vhdx", outer_win_l);
        log_state(
            state,
            ctx,
            LogLevel::Info,
            format!("Menyalin '{}' -> '{}'...", opts.source_path, dest_vhd),
        );
        fs::copy(&opts.source_path, &dest_vhd)
            .map_err(|e| format!("Gagal menyalin berkas VHD/VHDX: {}", e))?;
        let free = find_free_drive_letters(1, None)?;
        let inner = free[0];
        let guard = VhdxAttachGuard::attach_existing_and_assign(&dest_vhd, inner, Some(cancel))?;
        // Wait for volume mount
        std::thread::sleep(Duration::from_millis(800));
        vhdx_guard = Some(guard);
        inner
    } else if opts.use_vhdx {
        check_cancelled(cancel)?;
        set_step(state, ctx, 3, 6, "Membuat & attach virtual disk .VHDX", 20.0);
        let free = find_free_drive_letters(1, None)?;
        let inner = free[0];
        let cfg = VhdxConfig {
            vhd_path: format!("{}:\\WinToGo.vhdx", outer_win_l),
            size_gb: opts.vhdx_size_gb,
            expandable: opts.vhdx_dynamic,
            label: opts.volume_label.clone(),
        };
        validate_vhdx_config(&cfg, None)?;
        let (guard, _) = VhdxAttachGuard::create_and_attach(&cfg, inner, Some(cancel))?;
        std::thread::sleep(Duration::from_millis(800));
        vhdx_guard = Some(guard);
        log_state(
            state,
            ctx,
            LogLevel::Ok,
            format!("VHDX siap di {}:\\WinToGo.vhdx (dimount sebagai {}:\\).", outer_win_l, inner),
        );
        inner
    } else {
        outer_win_l
    };

    if kind != SourceKind::VhdVhdx {
        check_cancelled(cancel)?;
        set_step(state, ctx, 4, 6, "Menerapkan image Windows (DISM /Apply-Image)", 22.0);
        let apply_args = generate_dism_apply_args(
            &wim_path,
            opts.wim_index,
            &format!("{}:\\", apply_letter),
            is_swm,
        );
        run_dism_with_progress(&apply_args, 22.0, 62.0, state, ctx, cancel)?;
        log_state(state, ctx, LogLevel::Ok, "Image Windows berhasil diterapkan.");
    }

    check_cancelled(cancel)?;
    set_step(state, ctx, 5, 6, "Konfigurasi Registry Windows To Go & Driver", 86.0);
    inject_drivers_if_requested(apply_letter, opts, state, ctx, cancel)?;
    apply_offline_registry_tweaks(apply_letter, opts, state, ctx)?;

    check_cancelled(cancel)?;
    set_step(state, ctx, 6, 6, "Memasang bootloader Windows (BCDBoot)", 92.0);
    install_bootloader(apply_letter, sys_l, opts.scheme, state, ctx, cancel)?;

    if let Some(mut g) = vhdx_guard {
        let _ = g.detach();
        log_state(state, ctx, LogLevel::Info, "VHDX dilepas (detach) secara bersih.");
    }
    if let Some(mut ig) = iso_guard {
        let _ = ig.dismount();
        log_state(state, ctx, LogLevel::Info, "ISO dilepas (dismount) secara bersih.");
    }

    set_step(state, ctx, 6, 6, "Selesai! Windows To Go siap digunakan.", 100.0);
    Ok(())
}

fn choose_best_temp_wim_path(estimated_needed_bytes: u64) -> PathBuf {
    let fname = format!("WinToUSB_LiveClone_{}.wim", std::process::id());
    let default_tmp = std::env::temp_dir().join(&fname);

    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
        };
        const DRIVE_FIXED: u32 = 3;

        let check_free = |dir_or_root: &Path| -> u64 {
            let s = dir_or_root.to_string_lossy();
            let root_str = if s.len() >= 2 && s.as_bytes()[1] == b':' {
                format!("{}:\\", s.chars().next().unwrap_or('C'))
            } else {
                s.to_string()
            };
            let wide: Vec<u16> = format!("{}\0", root_str).encode_utf16().collect();
            let mut avail = 0u64;
            let mut total = 0u64;
            let mut free_total = 0u64;
            if unsafe {
                GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free_total)
            } != 0
            {
                avail
            } else {
                0
            }
        };

        if check_free(&std::env::temp_dir()) >= estimated_needed_bytes {
            return default_tmp;
        }

        let mask = unsafe { GetLogicalDrives() };
        let mut best_drive: Option<(char, u64)> = None;
        for idx in 2u8..=25u8 {
            if (mask & (1u32 << idx)) != 0 {
                let ch = (b'A' + idx) as char;
                let root: Vec<u16> = format!("{}:\\\0", ch).encode_utf16().collect();
                if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED {
                    let free = check_free(Path::new(&format!("{}:\\", ch)));
                    if best_drive.map(|(_, b)| free > b).unwrap_or(true) {
                        best_drive = Some((ch, free));
                    }
                }
            }
        }

        if let Some((ch, free)) = best_drive {
            if free > check_free(&std::env::temp_dir()) {
                return PathBuf::from(format!("{}:\\{}", ch, fname));
            }
        }
    }

    let _ = estimated_needed_bytes;
    default_tmp
}

fn execute_live_clone_pipeline(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let live_info = query_live_os_info()?;
    let min_bytes = live_info.recommended_min_usb_bytes();

    check_cancelled(cancel)?;
    set_step(state, ctx, 2, 6, "Membuat Volume Shadow Copy (VSS) dari C:\\", 8.0);
    let mut vss_guard = VssShadowGuard::create_for_volume(&live_info.system_drive)?;
    log_state(
        state,
        ctx,
        LogLevel::Ok,
        format!(
            "Snapshot VSS dibuat: {} ({}) -> {:?}",
            vss_guard.shadow_id, vss_guard.device_object, vss_guard.mount_link
        ),
    );

    let cfg_path = temp_script_path("wintousb_excl");
    fs::write(&cfg_path, generate_wimscript_ini())
        .map_err(|e| format!("Gagal menulis WimScript.ini: {}", e))?;
    // Compressed fast WIM is typically ~50-60% of used volume size
    let est_wim = (live_info.used_bytes / 2).max(4 * 1024 * 1024 * 1024);
    let wim_temp_path = choose_best_temp_wim_path(est_wim);

    let cfg_str = cfg_path.to_string_lossy().to_string();
    let wim_str = wim_temp_path.to_string_lossy().to_string();
    let link_str = vss_guard.mount_link.to_string_lossy().to_string();

    set_step(state, ctx, 3, 6, "Menangkap snapshot OS aktif (DISM /Capture-Image)", 14.0);
    let cap_args = build_vss_capture_dism_args(&wim_str, &link_str, Some(&cfg_str));
    let cap_res = run_dism_with_progress(&cap_args, 14.0, 36.0, state, ctx, cancel);

    // Clean up VSS & exclusion file immediately once capture finishes or errors
    let _ = fs::remove_file(&cfg_path);
    let _ = vss_guard.cleanup();

    if let Err(e) = cap_res {
        let _ = fs::remove_file(&wim_temp_path);
        return Err(e);
    }

    check_cancelled(cancel)?;
    set_step(state, ctx, 4, 6, "Mempartisi & menerapkan kloning OS ke USB", 52.0);
    let prep_res = prepare_target_partitions(opts, target_disk, min_bytes, state, ctx, cancel);
    let (sys_l, outer_win_l) = match prep_res {
        Ok(pair) => pair,
        Err(e) => {
            let _ = fs::remove_file(&wim_temp_path);
            return Err(e);
        }
    };

    let mut vhdx_guard: Option<VhdxAttachGuard> = None;
    let apply_letter = if opts.use_vhdx {
        let free = find_free_drive_letters(1, None)?;
        let inner = free[0];
        let cfg = VhdxConfig {
            vhd_path: format!("{}:\\WinToGo.vhdx", outer_win_l),
            size_gb: opts.vhdx_size_gb,
            expandable: opts.vhdx_dynamic,
            label: opts.volume_label.clone(),
        };
        let (guard, _) = VhdxAttachGuard::create_and_attach(&cfg, inner, Some(cancel))?;
        std::thread::sleep(Duration::from_millis(800));
        vhdx_guard = Some(guard);
        inner
    } else {
        outer_win_l
    };

    let apply_args = generate_dism_apply_args(&wim_str, 1, &format!("{}:\\", apply_letter), false);
    let apply_res = run_dism_with_progress(&apply_args, 55.0, 33.0, state, ctx, cancel);
    let _ = fs::remove_file(&wim_temp_path);
    apply_res?;

    check_cancelled(cancel)?;
    set_step(state, ctx, 5, 6, "Konfigurasi Registry Windows To Go", 90.0);
    inject_drivers_if_requested(apply_letter, opts, state, ctx, cancel)?;
    apply_offline_registry_tweaks(apply_letter, opts, state, ctx)?;

    check_cancelled(cancel)?;
    set_step(state, ctx, 6, 6, "Memasang bootloader Windows (BCDBoot)", 95.0);
    install_bootloader(apply_letter, sys_l, opts.scheme, state, ctx, cancel)?;

    if let Some(mut g) = vhdx_guard {
        let _ = g.detach();
    }

    set_step(state, ctx, 6, 6, "Selesai! Kloning Windows OS siap booting.", 100.0);
    Ok(())
}

fn execute_installer_pipeline(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if detect_source_kind(&opts.source_path)? != SourceKind::Iso {
        return Err("Mode Installer USB memerlukan sumber berkas .iso Windows.".to_string());
    }

    set_step(state, ctx, 2, 5, "Me-mount ISO Windows Setup", 10.0);
    let mut iso_guard = mount_iso(&opts.source_path)?;
    let iso_root = iso_guard.drive_root();
    log_state(state, ctx, LogLevel::Ok, format!("ISO dimount di {}", iso_root));

    check_cancelled(cancel)?;
    set_step(state, ctx, 3, 5, "Memformat USB ke FAT32 bootable", 20.0);
    validate_target_disk(target_disk, opts.allow_non_usb_target, MIN_INSTALLER_DISK_BYTES)?;
    let free = find_free_drive_letters(1, None)?;
    let usb_letter = free[0];
    let dp_script = generate_single_fs_script(
        target_disk.number,
        opts.scheme == PartitionScheme::GptUefi,
        "fat32",
        usb_letter,
        &opts.volume_label,
        target_disk.size_bytes,
    )?;
    execute_diskpart_script(&dp_script, Some(cancel))?;
    std::thread::sleep(Duration::from_millis(800));

    let usb_root = format!("{}:\\", usb_letter);
    let wim_candidate = Path::new(&iso_root).join("sources\\install.wim");
    let esd_candidate = Path::new(&iso_root).join("sources\\install.esd");
    let big_wim = if wim_candidate.exists()
        && fs::metadata(&wim_candidate).map(|m| m.len()).unwrap_or(0) > 4 * 1024 * 1024 * 1024
    {
        Some(wim_candidate)
    } else if esd_candidate.exists()
        && fs::metadata(&esd_candidate).map(|m| m.len()).unwrap_or(0) > 4 * 1024 * 1024 * 1024
    {
        Some(esd_candidate)
    } else {
        None
    };

    set_step(state, ctx, 4, 5, "Menyalin berkas installer Windows ke USB", 35.0);
    let mut robo_args = vec![
        iso_root.trim_end_matches('\\').to_string(),
        usb_root.trim_end_matches('\\').to_string(),
        "/E".to_string(),
        "/NFL".to_string(),
        "/NDL".to_string(),
        "/NJH".to_string(),
        "/NJS".to_string(),
    ];
    if let Some(ref big) = big_wim {
        if let Some(name) = big.file_name().and_then(|n| n.to_str()) {
            robo_args.push("/XF".to_string());
            robo_args.push(name.to_string());
        }
    }
    log_state(state, ctx, LogLevel::Cmd, format!("robocopy {}", robo_args.join(" ")));
    let robo_refs: Vec<&str> = robo_args.iter().map(|s| s.as_str()).collect();
    let (r_code, _, r_err) =
        crate::disk::run_command_cancellable("robocopy.exe", &robo_refs, Some(cancel))?;
    // Robocopy exit codes 0..=7 indicate success with varying copy counts
    if r_code >= 8 {
        return Err(format!("Robocopy gagal (exit {}): {}", r_code, r_err.trim()));
    }

    if let Some(big) = big_wim {
        check_cancelled(cancel)?;
        let is_esd = big
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("esd"))
            .unwrap_or(false);

        let wim_to_split = if is_esd {
            set_step(
                state,
                ctx,
                4,
                5,
                "Mengonversi install.esd > 4 GB menjadi WIM sebelum dipecah",
                58.0,
            );
            let tmp_wim = std::env::temp_dir().join(format!(
                "WinToUSB_Export_{}.wim",
                std::process::id()
            ));
            let tmp_str = tmp_wim.to_string_lossy().to_string();
            let export_args = vec![
                "/English".to_string(),
                "/Export-Image".to_string(),
                format!("/SourceImageFile:{}", big.to_string_lossy()),
                "/All".to_string(),
                format!("/DestinationImageFile:{}", tmp_str),
                "/Compress:fast".to_string(),
            ];
            run_dism_with_progress(&export_args, 58.0, 12.0, state, ctx, cancel)?;
            Some(tmp_wim)
        } else {
            None
        };

        let split_source = wim_to_split
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| big.to_string_lossy().to_string());

        set_step(
            state,
            ctx,
            4,
            5,
            "Memecah install.wim > 4 GB menjadi install*.swm untuk FAT32",
            70.0,
        );
        let swm_out = format!("{}:\\sources\\install.swm", usb_letter);
        let split_args = vec![
            "/English".to_string(),
            "/Split-Image".to_string(),
            format!("/ImageFile:{}", split_source),
            format!("/SWMFile:{}", swm_out),
            "/FileSize:3800".to_string(),
        ];
        let split_res = run_dism_with_progress(&split_args, 70.0, 24.0, state, ctx, cancel);
        if let Some(ref tmp) = wim_to_split {
            let _ = fs::remove_file(tmp);
        }
        split_res?;
    }

    if opts.bypass_win11_checks {
        set_step(state, ctx, 5, 5, "Menulis autounattend.xml Bypass Win11", 95.0);
        let unattend_path = format!("{}:\\autounattend.xml", usb_letter);
        fs::write(&unattend_path, generate_installer_bypass_autounattend())
            .map_err(|e| format!("Gagal menulis autounattend.xml: {}", e))?;
        log_state(state, ctx, LogLevel::Ok, "autounattend.xml Bypass Win11 ditulis.");
    }

    let _ = iso_guard.dismount();
    set_step(state, ctx, 5, 5, "Selesai! USB Installer Windows siap dipakai.", 100.0);
    Ok(())
}

fn execute_utility_pipeline(
    opts: &DeployOptions,
    target_disk: &DiskInfo,
    state: &Arc<Mutex<DeployState>>,
    ctx: &egui::Context,
    cancel: &AtomicBool,
) -> Result<(), String> {
    match opts.utility_action {
        UtilityAction::FormatSinglePartition => {
            set_step(state, ctx, 2, 2, "Memformat disk USB target", 40.0);
            validate_target_disk(target_disk, opts.allow_non_usb_target, 512 * 1024 * 1024)?;
            let free = find_free_drive_letters(1, None)?;
            let script = generate_single_fs_script(
                target_disk.number,
                opts.utility_use_gpt,
                &opts.utility_fs_type,
                free[0],
                &opts.volume_label,
                target_disk.size_bytes,
            )?;
            execute_diskpart_script(&script, Some(cancel))?;
            set_step(
                state,
                ctx,
                2,
                2,
                format!("Selesai! Disk {} berhasil diformat ({}:\\).", target_disk.number, free[0]),
                100.0,
            );
            Ok(())
        }
        UtilityAction::PrepareBootableLayout => {
            set_step(state, ctx, 2, 2, "Membuat partisi bootable pada USB", 40.0);
            let mut local_opts = opts.clone();
            if local_opts.scheme == PartitionScheme::KeepExisting {
                local_opts.scheme = PartitionScheme::MbrBiosAndUefi;
            }
            let (s, w) = prepare_target_partitions(
                &local_opts,
                target_disk,
                MIN_INSTALLER_DISK_BYTES,
                state,
                ctx,
                cancel,
            )?;
            set_step(
                state,
                ctx,
                2,
                2,
                format!("Selesai! Partisi Sistem {}:\\ & Data {}:\\ siap.", s, w),
                100.0,
            );
            Ok(())
        }
        UtilityAction::CreateStandaloneVhdx => {
            set_step(state, ctx, 2, 2, "Membuat berkas virtual disk .VHDX", 40.0);
            let drv = opts
                .utility_vhdx_target_letter
                .ok_or("Pilih huruf drive target untuk menyimpan berkas .VHDX.")?;
            let fname = if opts.utility_vhdx_filename.trim().is_empty() {
                "WinToGo.vhdx".to_string()
            } else {
                opts.utility_vhdx_filename.trim().to_string()
            };
            let vhd_path = format!("{}:\\{}", drv.to_ascii_uppercase(), fname);
            let cfg = VhdxConfig {
                vhd_path: vhd_path.clone(),
                size_gb: opts.vhdx_size_gb,
                expandable: opts.vhdx_dynamic,
                label: opts.volume_label.clone(),
            };
            validate_vhdx_config(&cfg, None)?;
            let free = find_free_drive_letters(1, None)?;
            let (mut guard, _) = VhdxAttachGuard::create_and_attach(&cfg, free[0], Some(cancel))?;
            let _ = guard.detach();
            set_step(
                state,
                ctx,
                2,
                2,
                format!("Selesai! Virtual disk '{}' berhasil dibuat.", vhd_path),
                100.0,
            );
            Ok(())
        }
    }
}

/// Spawn non-blocking background deployment thread wired to egui repaint updates.
pub fn start_deployment(
    opts: DeployOptions,
    state: Arc<Mutex<DeployState>>,
    ctx: egui::Context,
) {
    {
        if let Ok(mut st) = state.lock() {
            if st.running {
                return;
            }
            st.running = true;
            st.finished = false;
            st.success = false;
            st.current_step = 0;
            st.total_steps = 6;
            st.step_title = "Memulai proses...".to_string();
            st.progress_pct = 0.0;
            st.logs.clear();
            st.started_at = Some(Instant::now());
            st.finished_elapsed_secs = None;
            st.error_message = None;
            st.cancel_flag.store(false, Ordering::Relaxed);
            st.push_log(LogLevel::Info, format!("Memulai tugas: {}", opts.mode.title_id()));
        }
    }

    let cancel = {
        state
            .lock()
            .map(|s| s.cancel_flag.clone())
            .unwrap_or_else(|_| Arc::new(AtomicBool::new(false)))
    };

    std::thread::spawn(move || {
        let result = run_deployment_inner(&opts, &state, &ctx, &cancel);

        if let Ok(mut st) = state.lock() {
            let elapsed = st.started_at.map(|s| s.elapsed().as_secs()).unwrap_or(0);
            st.finished_elapsed_secs = Some(elapsed);
            st.running = false;
            st.finished = true;
            match result {
                Ok(()) => {
                    st.success = true;
                    st.progress_pct = 100.0;
                    st.push_log(
                        LogLevel::Ok,
                        format!("Semua proses selesai sukses dalam {} detik!", elapsed),
                    );
                }
                Err(err) => {
                    st.success = false;
                    st.error_message = Some(err.clone());
                    st.step_title = "Proses berhenti / gagal".to_string();
                    st.push_log(LogLevel::Error, err);
                }
            }
        }
        ctx.request_repaint();
    });
}
