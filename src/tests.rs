use crate::deploy::{
    build_dry_run_plan, generate_bcdboot_args, generate_dism_apply_args, DeployMode, DeployOptions,
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
    assert_eq!(uefi, vec!["Y:\\Windows", "/s", "Z:", "/f", "UEFI"]);

    let bios = generate_bcdboot_args("Y:\\Windows", 'Y', PartitionScheme::MbrBios)
        .expect("BIOS BCDBoot args");
    assert_eq!(bios, vec!["Y:\\Windows", "/s", "Y:", "/f", "BIOS"]);

    let dual = generate_bcdboot_args("Y:\\Windows", 'Z', PartitionScheme::MbrBiosAndUefi)
        .expect("Dual boot BCDBoot args");
    assert_eq!(dual, vec!["Y:\\Windows", "/s", "Z:", "/f", "ALL"]);

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
    assert!(plan.bcdboot_command.contains("X:\\Windows /s Z: /f ALL"));
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
