use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::disk::{format_bytes_human, new_hidden_command, run_powershell_hidden, MIN_WTG_DISK_BYTES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveOsInfo {
    pub caption: String,
    pub version: String,
    pub build_number: String,
    pub architecture: String,
    pub system_drive: String,
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub firmware_type: String,
}

impl LiveOsInfo {
    pub fn recommended_min_usb_bytes(&self) -> u64 {
        minimum_required_usb_bytes(self.used_bytes)
    }

    pub fn used_display(&self) -> String {
        format_bytes_human(self.used_bytes)
    }

    pub fn total_display(&self) -> String {
        format_bytes_human(self.total_bytes)
    }
}

/// Minimum USB capacity needed to hot-clone a live OS safely (+15% headroom, floor 16 GiB).
pub fn minimum_required_usb_bytes(used_bytes: u64) -> u64 {
    let with_headroom = ((used_bytes as f64) * 1.15).ceil() as u64;
    with_headroom.max(MIN_WTG_DISK_BYTES)
}

pub fn detect_firmware_type() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{
            FirmwareTypeBios, FirmwareTypeUefi, GetFirmwareType, FIRMWARE_TYPE,
        };
        let mut fw: FIRMWARE_TYPE = 0;
        let ok = unsafe { GetFirmwareType(&mut fw) };
        if ok != 0 {
            if fw == FirmwareTypeUefi {
                return "UEFI".to_string();
            } else if fw == FirmwareTypeBios {
                return "BIOS (Legacy)".to_string();
            }
        }
    }
    "Unknown".to_string()
}

fn query_drive_usage_bytes(drive_root: &str) -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = format!("{}\0", drive_root.trim_end_matches('\\'))
            .replace(':', ":\\")
            .encode_utf16()
            .collect();
        let mut free_avail = 0u64;
        let mut total = 0u64;
        let mut free_total = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut free_total)
        };
        if ok != 0 && total >= free_total {
            return (total - free_total, total);
        }
    }
    let _ = drive_root;
    (0, 0)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawOsJson {
    caption: Option<String>,
    version: Option<String>,
    build_number: Option<String>,
    #[serde(rename = "OSArchitecture")]
    os_architecture: Option<String>,
    system_drive: Option<String>,
}

pub fn parse_live_os_json(json_str: &str, used_bytes: u64, total_bytes: u64, firmware_type: String) -> Result<LiveOsInfo, String> {
    let raw: RawOsJson = serde_json::from_str(json_str.trim())
        .map_err(|e| format!("Gagal mem-parsing metadata OS aktif: {}", e))?;

    Ok(LiveOsInfo {
        caption: raw
            .caption
            .unwrap_or_else(|| "Microsoft Windows".to_string())
            .trim()
            .to_string(),
        version: raw.version.unwrap_or_default().trim().to_string(),
        build_number: raw.build_number.unwrap_or_default().trim().to_string(),
        architecture: raw
            .os_architecture
            .unwrap_or_else(|| "64-bit".to_string())
            .trim()
            .to_string(),
        system_drive: raw
            .system_drive
            .unwrap_or_else(|| "C:".to_string())
            .trim()
            .to_string(),
        used_bytes,
        total_bytes,
        firmware_type,
    })
}

pub fn query_live_os_info() -> Result<LiveOsInfo, String> {
    let ps = r#"
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture, SystemDrive | ConvertTo-Json -Compress
"#;
    let json = run_powershell_hidden(ps)?;
    let parsed_preview: RawOsJson = serde_json::from_str(json.trim())
        .map_err(|e| format!("Gagal mem-parsing info OS: {}", e))?;
    let sys_drv = parsed_preview
        .system_drive
        .clone()
        .unwrap_or_else(|| "C:".to_string());
    let (used, total) = query_drive_usage_bytes(&sys_drv);
    let fw = detect_firmware_type();
    parse_live_os_json(&json, used, total, fw)
}

/// Generate WimScript.ini exclusion file content for DISM live OS capture,
/// skipping pagefile, hibernation file, Recycle Bin, and transient caches.
pub fn generate_wimscript_ini() -> String {
    "[ExclusionList]\r\n\
     \\$Recycle.Bin\r\n\
     \\System Volume Information\r\n\
     \\pagefile.sys\r\n\
     \\hiberfil.sys\r\n\
     \\swapfile.sys\r\n\
     \\DumpStack.log.tmp\r\n\
     \\Windows\\CSC\r\n\
     \\Windows\\Temp\\*\r\n\
     \\Windows\\SoftwareDistribution\\Download\\*\r\n\
     \\WinToUSB_Temp\r\n"
        .to_string()
}

pub fn build_vss_capture_dism_args(
    wim_path: &str,
    capture_dir: &str,
    config_file: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "/English".to_string(),
        "/Capture-Image".to_string(),
        format!("/ImageFile:{}", wim_path),
        format!("/CaptureDir:{}", capture_dir),
        "/Name:WinToUSB-LiveClone".to_string(),
        "/Compress:fast".to_string(),
        "/CheckIntegrity".to_string(),
    ];
    if let Some(cfg) = config_file {
        args.push(format!("/ConfigFile:{}", cfg));
    }
    args
}

/// RAII guard for a Windows Volume Shadow Copy + temporary directory symlink.
/// Ensures both the symlink and shadow copy are deleted on drop/abort/completion.
pub struct VssShadowGuard {
    pub shadow_id: String,
    pub device_object: String,
    pub mount_link: PathBuf,
    cleaned: bool,
}

impl VssShadowGuard {
    /// Create a ClientAccessible shadow copy of `volume` (e.g. `"C:\\"`) via WMI
    /// and link it to a temporary directory for DISM capture.
    pub fn create_for_volume(volume: &str) -> Result<Self, String> {
        let normalized_vol = if volume.ends_with('\\') {
            volume.to_string()
        } else {
            format!("{}\\", volume)
        };
        let escaped_vol = normalized_vol.replace('\'', "''");

        let ps = format!(
            r#"
$ErrorActionPreference = 'Stop'
$class = [wmiclass]'Win32_ShadowCopy'
$res = $class.Create('{vol}', 'ClientAccessible')
if ($res.ReturnValue -ne 0) {{
    throw "Win32_ShadowCopy.Create gagal dengan kode $($res.ReturnValue). Pastikan dijalankan sebagai Administrator dan layanan Volume Shadow Copy (vss) aktif."
}}
$sc = Get-CimInstance Win32_ShadowCopy | Where-Object {{ $_.ID -eq $res.ShadowID }} | Select-Object -First 1
if (-not $sc) {{
    throw "Shadow copy berhasil dibuat ($($res.ShadowID)) tetapi objek perangkat tidak ditemukan."
}}
[pscustomobject]@{{ ID = $sc.ID; DeviceObject = $sc.DeviceObject }} | ConvertTo-Json -Compress
"#,
            vol = escaped_vol
        );

        let out = run_powershell_hidden(&ps)?;
        let val: serde_json::Value = serde_json::from_str(out.trim())
            .map_err(|e| format!("Gagal mem-parsing output VSS: {} ({})", e, out))?;
        let shadow_id = val
            .get("ID")
            .and_then(|v| v.as_str())
            .ok_or("ShadowID tidak ditemukan pada output VSS.")?
            .to_string();
        let device_object = val
            .get("DeviceObject")
            .and_then(|v| v.as_str())
            .ok_or("DeviceObject tidak ditemukan pada output VSS.")?
            .to_string();

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let link_path = std::env::temp_dir().join(format!(
            "WinToUSB_VSS_{}_{}",
            std::process::id(),
            ts
        ));

        let target_with_slash = if device_object.ends_with('\\') {
            device_object.clone()
        } else {
            format!("{}\\", device_object)
        };
        let link_str = link_path.to_string_lossy().to_string();

        let mk_out = new_hidden_command("cmd.exe")
            .args(["/c", "mklink", "/d", &link_str, &target_with_slash])
            .output()
            .map_err(|e| format!("Gagal membuat symlink VSS: {}", e))?;

        if !mk_out.status.success() {
            // Cleanup shadow immediately before returning error
            let _ = delete_shadow_by_id(&shadow_id);
            let err_txt = String::from_utf8_lossy(&mk_out.stderr);
            return Err(format!(
                "mklink /d gagal untuk shadow copy: {}",
                err_txt.trim()
            ));
        }

        Ok(Self {
            shadow_id,
            device_object,
            mount_link: link_path,
            cleaned: false,
        })
    }

    pub fn cleanup(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        if self.mount_link.exists() {
            // Remove ONLY the symlink directory entry (never recurse)
            let link_str = self.mount_link.to_string_lossy().to_string();
            let _ = new_hidden_command("cmd.exe")
                .args(["/c", "rmdir", &link_str])
                .output();
            if self.mount_link.exists() {
                let _ = fs::remove_dir(&self.mount_link);
            }
        }

        delete_shadow_by_id(&self.shadow_id)
    }
}

impl Drop for VssShadowGuard {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

fn delete_shadow_by_id(shadow_id: &str) -> Result<(), String> {
    let out = new_hidden_command("vssadmin.exe")
        .args([
            "delete",
            "shadows",
            &format!("/shadow={}", shadow_id),
            "/quiet",
        ])
        .output()
        .map_err(|e| format!("Gagal menghapus shadow copy {}: {}", shadow_id, e))?;

    if out.status.success() {
        Ok(())
    } else {
        // Fallback via CIM/WMI
        let escaped = shadow_id.replace('\'', "''");
        let ps = format!(
            "Get-CimInstance Win32_ShadowCopy | Where-Object {{ $_.ID -eq '{}' }} | Remove-CimInstance -ErrorAction SilentlyContinue",
            escaped
        );
        run_powershell_hidden(&ps).map(|_| ())
    }
}
