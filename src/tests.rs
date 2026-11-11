use std::fs;
use std::path::Path;

use crate::deploy::{
    build_dry_run_plan, extract_bfsvc_error_lines, generate_bcdboot_args, generate_dism_apply_args,
    is_checksum_or_pe_error, read_pe_machine, resolve_bcdboot_candidates_from_dirs,
    select_bcdboot_candidates, verify_bcd_enum_retargeted, DeployMode, DeployOptions,
    UtilityAction, PE_MACHINE_AMD64, PE_MACHINE_ARM64,
};
use crate::disk::{
    encode_powershell_command, enumerate_disks, find_free_drive_letters, generate_diskpart_script,
    generate_single_fs_script, parse_disks_json, validate_target_disk, DiskInfo, PartitionInfo,
    PartitionScheme, MIN_WTG_DISK_BYTES,
};
use crate::elevate::is_elevated;
use crate::image::{parse_dism_progress, parse_dism_wim_info};
use crate::vhdx::{
    generate_vhdx_attach_script, generate_vhdx_create_script, generate_vhdx_detach_script,
    validate_vhdx_config, VhdxConfig,
};

fn make_disk(
    number: u32,
    bus_type: &str,
    size_gb: u64,
    is_system: bool,
    is_boot: bool,
    partitions: Vec<PartitionInfo>,
) -> DiskInfo {
    DiskInfo {
        number,
        friendly_name: format!("Test {} Drive #{}", bus_type, number),
        bus_type: bus_type.to_string(),
        size_bytes: size_gb * 1024 * 1024 * 1024,
        partition_style: "GPT".to_string(),
        is_system,
        is_boot,
        is_offline: false,
        is_read_only: false,
        serial_number: "TESTSERIAL001".to_string(),
        partitions,
    }
}

#[test]
fn test_safety_guard_blocks_system_and_internal_disks() {
    // 1. Disk 0 USB or SATA must ALWAYS be blocked
    let disk0_sata = make_disk(0, "SATA", 500, false, false, vec![]);
    assert!(!disk0_sata.is_safe_usb_target());
    assert!(validate_target_disk(&disk0_sata, false, MIN_WTG_DISK_BYTES).is_err());
    assert!(validate_target_disk(&disk0_sata, true, MIN_WTG_DISK_BYTES).is_err());

    let disk0_usb = make_disk(0, "USB", 64, false, false, vec![]);
    assert!(!disk0_usb.is_safe_usb_target());
    assert!(validate_target_disk(&disk0_usb, true, MIN_WTG_DISK_BYTES).is_err());

    // 2. System / Boot disk (even on Disk 1) must ALWAYS be blocked even if allow_non_usb=true
    let sys_nvme = make_disk(1, "NVMe", 256, true, true, vec![]);
    assert!(!sys_nvme.is_safe_usb_target());
    assert!(validate_target_disk(&sys_nvme, false, MIN_WTG_DISK_BYTES).is_err());
    assert!(validate_target_disk(&sys_nvme, true, MIN_WTG_DISK_BYTES).is_err());

    // 3. USB disk flagged IsBoot or IsSystem or containing C:\ must ALWAYS be blocked
    let boot_usb = make_disk(2, "USB", 64, false, true, vec![]);
    assert!(!boot_usb.is_safe_usb_target());
    assert!(validate_target_disk(&boot_usb, true, MIN_WTG_DISK_BYTES).is_err());

    let c_drive_disk = make_disk(
        2,
        "USB",
        128,
        false,
        false,
        vec![PartitionInfo {
            disk_number: 2,
            partition_number: 1,
            drive_letter: Some('C'),
            size_bytes: 64 * 1024 * 1024 * 1024,
            part_type: "Basic".to_string(),
            file_system: "NTFS".to_string(),
            label: "Windows".to_string(),
            is_boot: false,
            is_system: false,
            is_active: false,
        }],
    );
    assert!(!c_drive_disk.is_safe_usb_target());
    assert!(validate_target_disk(&c_drive_disk, true, MIN_WTG_DISK_BYTES).is_err());

    // 4. Non-system SATA / NVMe blocked when allow_non_usb=false
    let data_sata = make_disk(2, "SATA", 1000, false, false, vec![]);
    assert!(!data_sata.is_safe_usb_target());
    assert!(validate_target_disk(&data_sata, false, MIN_WTG_DISK_BYTES).is_err());
}

#[test]
fn test_safety_guard_allows_clean_removable_usb() {
    let clean_usb = make_disk(
        2,
        "USB",
        64,
        false,
        false,
        vec![PartitionInfo {
            disk_number: 2,
            partition_number: 1,
            drive_letter: Some('E'),
            size_bytes: 64 * 1024 * 1024 * 1024,
            part_type: "Basic".to_string(),
            file_system: "NTFS".to_string(),
            label: "USB".to_string(),
            is_boot: false,
            is_system: false,
            is_active: false,
        }],
    );

    assert!(clean_usb.is_safe_usb_target());
    assert!(validate_target_disk(&clean_usb, false, MIN_WTG_DISK_BYTES).is_ok());

    // Too small USB (< 16 GB) should fail capacity check
    let small_usb = make_disk(3, "USB", 8, false, false, vec![]);
    assert!(small_usb.is_safe_usb_target());
    assert!(validate_target_disk(&small_usb, false, MIN_WTG_DISK_BYTES).is_err());
}

#[test]
fn test_parse_dism_wim_info_output() {
    let sample = r#"
Deployment Image Servicing and Management tool
Version: 10.0.19041.3636

Details for image : C:\ISO\sources\install.wim

Index : 1
Name : Windows 11 Home
Description : Windows 11 Home
Size : 16,106,127,360 bytes

Index : 2
Name : Windows 11 Education
Description : Windows 11 Education N/A
Size : 16,423,912,411 bytes

Index : 6
Name : Windows 11 Pro
Description : Windows 11 Pro
Size : 16,642,998,272 bytes

The operation completed successfully.
"#;

    let editions = parse_dism_wim_info(sample);
    assert_eq!(editions.len(), 3);
    assert_eq!(editions[0].index, 1);
    assert_eq!(editions[0].name, "Windows 11 Home");
    assert_eq!(editions[0].size_bytes, 16_106_127_360);
    assert!(editions[0].is_windows_11);

    assert_eq!(editions[1].index, 2);
    assert_eq!(editions[1].name, "Windows 11 Education");
    assert_eq!(editions[1].size_bytes, 16_423_912_411);

    assert_eq!(editions[2].index, 6);
    assert_eq!(editions[2].name, "Windows 11 Pro");
    assert_eq!(editions[2].size_bytes, 16_642_998_272);
}

#[test]
fn test_parse_dism_progress_percentage() {
    assert_eq!(
        parse_dism_progress("[==========================100.0%==========================]"),
        Some(100.0)
    );
    assert_eq!(
        parse_dism_progress("[===========       42.5%                   ]"),
        Some(42.5)
    );
    assert_eq!(parse_dism_progress("  0.0% "), Some(0.0));
    assert_eq!(parse_dism_progress("Version: 10.0.19041.3636"), None);
    assert_eq!(
        parse_dism_progress("Applying image... 78,4%"),
        Some(78.4)
    );
}

#[test]
fn test_generate_diskpart_script_gpt_uefi() {
    let script = generate_diskpart_script(
        2,
        PartitionScheme::GptUefi,
        Some('Z'),
        Some('Y'),
        260,
        "WinToUSB",
    )
    .expect("GPT UEFI script should succeed");

    assert!(script.contains("select disk 2"));
    assert!(script.contains("clean"));
    assert!(script.contains("convert gpt"));
    assert!(script.contains("create partition efi size=260"));
    assert!(script.contains("format quick fs=fat32 label=\"EFI\""));
    assert!(script.contains("assign letter=Z"));
    assert!(script.contains("create partition primary"));
    assert!(script.contains("format quick fs=ntfs label=\"WinToUSB\""));
    assert!(script.contains("assign letter=Y"));
    assert!(script.contains("exit"));

    // Disk 0 must fail even directly in script generator
    assert!(generate_diskpart_script(
        0,
        PartitionScheme::GptUefi,
        Some('Z'),
        Some('Y'),
        260,
        "WinToUSB"
    )
    .is_err());

    // FAT32 on >32GB drive must automatically cap partition at 32000 MB
    let fat32_64g = generate_single_fs_script(
        2,
        false,
        "fat32",
        'Z',
        "WININSTALL",
        64 * 1024 * 1024 * 1024,
    )
    .expect("FAT32 single script");
    assert!(fat32_64g.contains("create partition primary size=32000"));
    assert!(fat32_64g.contains("format quick fs=fat32"));
}

#[test]
fn test_generate_diskpart_script_mbr_dual_boot() {
    let script = generate_diskpart_script(
        3,
        PartitionScheme::MbrBiosAndUefi,
        Some('X'),
        Some('W'),
        300,
        "Win11USB",
    )
    .expect("MBR Dual Boot script should succeed");

    assert!(script.contains("select disk 3"));
    assert!(script.contains("clean"));
    assert!(script.contains("convert mbr"));
    assert!(script.contains("create partition primary size=300"));
    assert!(script.contains("format quick fs=fat32 label=\"BOOT\""));
    assert!(script.contains("active"));
    assert!(script.contains("assign letter=X"));
    assert!(script.contains("format quick fs=ntfs label=\"Win11USB\""));
    assert!(script.contains("assign letter=W"));
}

#[test]
fn test_generate_vhdx_script_and_validation() {
    let bad_small = VhdxConfig {
        vhd_path: "E:\\WinToGo.vhdx".to_string(),
        size_gb: 8,
        expandable: true,
        label: "WinToGo".to_string(),
    };
    assert!(validate_vhdx_config(&bad_small, None).is_err());

    let bad_ext = VhdxConfig {
        vhd_path: "E:\\WinToGo.img".to_string(),
        size_gb: 32,
        expandable: true,
        label: "WinToGo".to_string(),
    };
    assert!(validate_vhdx_config(&bad_ext, None).is_err());

    let good = VhdxConfig {
        vhd_path: "Y:\\WinToGo.vhdx".to_string(),
        size_gb: 40,
        expandable: true,
        label: "WinToGo".to_string(),
    };
    assert!(validate_vhdx_config(&good, Some(20 * 1024 * 1024 * 1024)).is_ok());

    let fixed_too_big = VhdxConfig {
        vhd_path: "Y:\\WinToGo.vhdx".to_string(),
        size_gb: 64,
        expandable: false,
        label: "WinToGo".to_string(),
    };
    assert!(validate_vhdx_config(&fixed_too_big, Some(32 * 1024 * 1024 * 1024)).is_err());

    let script = generate_vhdx_create_script(&good, 'X').expect("VHDX create script");
    assert!(script.contains("create vdisk file=\"Y:\\WinToGo.vhdx\" maximum=40960 type=expandable"));
    assert!(script.contains("attach vdisk"));
    assert!(script.contains("format quick fs=ntfs label=\"WinToGo\""));
    assert!(script.contains("assign letter=X"));

    let attach = generate_vhdx_attach_script("Y:\\WinToGo.vhdx", Some('X')).expect("attach script");
    assert!(attach.contains("select vdisk file=\"Y:\\WinToGo.vhdx\""));
    assert!(attach.contains("attach vdisk"));
    assert!(attach.contains("assign letter=X"));

    let detach = generate_vhdx_detach_script("Y:\\WinToGo.vhdx").expect("detach script");
    assert!(detach.contains("select vdisk file=\"Y:\\WinToGo.vhdx\""));
    assert!(detach.contains("detach vdisk"));
}

#[test]
fn test_bcdboot_command_generation_modes() {
    let uefi = generate_bcdboot_args("Y:\\Windows", 'Z', PartitionScheme::GptUefi)
        .expect("UEFI BCDBoot args");
    assert_eq!(uefi, vec!["Y:\\Windows", "/s", "Z:", "/f", "UEFI", "/v"]);

    let bios = generate_bcdboot_args("Y:\\Windows", 'Y', PartitionScheme::MbrBios)
        .expect("BIOS BCDBoot args");
    assert_eq!(bios, vec!["Y:\\Windows", "/s", "Y:", "/f", "BIOS", "/v"]);

    let dual = generate_bcdboot_args("Y:\\Windows", 'Z', PartitionScheme::MbrBiosAndUefi)
        .expect("Dual boot BCDBoot args");
    assert_eq!(dual, vec!["Y:\\Windows", "/s", "Z:", "/f", "ALL", "/v"]);

    // C: is protected and must be rejected
    assert!(generate_bcdboot_args("Y:\\Windows", 'C', PartitionScheme::GptUefi).is_err());

    // Also verify SWM apply args
    let swm_args = generate_dism_apply_args("E:\\sources\\install.swm", 1, "Y:\\", true);
    assert!(swm_args.iter().any(|a| a.starts_with("/SWMFile:") && a.contains("install*.swm")));
}

#[test]
fn test_find_free_drive_letters_avoids_used() {
    // Mask: A(0), B(1), C(2), D(3), Z(25), Y(24) are occupied
    let mask = (1u32 << 0) | (1u32 << 1) | (1u32 << 2) | (1u32 << 3) | (1u32 << 25) | (1u32 << 24);
    let letters = find_free_drive_letters(3, Some(mask)).expect("Should find 3 free letters");
    assert_eq!(letters, vec!['X', 'W', 'V']);
    assert!(!letters.contains(&'C'));
    assert!(!letters.contains(&'D'));
    assert!(!letters.contains(&'Z'));
    assert!(!letters.contains(&'Y'));
}

#[test]
fn test_dry_run_plan_generation_no_side_effects() {
    let usb_disk = make_disk(2, "USB", 64, false, false, vec![]);
    let mut opts = DeployOptions::default();
    opts.mode = DeployMode::WindowsToGo;
    opts.source_path = "C:\\ISO\\Win11.iso".to_string();
    opts.wim_index = 6;
    opts.target_disk_number = 2;
    opts.scheme = PartitionScheme::MbrBiosAndUefi;
    opts.use_vhdx = true;
    opts.vhdx_size_gb = 32;
    opts.bypass_win11_checks = true;

    let plan = build_dry_run_plan(&opts, &usb_disk, None, Some(&['Z', 'Y', 'X']))
        .expect("Dry run plan should succeed");

    assert!(plan.diskpart_script.contains("select disk 2"));
    assert!(plan.diskpart_script.contains("convert mbr"));
    assert!(plan.vhdx_script.as_ref().unwrap().contains("WinToGo.vhdx"));
    assert!(plan.dism_commands.iter().any(|c| c.contains("/Apply-Image") && c.contains("/Index:6")));
    assert!(plan.bcdboot_command.contains("X:\\Windows /s Z: /f ALL /v"));
    assert!(plan
        .registry_tweaks
        .iter()
        .any(|t| t.contains("PortableOperatingSystem")));
    assert!(plan
        .registry_tweaks
        .iter()
        .any(|t| t.contains("BypassTPMCheck")));

    // Verify internal Disk 0 is rejected by dry-run planner too
    let sys_disk = make_disk(0, "SATA", 500, true, true, vec![]);
    assert!(build_dry_run_plan(&opts, &sys_disk, None, Some(&['Z', 'Y', 'X'])).is_err());
}

#[test]
fn test_live_disk_enumeration_no_panic() {
    let disks = enumerate_disks().expect("Read-only disk enumeration should succeed on Windows");
    assert!(!disks.is_empty(), "Expected at least 1 physical disk on host");
    // Every system/boot disk or Disk 0 must be blocked by safety guard
    for d in &disks {
        if d.number == 0 || d.is_system || d.is_boot || !d.bus_type.eq_ignore_ascii_case("USB") {
            assert!(!d.is_safe_usb_target(), "Non-USB or system disk {} leaked safety guard", d.number);
        }
    }
}

#[test]
fn test_elevation_check_no_panic() {
    let _elevated = is_elevated();
}

#[test]
fn test_parse_single_disk_object_and_numeric_bus() {
    let json = r#"{
        "Disks": {"Number": 2, "FriendlyName": "SanDisk Extreme", "BusType": 7, "Size": 64000000000, "PartitionStyle": 2, "IsSystem": false, "IsBoot": false, "IsOffline": false, "IsReadOnly": false, "SerialNumber": "ABC"},
        "Partitions": {"DiskNumber": 2, "PartitionNumber": 1, "DriveLetter": "E", "Size": 64000000000, "Type": "Basic", "IsBoot": false, "IsSystem": false, "IsActive": false},
        "Volumes": {"DriveLetter": "E", "FileSystem": "NTFS", "FileSystemLabel": "WINUSB"}
    }"#;
    let parsed = parse_disks_json(json).expect("Should parse single object JSON");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].bus_type, "USB");
    assert_eq!(parsed[0].partition_style, "GPT");
    assert_eq!(parsed[0].partitions[0].drive_letter, Some('E'));
    assert_eq!(parsed[0].partitions[0].file_system, "NTFS");
    assert!(parsed[0].is_safe_usb_target());

    // Also sanity-check UTF-16LE base64 encoder
    assert_eq!(encode_powershell_command("hi"), "aABpAA==");
}

#[test]
fn test_extract_bfsvc_error_lines() {
    let sample = "\
BFSVC: ServiceBootFiles Mui = 0
BFSVC: Copying boot files from Y:\\Windows\\boot\\EFI to Z:\\EFI\\Microsoft\\Boot\\
BFSVC: Failed to validate boot manager checksum (Y:\\Windows\\Boot\\PCAT\\bootmgr)! Error code = 0xc1
BFSVC: Servicing boot files failed. Error code = 0xc1
BFSVC: Non-error status Error code = 0x0
Failure when attempting to copy boot files.
";

    let extracted = extract_bfsvc_error_lines(sample);
    assert_eq!(extracted.len(), 2);
    assert!(extracted[0].contains("Failed to validate boot manager checksum"));
    assert!(extracted[1].contains("Servicing boot files failed"));
    assert!(is_checksum_or_pe_error(193, sample));
    assert!(is_checksum_or_pe_error(1, sample));
    assert!(!is_checksum_or_pe_error(5, "BFSVC: Access denied."));
}

#[test]
fn test_read_pe_machine_self_executable() {
    let exe_path = std::env::current_exe().expect("current_exe should resolve");
    assert_eq!(read_pe_machine(&exe_path), Some(PE_MACHINE_AMD64));

    assert_eq!(
        read_pe_machine(Path::new("nonexistent_wintousb_binary_12345.exe")),
        None
    );

    let tmp_dir = std::env::temp_dir().join(format!("wintousb_pe_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&tmp_dir);

    let non_pe = tmp_dir.join("not_pe.bin");
    fs::write(&non_pe, b"NOT_A_PE_FILE_1234567890_PADDING_BYTES_000000000000000000000000000000").unwrap();
    assert_eq!(read_pe_machine(&non_pe), None);

    // Construct minimal synthetic ARM64 PE header
    let mut synth = vec![0u8; 0x90];
    synth[0] = b'M';
    synth[1] = b'Z';
    let pe_off: u32 = 0x80;
    synth[0x3C..0x40].copy_from_slice(&pe_off.to_le_bytes());
    synth[0x80..0x84].copy_from_slice(b"PE\0\0");
    synth[0x84..0x86].copy_from_slice(&PE_MACHINE_ARM64.to_le_bytes());
    let arm_pe = tmp_dir.join("arm64.efi");
    fs::write(&arm_pe, &synth).unwrap();
    assert_eq!(read_pe_machine(&arm_pe), Some(PE_MACHINE_ARM64));

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_dry_run_repair_bootloader_no_wipe_and_blocks_c_drive() {
    let usb_disk = make_disk(
        2,
        "USB",
        512,
        false,
        false,
        vec![
            PartitionInfo {
                disk_number: 2,
                partition_number: 1,
                drive_letter: Some('Z'),
                size_bytes: 300 * 1024 * 1024,
                part_type: "Basic".to_string(),
                file_system: "FAT32".to_string(),
                label: "BOOT".to_string(),
                is_boot: false,
                is_system: false,
                is_active: true,
            },
            PartitionInfo {
                disk_number: 2,
                partition_number: 2,
                drive_letter: Some('Y'),
                size_bytes: 500 * 1024 * 1024 * 1024,
                part_type: "Basic".to_string(),
                file_system: "NTFS".to_string(),
                label: "WinToUSB".to_string(),
                is_boot: false,
                is_system: false,
                is_active: false,
            },
        ],
    );

    let mut opts = DeployOptions::default();
    opts.mode = DeployMode::DiskVhdUtility;
    opts.utility_action = UtilityAction::RepairUsbBootloader;
    opts.target_disk_number = 2;
    opts.scheme = PartitionScheme::MbrBiosAndUefi;
    opts.existing_sys_letter = Some('Z');
    opts.existing_win_letter = Some('Y');
    opts.repair_reapply_wtg_tweaks = true;

    let plan = build_dry_run_plan(&opts, &usb_disk, None, Some(&['Z', 'Y', 'X']))
        .expect("Repair bootloader dry-run plan should succeed");

    assert!(plan.diskpart_script.contains("Tidak memformat"));
    assert!(!plan.diskpart_script.to_ascii_lowercase().contains("clean"));
    assert_eq!(plan.bcdboot_command, "bcdboot.exe Y:\\Windows /s Z: /f ALL /v");
    assert!(plan
        .registry_tweaks
        .iter()
        .any(|t| t.contains("PortableOperatingSystem = 1")));

    // Reject C:\ as Windows target
    let mut bad_win = opts.clone();
    bad_win.existing_win_letter = Some('C');
    assert!(build_dry_run_plan(&bad_win, &usb_disk, None, None).is_err());

    // Reject C:\ as System target
    let mut bad_sys = opts.clone();
    bad_sys.existing_sys_letter = Some('C');
    assert!(build_dry_run_plan(&bad_sys, &usb_disk, None, None).is_err());

    // Reject Disk 0 / system disk
    let sys_disk = make_disk(0, "NVMe", 512, true, true, vec![]);
    assert!(build_dry_run_plan(&opts, &sys_disk, None, None).is_err());
}

#[test]
fn test_select_bcdboot_candidates_prefers_target_first() {
    let tmp_root = std::env::temp_dir().join(format!("wintousb_bcd_cand_{}", std::process::id()));
    let target_win = tmp_root.join("target_win");
    let host_win = tmp_root.join("host_win");
    fs::create_dir_all(target_win.join("System32")).unwrap();
    fs::create_dir_all(host_win.join("System32")).unwrap();

    let target_exe = target_win.join("System32").join("bcdboot.exe");
    let host_exe = host_win.join("System32").join("bcdboot.exe");
    fs::write(&host_exe, b"host").unwrap();

    // When target bcdboot.exe does not exist, host comes alone
    let only_host = resolve_bcdboot_candidates_from_dirs(&target_win, &host_win);
    assert_eq!(only_host, vec![host_exe.to_string_lossy().to_string()]);

    // When target bcdboot.exe exists, target is strictly preferred before host
    fs::write(&target_exe, b"target").unwrap();
    let both = resolve_bcdboot_candidates_from_dirs(&target_win, &host_win);
    assert_eq!(
        both,
        vec![
            target_exe.to_string_lossy().to_string(),
            host_exe.to_string_lossy().to_string()
        ]
    );

    // Same directory deduplicates cleanly
    let dedup = resolve_bcdboot_candidates_from_dirs(&host_win, &host_win);
    assert_eq!(dedup.len(), 1);

    // Live helper always returns at least one host candidate
    let live_cands = select_bcdboot_candidates('Z');
    assert!(!live_cands.is_empty());

    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_verify_bcd_enum_retargeted() {
    let ok_en = "\
identifier              {default}
device                  partition=Y:
path                    \\WINDOWS\\system32\\winload.efi
osdevice                partition=Y:
";
    assert!(verify_bcd_enum_retargeted(ok_en, 'Y', 'C'));

    let ok_vhdx = "\
identifier              {default}
device                  vhd=[W:]\\WinToGo.vhdx
osdevice                vhd=[W:]\\WinToGo.vhdx
";
    assert!(verify_bcd_enum_retargeted(ok_vhdx, 'X', 'C'));

    let bad_half = "\
identifier              {default}
device                  partition=Y:
osdevice                partition=C:
";
    assert!(!verify_bcd_enum_retargeted(bad_half, 'Y', 'C'));

    let bad_host = "\
identifier              {default}
device                  partition=C:
osdevice                partition=C:
";
    assert!(!verify_bcd_enum_retargeted(bad_host, 'Y', 'C'));
}
