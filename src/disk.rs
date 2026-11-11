use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const MIN_WTG_DISK_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB
pub const MIN_INSTALLER_DISK_BYTES: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PartitionScheme {
    GptUefi,
    MbrBios,
    MbrBiosAndUefi,
    KeepExisting,
}

impl PartitionScheme {
    pub fn label_id(&self) -> &'static str {
        match self {
            Self::GptUefi => "GPT untuk UEFI (Direkomendasikan)",
            Self::MbrBios => "MBR untuk BIOS (Legacy)",
            Self::MbrBiosAndUefi => "MBR untuk BIOS & UEFI (Dual Boot)",
            Self::KeepExisting => "Pertahankan Partisi yang Ada (Tanpa Format Disk)",
        }
    }

    pub fn description_id(&self) -> &'static str {
        match self {
            Self::GptUefi => "Membuat partisi EFI FAT32 (260 MB) + partisi Windows NTFS. Cocok untuk laptop/PC UEFI modern & Mac.",
            Self::MbrBios => "Membuat 1 partisi NTFS aktif (MBR). Cocok untuk komputer Legacy BIOS lama.",
            Self::MbrBiosAndUefi => "Membuat partisi Boot/EFI FAT32 aktif (300 MB) + partisi Windows NTFS di MBR. Bisa booting di PC BIOS lama maupun UEFI modern!",
            Self::KeepExisting => "Tidak menghapus isi disk! Langsung men-deploy ke huruf partisi Sistem & Windows yang sudah Anda siapkan.",
        }
    }

    pub fn is_destructive(&self) -> bool {
        !matches!(self, Self::KeepExisting)
    }

    pub fn needs_separate_system_partition(&self) -> bool {
        matches!(self, Self::GptUefi | Self::MbrBiosAndUefi | Self::KeepExisting)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionInfo {
    pub disk_number: u32,
    pub partition_number: u32,
    pub drive_letter: Option<char>,
    pub size_bytes: u64,
    pub part_type: String,
    pub file_system: String,
    pub label: String,
    pub is_boot: bool,
    pub is_system: bool,
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub number: u32,
    pub friendly_name: String,
    pub bus_type: String,
    pub size_bytes: u64,
    pub partition_style: String,
    pub is_system: bool,
    pub is_boot: bool,
    pub is_offline: bool,
    pub is_read_only: bool,
    pub serial_number: String,
    pub partitions: Vec<PartitionInfo>,
}

impl DiskInfo {
    pub fn size_display(&self) -> String {
        format_bytes_human(self.size_bytes)
    }

    /// Strict safety predicate for automatic USB targets.
    /// Rejects Disk 0, system/boot disks, read-only disks, non-USB buses,
    /// and any disk holding C:\, boot, or system partitions.
    pub fn is_safe_usb_target(&self) -> bool {
        if !self.bus_type.eq_ignore_ascii_case("USB") {
            return false;
        }
        if self.number == 0 || self.is_system || self.is_boot || self.is_read_only || self.is_offline {
            return false;
        }
        let has_protected_part = self
            .partitions
            .iter()
            .any(|p| p.is_system || p.is_boot || p.drive_letter == Some('C'));
        !has_protected_part
    }

    /// Returns a clear Indonesian safety/validation message if this disk cannot be wiped/targeted.
    pub fn safety_block_reason(&self, allow_non_usb: bool, min_bytes: u64) -> Option<String> {
        if self.is_system || self.is_boot {
            return Some(format!(
                "DIBLOKIR: Disk {} ({}) adalah Disk Sistem / Boot Windows aktif.",
                self.number, self.friendly_name
            ));
        }

        if self
            .partitions
            .iter()
            .any(|p| p.is_system || p.is_boot || p.drive_letter == Some('C'))
        {
            return Some(format!(
                "DIBLOKIR: Disk {} ({}) berisi partisi Sistem, Boot, atau drive C:\\.",
                self.number, self.friendly_name
            ));
        }

        if self.number == 0 {
            return Some(format!(
                "DIBLOKIR: Disk 0 ({}) dilindungi secara permanen untuk mencegah kehilangan data drive utama.",
                self.friendly_name
            ));
        }

        if self.is_read_only {
            return Some(format!(
                "Disk {} ({}) dalam status Read-Only (Write Protected).",
                self.number, self.friendly_name
            ));
        }

        if self.is_offline {
            return Some(format!(
                "Disk {} ({}) dalam status Offline. Online-kan disk terlebih dahulu.",
                self.number, self.friendly_name
            ));
        }

        if !allow_non_usb && !self.bus_type.eq_ignore_ascii_case("USB") {
            return Some(format!(
                "DIBLOKIR: Disk {} ({}) menggunakan bus '{}' (bukan USB). Aktifkan opsi lanjutan jika benar-benar yakin.",
                self.number, self.friendly_name, self.bus_type
            ));
        }

        if self.size_bytes < min_bytes {
            return Some(format!(
                "Kapasitas Disk {} ({}) terlalu kecil: tersedia {}, dibutuhkan minimal {}.",
                self.number,
                self.friendly_name,
                format_bytes_human(self.size_bytes),
                format_bytes_human(min_bytes)
            ));
        }

        None
    }
}

pub fn validate_target_disk(
    disk: &DiskInfo,
    allow_non_usb: bool,
    min_bytes: u64,
) -> Result<(), String> {
    if let Some(reason) = disk.safety_block_reason(allow_non_usb, min_bytes) {
        Err(reason)
    } else {
        Ok(())
    }
}

pub fn format_bytes_human(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if (bytes as f64) >= GIB {
        format!("{:.1} GB", (bytes as f64) / GIB)
    } else if (bytes as f64) >= MIB {
        format!("{:.1} MB", (bytes as f64) / MIB)
    } else {
        format!("{} B", bytes)
    }
}

/// Encode UTF-8 string into PowerShell `-EncodedCommand` UTF-16LE Base64.
pub fn encode_powershell_command(script: &str) -> String {
    let utf16_bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    encode_base64(&utf16_bytes)
}

pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn new_hidden_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

pub(crate) fn run_powershell_hidden(script: &str) -> Result<String, String> {
    let encoded = encode_powershell_command(script);
    let output = new_hidden_command("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .output()
        .map_err(|e| format!("Gagal menjalankan PowerShell: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "PowerShell gagal (exit {:?}): {}",
            output.status.code(),
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

pub(crate) fn run_command_cancellable(
    program: &str,
    args: &[&str],
    cancel: Option<&AtomicBool>,
) -> Result<(i32, String, String), String> {
    let mut child = new_hidden_command(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Gagal menjalankan '{}': {}", program, e))?;

    if let Some(flag) = cancel {
        loop {
            if flag.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Operasi dibatalkan oleh pengguna.".to_string());
            }
            match child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(120)),
                Err(e) => return Err(format!("Gagal memantau proses '{}': {}", program, e)),
            }
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("Gagal membaca output '{}': {}", program, e))?;

    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Ok((code, stdout, stderr))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDiskBundle {
    disks: Option<Value>,
    partitions: Option<Value>,
    volumes: Option<Value>,
}

fn value_to_vec(v: Option<Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr,
        Some(obj @ Value::Object(_)) => vec![obj],
        _ => Vec::new(),
    }
}

fn val_u32(obj: &Value, key: &str) -> u32 {
    obj.get(key)
        .and_then(|v| {
            v.as_u64()
                .map(|n| n as u32)
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u32>().ok()))
        })
        .unwrap_or(0)
}

fn val_u64(obj: &Value, key: &str) -> u64 {
    obj.get(key)
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        })
        .unwrap_or(0)
}

fn val_bool(obj: &Value, key: &str) -> bool {
    obj.get(key)
        .and_then(|v| {
            v.as_bool().or_else(|| {
                v.as_str()
                    .map(|s| s.eq_ignore_ascii_case("true") || s == "1")
            })
        })
        .unwrap_or(false)
}

fn val_string(obj: &Value, key: &str) -> String {
    obj.get(key)
        .and_then(|v| match v {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn decode_bus_type(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                "Unknown".to_string()
            } else if let Ok(code) = trimmed.parse::<u64>() {
                bus_code_to_name(code).to_string()
            } else {
                trimmed.to_string()
            }
        }
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|c| bus_code_to_name(c).to_string())
            .unwrap_or_else(|| "Unknown".to_string()),
        _ => "Unknown".to_string(),
    }
}

fn bus_code_to_name(code: u64) -> &'static str {
    match code {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        5 => "SSA",
        6 => "Fibre Channel",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "FileBackedVirtual",
        16 => "StorageSpaces",
        17 => "NVMe",
        _ => "Unknown",
    }
}

fn decode_partition_style(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => {
            let t = s.trim();
            match t {
                "0" => "RAW".to_string(),
                "1" => "MBR".to_string(),
                "2" => "GPT".to_string(),
                "" => "RAW".to_string(),
                other => other.to_string(),
            }
        }
        Some(Value::Number(n)) => match n.as_u64() {
            Some(1) => "MBR".to_string(),
            Some(2) => "GPT".to_string(),
            _ => "RAW".to_string(),
        },
        _ => "RAW".to_string(),
    }
}

fn parse_drive_letter(v: Option<&Value>) -> Option<char> {
    let raw = v?.as_str()?.trim();
    let mut chars = raw.chars();
    let ch = chars.next()?;
    if ch.is_ascii_alphabetic() {
        Some(ch.to_ascii_uppercase())
    } else {
        None
    }
}

/// Deterministic JSON parser for storage enumeration output.
pub fn parse_disks_json(json_str: &str) -> Result<Vec<DiskInfo>, String> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let bundle: RawDiskBundle = serde_json::from_str(trimmed)
        .map_err(|e| format!("Gagal mem-parsing JSON disk: {}", e))?;

    let vol_rows = value_to_vec(bundle.volumes);
    let mut vol_by_letter = std::collections::HashMap::<char, (String, String)>::new();
    for v in &vol_rows {
        if let Some(ch) = parse_drive_letter(v.get("DriveLetter")) {
            let fs = val_string(v, "FileSystem");
            let label = val_string(v, "FileSystemLabel");
            vol_by_letter.insert(ch, (fs, label));
        }
    }

    let part_rows = value_to_vec(bundle.partitions);
    let mut partitions: Vec<PartitionInfo> = Vec::with_capacity(part_rows.len());
    for p in &part_rows {
        let drive_letter = parse_drive_letter(p.get("DriveLetter"));
        let (file_system, label) = drive_letter
            .and_then(|ch| vol_by_letter.get(&ch).cloned())
            .unwrap_or_default();

        partitions.push(PartitionInfo {
            disk_number: val_u32(p, "DiskNumber"),
            partition_number: val_u32(p, "PartitionNumber"),
            drive_letter,
            size_bytes: val_u64(p, "Size"),
            part_type: val_string(p, "Type"),
            file_system,
            label,
            is_boot: val_bool(p, "IsBoot"),
            is_system: val_bool(p, "IsSystem"),
            is_active: val_bool(p, "IsActive"),
        });
    }

    let disk_rows = value_to_vec(bundle.disks);
    let mut disks: Vec<DiskInfo> = Vec::with_capacity(disk_rows.len());
    for d in &disk_rows {
        let num = val_u32(d, "Number");
        let disk_parts: Vec<PartitionInfo> = partitions
            .iter()
            .filter(|p| p.disk_number == num)
            .cloned()
            .collect();

        disks.push(DiskInfo {
            number: num,
            friendly_name: val_string(d, "FriendlyName"),
            bus_type: decode_bus_type(d.get("BusType")),
            size_bytes: val_u64(d, "Size"),
            partition_style: decode_partition_style(d.get("PartitionStyle")),
            is_system: val_bool(d, "IsSystem"),
            is_boot: val_bool(d, "IsBoot"),
            is_offline: val_bool(d, "IsOffline"),
            is_read_only: val_bool(d, "IsReadOnly"),
            serial_number: val_string(d, "SerialNumber"),
            partitions: disk_parts,
        });
    }

    // Sort USB disks first, then by disk number
    disks.sort_by_key(|d| {
        let usb_rank = if d.bus_type.eq_ignore_ascii_case("USB") {
            0
        } else {
            1
        };
        (usb_rank, d.number)
    });

    Ok(disks)
}

/// Enumerate all physical disks and partitions via hidden PowerShell JSON query.
pub fn enumerate_disks() -> Result<Vec<DiskInfo>, String> {
    let ps_script = r#"
$ErrorActionPreference = 'Stop'
$disks = @(Get-Disk | Select-Object Number, FriendlyName, @{N='BusType';E={$_.BusType.ToString()}}, Size, @{N='PartitionStyle';E={$_.PartitionStyle.ToString()}}, IsSystem, IsBoot, IsOffline, IsReadOnly, SerialNumber)
$parts = @(Get-Partition | Select-Object DiskNumber, PartitionNumber, @{N='DriveLetter';E={[string]$_.DriveLetter}}, Size, @{N='Type';E={[string]$_.Type}}, IsBoot, IsSystem, IsActive)
$vols = @(Get-Volume | Where-Object { $_.DriveLetter } | Select-Object @{N='DriveLetter';E={[string]$_.DriveLetter}}, FileSystem, FileSystemLabel)
[pscustomobject]@{
    Disks = $disks
    Partitions = $parts
    Volumes = $vols
} | ConvertTo-Json -Depth 4 -Compress
"#;

    let json_out = run_powershell_hidden(ps_script)?;
    parse_disks_json(&json_out)
}

/// Find `count` unused drive letters scanning from Z: down to E: so A:-D: are never disturbed.
/// `used_mask_override` allows deterministic testing without Win32 calls.
pub fn find_free_drive_letters(
    count: usize,
    used_mask_override: Option<u32>,
) -> Result<Vec<char>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }

    let mask: u32 = match used_mask_override {
        Some(m) => m,
        None => {
            #[cfg(windows)]
            {
                unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() }
            }
            #[cfg(not(windows))]
            {
                0b0000_1111 // A-D marked used
            }
        }
    };

    let mut free = Vec::with_capacity(count);
    for idx in (4u8..=25u8).rev() {
        // bit 4 = E, bit 25 = Z
        if (mask & (1u32 << idx)) == 0 {
            free.push((b'A' + idx) as char);
            if free.len() == count {
                return Ok(free);
            }
        }
    }

    Err(format!(
        "Tidak cukup huruf drive bebas (dibutuhkan {}, hanya tersedia {}).",
        count,
        free.len()
    ))
}

pub fn sanitize_volume_label(label: &str, max_len: usize) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .take(max_len)
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() {
        "WinToUSB".to_string()
    } else {
        trimmed
    }
}

fn validate_drive_letter(ch: char) -> Result<char, String> {
    let upper = ch.to_ascii_uppercase();
    if ('E'..='Z').contains(&upper) {
        Ok(upper)
    } else {
        Err(format!(
            "Huruf drive target '{}' tidak valid atau dilindungi (harus E-Z).",
            ch
        ))
    }
}

/// Generate deterministic, validated DiskPart script for the chosen partition scheme.
pub fn generate_diskpart_script(
    disk_number: u32,
    scheme: PartitionScheme,
    system_letter: Option<char>,
    windows_letter: Option<char>,
    efi_size_mb: u32,
    volume_label: &str,
) -> Result<String, String> {
    if disk_number == 0 {
        return Err("DIBLOKIR: Disk 0 tidak diizinkan dalam skrip format DiskPart.".to_string());
    }

    let ntfs_label = sanitize_volume_label(volume_label, 32);

    match scheme {
        PartitionScheme::KeepExisting => Err(
            "Skema 'Pertahankan Partisi yang Ada' tidak menghasilkan skrip clean/format DiskPart."
                .to_string(),
        ),
        PartitionScheme::GptUefi => {
            let sys = validate_drive_letter(
                system_letter.ok_or("Huruf partisi EFI (System) diperlukan untuk GPT UEFI.")?,
            )?;
            let win = validate_drive_letter(
                windows_letter.ok_or("Huruf partisi Windows diperlukan untuk GPT UEFI.")?,
            )?;
            if sys == win {
                return Err("Huruf partisi EFI dan Windows tidak boleh sama.".to_string());
            }
            let efi_mb = efi_size_mb.clamp(100, 1024);
            Ok(format!(
                "select disk {disk}\r\nclean\r\nconvert gpt\r\ncreate partition efi size={efi}\r\nformat quick fs=fat32 label=\"EFI\"\r\nassign letter={sys}\r\ncreate partition primary\r\nformat quick fs=ntfs label=\"{label}\"\r\nassign letter={win}\r\nexit\r\n",
                disk = disk_number,
                efi = efi_mb,
                sys = sys,
                win = win,
                label = ntfs_label
            ))
        }
        PartitionScheme::MbrBios => {
            let win = validate_drive_letter(
                windows_letter.ok_or("Huruf partisi Windows diperlukan untuk MBR BIOS.")?,
            )?;
            Ok(format!(
                "select disk {disk}\r\nclean\r\nconvert mbr\r\ncreate partition primary\r\nformat quick fs=ntfs label=\"{label}\"\r\nactive\r\nassign letter={win}\r\nexit\r\n",
                disk = disk_number,
                win = win,
                label = ntfs_label
            ))
        }
        PartitionScheme::MbrBiosAndUefi => {
            let sys = validate_drive_letter(
                system_letter.ok_or("Huruf partisi Boot/EFI diperlukan untuk MBR Dual Boot.")?,
            )?;
            let win = validate_drive_letter(
                windows_letter.ok_or("Huruf partisi Windows diperlukan untuk MBR Dual Boot.")?,
            )?;
            if sys == win {
                return Err("Huruf partisi Boot/EFI dan Windows tidak boleh sama.".to_string());
            }
            let boot_mb = efi_size_mb.clamp(260, 2048);
            Ok(format!(
                "select disk {disk}\r\nclean\r\nconvert mbr\r\ncreate partition primary size={boot}\r\nformat quick fs=fat32 label=\"BOOT\"\r\nactive\r\nassign letter={sys}\r\ncreate partition primary\r\nformat quick fs=ntfs label=\"{label}\"\r\nassign letter={win}\r\nexit\r\n",
                disk = disk_number,
                boot = boot_mb,
                sys = sys,
                win = win,
                label = ntfs_label
            ))
        }
    }
}

/// Generate simple single-partition FAT32/exFAT/NTFS format script for utility / installer mode.
/// Automatically caps native FAT32 partition size at 32,000 MB when `disk_size_bytes > 32 GiB`
/// because Windows DiskPart rejects `format fs=fat32` on volumes larger than 32 GiB.
pub fn generate_single_fs_script(
    disk_number: u32,
    use_gpt: bool,
    fs_type: &str,
    letter: char,
    volume_label: &str,
    disk_size_bytes: u64,
) -> Result<String, String> {
    if disk_number == 0 {
        return Err("DIBLOKIR: Disk 0 tidak diizinkan untuk diformat.".to_string());
    }
    let drv = validate_drive_letter(letter)?;
    let fs_clean = match fs_type.to_ascii_lowercase().as_str() {
        "fat32" => "fat32",
        "ntfs" => "ntfs",
        "exfat" => "exfat",
        other => return Err(format!("Sistem berkas '{}' tidak didukung.", other)),
    };
    let max_label = if fs_clean == "fat32" { 11 } else { 32 };
    let clean_label = sanitize_volume_label(volume_label, max_label);
    let convert_cmd = if use_gpt { "convert gpt" } else { "convert mbr" };
    let active_line = if use_gpt { "" } else { "active\r\n" };
    let create_part = if fs_clean == "fat32" && disk_size_bytes > 32 * 1024 * 1024 * 1024 {
        "create partition primary size=32000"
    } else {
        "create partition primary"
    };

    Ok(format!(
        "select disk {disk}\r\nclean\r\n{convert}\r\n{create_part}\r\nformat quick fs={fs} label=\"{label}\"\r\n{active}assign letter={drv}\r\nexit\r\n",
        disk = disk_number,
        convert = convert_cmd,
        create_part = create_part,
        fs = fs_clean,
        label = clean_label,
        active = active_line,
        drv = drv
    ))
}

pub fn temp_script_path(prefix: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    std::env::temp_dir().join(format!("{}_{}_{}.txt", prefix, pid, ts))
}

/// Execute a DiskPart script silently via `diskpart.exe /s <script>` with cleanup.
pub fn execute_diskpart_script(
    script: &str,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    let path = temp_script_path("wintousb_dp");
    fs::write(&path, script).map_err(|e| format!("Gagal menulis skrip sementara DiskPart: {}", e))?;

    let path_str = path.to_string_lossy().to_string();
    let result = run_command_cancellable("diskpart.exe", &["/s", &path_str], cancel);
    let _ = fs::remove_file(&path);

    let (code, stdout, stderr) = result?;
    let combined = format!("{}\n{}", stdout, stderr);
    let lower = combined.to_ascii_lowercase();

    if code != 0
        || lower.contains("diskpart has encountered an error")
        || lower.contains("access is denied")
        || lower.contains("the parameter is incorrect")
        || lower.contains("virtual disk service error")
    {
        return Err(format!(
            "DiskPart gagal (exit {}): {}",
            code,
            combined.trim()
        ));
    }

    Ok(stdout)
}
