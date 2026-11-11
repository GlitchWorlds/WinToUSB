use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::disk::{execute_diskpart_script, format_bytes_human, sanitize_volume_label};

pub const MIN_VHDX_SIZE_GB: u32 = 16;
pub const MAX_VHDX_SIZE_GB: u32 = 2040;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhdxConfig {
    pub vhd_path: String,
    pub size_gb: u32,
    pub expandable: bool,
    pub label: String,
}

impl Default for VhdxConfig {
    fn default() -> Self {
        Self {
            vhd_path: "E:\\WinToGo.vhdx".to_string(),
            size_gb: 32,
            expandable: true,
            label: "WinToGo".to_string(),
        }
    }
}

/// Validate virtual disk configuration before generating DiskPart scripts.
pub fn validate_vhdx_config(
    cfg: &VhdxConfig,
    target_free_bytes: Option<u64>,
) -> Result<(), String> {
    let trimmed = cfg.vhd_path.trim();
    if trimmed.is_empty() {
        return Err("Path berkas VHD/VHDX tidak boleh kosong.".to_string());
    }
    if trimmed.contains('"') || trimmed.contains('\n') || trimmed.contains('\r') {
        return Err("Path VHD/VHDX mengandung karakter tidak valid.".to_string());
    }

    let ext = Path::new(trimmed)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if ext != "vhdx" && ext != "vhd" {
        return Err("Ekstensi virtual disk harus '.vhdx' atau '.vhd'.".to_string());
    }

    if cfg.size_gb < MIN_VHDX_SIZE_GB {
        return Err(format!(
            "Ukuran VHD/VHDX minimal {} GB agar cukup menampung instalasi Windows (diminta: {} GB).",
            MIN_VHDX_SIZE_GB, cfg.size_gb
        ));
    }

    if cfg.size_gb > MAX_VHDX_SIZE_GB {
        return Err(format!(
            "Ukuran VHD/VHDX maksimal {} GB (diminta: {} GB).",
            MAX_VHDX_SIZE_GB, cfg.size_gb
        ));
    }

    if let Some(free) = target_free_bytes {
        let req_bytes = if cfg.expandable {
            // Dynamic VHDX initially needs space for the Windows image (~10 GiB minimum)
            10 * 1024 * 1024 * 1024u64
        } else {
            (cfg.size_gb as u64) * 1024 * 1024 * 1024 + (256 * 1024 * 1024)
        };
        if free < req_bytes {
            return Err(format!(
                "Ruang kosong drive tidak mencukupi untuk {} VHDX: tersedia {}, dibutuhkan {}.",
                if cfg.expandable { "Dynamic" } else { "Fixed" },
                format_bytes_human(free),
                format_bytes_human(req_bytes)
            ));
        }
    }

    Ok(())
}

/// Generate DiskPart script that creates, attaches, partitions, and formats a VHD/VHDX.
pub fn generate_vhdx_create_script(
    cfg: &VhdxConfig,
    inner_win_letter: char,
) -> Result<String, String> {
    validate_vhdx_config(cfg, None)?;
    let drv = inner_win_letter.to_ascii_uppercase();
    if !('E'..='Z').contains(&drv) {
        return Err(format!(
            "Huruf drive internal VHDX '{}' tidak valid (harus E-Z).",
            inner_win_letter
        ));
    }

    let size_mb = (cfg.size_gb as u64) * 1024;
    let vhd_type = if cfg.expandable {
        "expandable"
    } else {
        "fixed"
    };
    let label = sanitize_volume_label(&cfg.label, 32);

    Ok(format!(
        "create vdisk file=\"{path}\" maximum={mb} type={vtype}\r\nselect vdisk file=\"{path}\"\r\nattach vdisk\r\nconvert mbr\r\ncreate partition primary\r\nformat quick fs=ntfs label=\"{label}\"\r\nassign letter={drv}\r\nexit\r\n",
        path = cfg.vhd_path.trim(),
        mb = size_mb,
        vtype = vhd_type,
        label = label,
        drv = drv
    ))
}

pub fn generate_vhdx_attach_script(
    vhd_path: &str,
    assign_letter: Option<char>,
) -> Result<String, String> {
    let trimmed = vhd_path.trim();
    if trimmed.is_empty() || trimmed.contains('"') || trimmed.contains('\n') {
        return Err("Path VHD/VHDX tidak valid untuk attach.".to_string());
    }
    if let Some(ch) = assign_letter {
        let drv = ch.to_ascii_uppercase();
        if !('E'..='Z').contains(&drv) {
            return Err(format!("Huruf drive '{}' tidak valid (harus E-Z).", ch));
        }
        Ok(format!(
            "select vdisk file=\"{}\"\r\nattach vdisk\r\nselect partition 1\r\nassign letter={}\r\nexit\r\n",
            trimmed, drv
        ))
    } else {
        Ok(format!(
            "select vdisk file=\"{}\"\r\nattach vdisk\r\nexit\r\n",
            trimmed
        ))
    }
}

pub fn generate_vhdx_detach_script(vhd_path: &str) -> Result<String, String> {
    let trimmed = vhd_path.trim();
    if trimmed.is_empty() || trimmed.contains('"') || trimmed.contains('\n') {
        return Err("Path VHD/VHDX tidak valid untuk detach.".to_string());
    }
    Ok(format!(
        "select vdisk file=\"{}\"\r\ndetach vdisk\r\nexit\r\n",
        trimmed
    ))
}

/// RAII guard ensuring an attached VHD/VHDX is detached cleanly if deployment fails or cancels.
pub struct VhdxAttachGuard {
    pub vhd_path: String,
    detached: bool,
}

impl VhdxAttachGuard {
    pub fn create_and_attach(
        cfg: &VhdxConfig,
        inner_win_letter: char,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Self, String), String> {
        let script = generate_vhdx_create_script(cfg, inner_win_letter)?;
        let out = execute_diskpart_script(&script, cancel)?;
        Ok((
            Self {
                vhd_path: cfg.vhd_path.clone(),
                detached: false,
            },
            out,
        ))
    }

    pub fn attach_existing_and_assign(
        vhd_path: &str,
        assign_letter: char,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self, String> {
        let script = generate_vhdx_attach_script(vhd_path, Some(assign_letter))?;
        execute_diskpart_script(&script, cancel)?;
        Ok(Self {
            vhd_path: vhd_path.trim().to_string(),
            detached: false,
        })
    }

    pub fn detach(&mut self) -> Result<String, String> {
        if self.detached {
            return Ok(String::new());
        }
        let script = generate_vhdx_detach_script(&self.vhd_path)?;
        self.detached = true;
        execute_diskpart_script(&script, None)
    }
}

impl Drop for VhdxAttachGuard {
    fn drop(&mut self) {
        if !self.detached {
            let _ = self.detach();
        }
    }
}
