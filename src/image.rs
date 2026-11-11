use std::fs;
use std::path::Path;

use crate::disk::{format_bytes_human, new_hidden_command, run_powershell_hidden};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Iso,
    WimEsd,
    Swm,
    VhdVhdx,
    CurrentOs,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Iso => "Windows ISO Image (.iso)",
            Self::WimEsd => "Windows WIM / ESD Image",
            Self::Swm => "Split Windows Image (.swm)",
            Self::VhdVhdx => "Virtual Disk Image (.vhd / .vhdx)",
            Self::CurrentOs => "Windows OS Aktif (Live Clone)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WimEditionInfo {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub size_bytes: u64,
    pub size_display: String,
    pub architecture: String,
    pub version: String,
    pub is_windows_11: bool,
}

#[derive(Debug, Clone)]
pub struct ImageInspectionResult {
    pub source_path: String,
    pub source_kind: SourceKind,
    pub is_split_swm: bool,
    pub image_file_size_bytes: u64,
    pub editions: Vec<WimEditionInfo>,
    pub notes: Vec<String>,
}

/// RAII wrapper around a PowerShell-mounted ISO disk image.
/// Automatically dismounts the ISO when dropped so failed/cancelled deployments never leak mounts.
pub struct MountedIsoGuard {
    pub iso_path: String,
    pub drive_letter: char,
    dismounted: bool,
}

impl MountedIsoGuard {
    pub fn drive_root(&self) -> String {
        format!("{}:\\", self.drive_letter)
    }

    pub fn dismount(&mut self) -> Result<(), String> {
        if self.dismounted {
            return Ok(());
        }
        let escaped = ps_single_quote(&self.iso_path);
        let script = format!(
            "Dismount-DiskImage -ImagePath '{}' -ErrorAction SilentlyContinue | Out-Null",
            escaped
        );
        self.dismounted = true;
        run_powershell_hidden(&script).map(|_| ())
    }
}

impl Drop for MountedIsoGuard {
    fn drop(&mut self) {
        if !self.dismounted {
            let _ = self.dismount();
        }
    }
}

pub(crate) fn ps_single_quote(input: &str) -> String {
    input.replace('\'', "''")
}

pub fn detect_source_kind(path: &str) -> Result<SourceKind, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "iso" => Ok(SourceKind::Iso),
        "wim" | "esd" => Ok(SourceKind::WimEsd),
        "swm" => Ok(SourceKind::Swm),
        "vhd" | "vhdx" => Ok(SourceKind::VhdVhdx),
        _ => Err(format!(
            "Format berkas tidak didukung ('{}'). Pilih berkas .iso, .wim, .esd, .swm, .vhd, atau .vhdx.",
            ext
        )),
    }
}

pub fn mount_iso(iso_path: &str) -> Result<MountedIsoGuard, String> {
    if !Path::new(iso_path).exists() {
        return Err(format!("Berkas ISO tidak ditemukan: {}", iso_path));
    }
    let escaped = ps_single_quote(iso_path);
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$img = Mount-DiskImage -ImagePath '{iso}' -PassThru
$vol = $img | Get-Volume | Select-Object -First 1
if (-not $vol -or -not $vol.DriveLetter) {{
    Start-Sleep -Milliseconds 500
    $vol = Get-DiskImage -ImagePath '{iso}' | Get-Volume | Select-Object -First 1
}}
if (-not $vol -or -not $vol.DriveLetter) {{
    Dismount-DiskImage -ImagePath '{iso}' -ErrorAction SilentlyContinue | Out-Null
    throw "ISO berhasil dimount tetapi huruf drive tidak tersedia."
}}
[string]$vol.DriveLetter
"#,
        iso = escaped
    );

    let out = run_powershell_hidden(&script)?;
    let ch = out
        .trim()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .ok_or_else(|| format!("Output huruf drive ISO tidak valid: '{}'", out))?
        .to_ascii_uppercase();

    Ok(MountedIsoGuard {
        iso_path: iso_path.to_string(),
        drive_letter: ch,
        dismounted: false,
    })
}

/// Locate Windows installation image inside a mounted ISO root (e.g. `E:\`).
/// Returns `(image_path, is_split_swm)`.
pub fn locate_install_image(drive_root: &str) -> Result<(String, bool), String> {
    let root = Path::new(drive_root);
    let candidates = [
        ("sources\\install.wim", false),
        ("sources\\install.esd", false),
        ("sources\\install.swm", true),
        ("x64\\sources\\install.wim", false),
        ("x64\\sources\\install.esd", false),
        ("x86\\sources\\install.wim", false),
        ("x86\\sources\\install.esd", false),
    ];

    for (rel, is_swm) in candidates {
        let full = root.join(rel);
        if full.exists() {
            return Ok((full.to_string_lossy().to_string(), is_swm));
        }
    }

    Err(format!(
        "Tidak ditemukan 'sources\\install.wim', 'install.esd', atau 'install.swm' di dalam ISO ({}).",
        drive_root
    ))
}

fn parse_byte_count(raw: &str) -> u64 {
    // Handles "16,423,912,411 bytes", "16.423.912.411 byte", etc.
    let before_unit = raw.split_whitespace().next().unwrap_or(raw);
    let digits: String = before_unit.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().unwrap_or(0)
}

fn detect_is_windows_11(name: &str, version: &str) -> bool {
    if name.to_ascii_lowercase().contains("windows 11") {
        return true;
    }
    if let Some(build_part) = version.strip_prefix("10.0.") {
        if let Some(build_str) = build_part.split('.').next() {
            if let Ok(build) = build_str.parse::<u32>() {
                return build >= 22000;
            }
        }
    }
    false
}

/// Parse `DISM.exe /English /Get-WimInfo /WimFile:"..."` stdout into structured editions.
pub fn parse_dism_wim_info(output: &str) -> Vec<WimEditionInfo> {
    let mut editions = Vec::new();
    let mut current_index: Option<u32> = None;
    let mut current_name = String::new();
    let mut current_desc = String::new();
    let mut current_size: u64 = 0;
    let mut current_arch = String::new();
    let mut current_ver = String::new();

    let flush_current = |idx_opt: &mut Option<u32>,
                         name: &mut String,
                         desc: &mut String,
                         size: &mut u64,
                         arch: &mut String,
                         ver: &mut String,
                         list: &mut Vec<WimEditionInfo>| {
        if let Some(idx) = idx_opt.take() {
            let final_name = if name.is_empty() {
                format!("Windows Edition #{}", idx)
            } else {
                name.clone()
            };
            let is_w11 = detect_is_windows_11(&final_name, ver);
            list.push(WimEditionInfo {
                index: idx,
                name: final_name,
                description: desc.clone(),
                size_bytes: *size,
                size_display: format_bytes_human(*size),
                architecture: if arch.is_empty() {
                    "Unknown".to_string()
                } else {
                    arch.clone()
                },
                version: ver.clone(),
                is_windows_11: is_w11,
            });
            name.clear();
            desc.clear();
            *size = 0;
            arch.clear();
            ver.clear();
        }
    };

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k_raw, v_raw)) = line.split_once(':') {
            let key = k_raw.trim().to_ascii_lowercase();
            let val = v_raw.trim();

            if key == "index" || key == "indeks" {
                flush_current(
                    &mut current_index,
                    &mut current_name,
                    &mut current_desc,
                    &mut current_size,
                    &mut current_arch,
                    &mut current_ver,
                    &mut editions,
                );
                if let Ok(idx) = val.parse::<u32>() {
                    current_index = Some(idx);
                }
            } else if key == "name" || key == "nama" {
                current_name = val.to_string();
            } else if key == "description" || key == "deskripsi" {
                current_desc = val.to_string();
            } else if key == "size" || key == "ukuran" {
                current_size = parse_byte_count(val);
            } else if key == "architecture" || key == "arsitektur" {
                current_arch = val.to_string();
            } else if key == "version" || key == "versi" {
                current_ver = val.to_string();
            }
        }
    }

    flush_current(
        &mut current_index,
        &mut current_name,
        &mut current_desc,
        &mut current_size,
        &mut current_arch,
        &mut current_ver,
        &mut editions,
    );

    editions
}

/// Extract detailed metadata (Architecture, Version) from `DISM /Get-WimInfo /Index:N`.
pub fn apply_dism_index_detail(output: &str, edition: &mut WimEditionInfo) {
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if let Some((k_raw, v_raw)) = line.split_once(':') {
            let key = k_raw.trim().to_ascii_lowercase();
            let val = v_raw.trim();
            if key == "architecture" || key == "arsitektur" {
                edition.architecture = val.to_string();
            } else if key == "version" || key == "versi" {
                edition.version = val.to_string();
            } else if (key == "servicepack build" || key == "service pack build")
                && !edition.version.is_empty()
                && edition.version.split('.').count() == 3
            {
                edition.version = format!("{}.{}", edition.version, val);
            }
        }
    }
    edition.is_windows_11 = detect_is_windows_11(&edition.name, &edition.version);
}

/// Parse progress percentage from a DISM stdout line such as `[=== 42.5% ===]` or `100.0%`.
pub fn parse_dism_progress(line: &str) -> Option<f32> {
    let pct_pos = line.find('%')?;
    let prefix = &line[..pct_pos];
    let num_str: String = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();

    if num_str.is_empty() {
        return None;
    }
    let normalized = num_str.replace(',', ".");
    let parsed = normalized.parse::<f32>().ok()?;
    Some(parsed.clamp(0.0, 100.0))
}

fn inspect_wim_file(wim_path: &str) -> Result<Vec<WimEditionInfo>, String> {
    let out = new_hidden_command("dism.exe")
        .args([
            "/English",
            "/Get-WimInfo",
            &format!("/WimFile:{}", wim_path),
        ])
        .output()
        .map_err(|e| format!("Gagal menjalankan DISM.exe: {}", e))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        return Err(format!(
            "DISM /Get-WimInfo gagal (exit {:?}): {}",
            out.status.code(),
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        ));
    }

    let mut editions = parse_dism_wim_info(&stdout);
    if editions.is_empty() {
        return Err(format!(
            "DISM tidak menemukan edisi Windows di dalam '{}'.",
            wim_path
        ));
    }

    // Fetch Architecture & Version for up to first 12 editions quickly
    for ed in editions.iter_mut().take(12) {
        if let Ok(detail_out) = new_hidden_command("dism.exe")
            .args([
                "/English",
                "/Get-WimInfo",
                &format!("/WimFile:{}", wim_path),
                &format!("/Index:{}", ed.index),
            ])
            .output()
        {
            if detail_out.status.success() {
                let d_stdout = String::from_utf8_lossy(&detail_out.stdout);
                apply_dism_index_detail(&d_stdout, ed);
            }
        }
    }

    Ok(editions)
}

/// Inspect `.iso`, `.wim`, `.esd`, `.swm`, or `.vhd`/`.vhdx` without permanently mounting or mutating state.
pub fn inspect_image_source(path: &str) -> Result<ImageInspectionResult, String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("Berkas sumber tidak ditemukan: {}", path));
    }

    let kind = detect_source_kind(path)?;
    let file_size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut notes = Vec::new();

    match kind {
        SourceKind::Iso => {
            let guard = mount_iso(path)?;
            let root = guard.drive_root();
            let (wim_path, is_swm) = locate_install_image(&root)?;
            let wim_size = fs::metadata(&wim_path).map(|m| m.len()).unwrap_or(0);
            if wim_size > 4 * 1024 * 1024 * 1024 {
                notes.push(format!(
                    "Ukuran image instalasi ({}) melebihi batas 4 GB FAT32; mode Windows Setup USB akan membagi otomatis ke .swm.",
                    format_bytes_human(wim_size)
                ));
            }
            let editions = inspect_wim_file(&wim_path)?;
            if editions.iter().any(|e| e.is_windows_11) {
                notes.push(
                    "Terdeteksi Windows 11 — Anda dapat mengaktifkan opsi Bypass TPM 2.0 / SecureBoot / RAM Check."
                        .to_string(),
                );
            }
            Ok(ImageInspectionResult {
                source_path: path.to_string(),
                source_kind: kind,
                is_split_swm: is_swm,
                image_file_size_bytes: file_size,
                editions,
                notes,
            })
        }
        SourceKind::WimEsd | SourceKind::Swm => {
            let editions = inspect_wim_file(path)?;
            if editions.iter().any(|e| e.is_windows_11) {
                notes.push(
                    "Terdeteksi Windows 11 — Anda dapat mengaktifkan opsi Bypass TPM 2.0 / SecureBoot / RAM Check."
                        .to_string(),
                );
            }
            Ok(ImageInspectionResult {
                source_path: path.to_string(),
                source_kind: kind,
                is_split_swm: kind == SourceKind::Swm,
                image_file_size_bytes: file_size,
                editions,
                notes,
            })
        }
        SourceKind::VhdVhdx => {
            notes.push(
                "Mode VHD/VHDX akan menyalin image virtual disk secara langsung dan mengonfigurasi boot loader Windows."
                    .to_string(),
            );
            let editions = vec![WimEditionInfo {
                index: 1,
                name: format!(
                    "Virtual Disk Image ({})",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ),
                description: "Deploy langsung berkas VHD/VHDX ke drive USB target".to_string(),
                size_bytes: file_size,
                size_display: format_bytes_human(file_size),
                architecture: "x64/Native".to_string(),
                version: "VHD/VHDX".to_string(),
                is_windows_11: false,
            }];
            Ok(ImageInspectionResult {
                source_path: path.to_string(),
                source_kind: kind,
                is_split_swm: false,
                image_file_size_bytes: file_size,
                editions,
                notes,
            })
        }
        SourceKind::CurrentOs => Err("Gunakan inspeksi OS aktif untuk mode Live Clone.".to_string()),
    }
}

/// Native Win32 file picker (`GetOpenFileNameW`) with zero PowerShell flash.
pub fn pick_image_file() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Controls::Dialogs::{
            GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
        };

        let mut file_buf = vec![0u16; 4096];
        let filter: Vec<u16> = "Windows Images (*.iso;*.wim;*.esd;*.swm;*.vhd;*.vhdx)\0*.iso;*.wim;*.esd;*.swm;*.vhd;*.vhdx\0ISO Images (*.iso)\0*.iso\0WIM / ESD / SWM (*.wim;*.esd;*.swm)\0*.wim;*.esd;*.swm\0Virtual Disk (*.vhd;*.vhdx)\0*.vhd;*.vhdx\0All Files (*.*)\0*.*\0\0"
            .encode_utf16()
            .collect();
        let title: Vec<u16> = "Pilih Berkas Image Windows (ISO / WIM / ESD / VHDX)\0"
            .encode_utf16()
            .collect();

        unsafe {
            let mut ofn: OPENFILENAMEW = std::mem::zeroed();
            ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
            ofn.lpstrFilter = filter.as_ptr();
            ofn.nFilterIndex = 1;
            ofn.lpstrFile = file_buf.as_mut_ptr();
            ofn.nMaxFile = file_buf.len() as u32;
            ofn.lpstrTitle = title.as_ptr();
            ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

            if GetOpenFileNameW(&mut ofn) != 0 {
                let len = file_buf
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(file_buf.len());
                let selected = String::from_utf16_lossy(&file_buf[..len]);
                if !selected.is_empty() {
                    return Some(selected);
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Native Win32 folder picker (`SHBrowseForFolderW`) for driver injection directory.
pub fn pick_folder() -> Option<String> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
        use windows_sys::Win32::System::Com::CoTaskMemFree;

        #[repr(C)]
        #[allow(non_snake_case)]
        struct BrowseInfoW {
            hwnd_owner: HWND,
            pidl_root: *const c_void,
            psz_display_name: *mut u16,
            lpsz_title: *const u16,
            ul_flags: u32,
            lpfn: *const c_void,
            l_param: LPARAM,
            i_image: i32,
        }

        #[link(name = "shell32")]
        extern "system" {
            fn SHBrowseForFolderW(lpbi: *mut BrowseInfoW) -> *mut c_void;
            fn SHGetPathFromIDListW(pidl: *const c_void, psz_path: *mut u16) -> BOOL;
        }

        const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
        const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;

        let title: Vec<u16> = "Pilih Folder Driver Windows (.inf)\0".encode_utf16().collect();
        let mut display_name = vec![0u16; 260];
        let mut path_buf = vec![0u16; 4096];

        unsafe {
            let mut bi = BrowseInfoW {
                hwnd_owner: 0,
                pidl_root: std::ptr::null(),
                psz_display_name: display_name.as_mut_ptr(),
                lpsz_title: title.as_ptr(),
                ul_flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
                lpfn: std::ptr::null(),
                l_param: 0,
                i_image: 0,
            };

            let pidl = SHBrowseForFolderW(&mut bi);
            if pidl.is_null() {
                return None;
            }
            let ok = SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr());
            CoTaskMemFree(pidl);

            if ok != 0 {
                let len = path_buf
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(path_buf.len());
                let path = String::from_utf16_lossy(&path_buf[..len]);
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}
