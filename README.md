# 💾 WinToUSB

**WinToUSB** is a fast, native Rust desktop suite for Windows that creates portable **Windows To Go** drives, performs **live Windows OS hot-cloning** onto USB storage without shutting down, builds **bootable Windows Setup USB** media (with automatic >4 GB `.wim` splitting and Windows 11 hardware bypass), and prepares **`.vhdx` native virtual-disk boot** environments.

Built with pure Rust (`eframe`/`egui`), native Win32 APIs, and Windows deployment primitives (`DISM.exe`, `DiskPart.exe`, `BCDBoot.exe`, WMI Storage, and Volume Shadow Copy `VSS`) with zero runtime bloat and `CREATE_NO_WINDOW` silent execution.

---

## ✨ Key Features

### 1. 🪟 Windows To Go (ISO / WIM / ESD / SWM / VHDX → USB)
- **Direct Image Inspection**: Mounts `.iso` files cleanly using an RAII guard (auto-dismounted on completion, error, or cancellation) and parses all editions inside `install.wim` / `install.esd` / `install.swm` via `DISM /Get-WimInfo`.
- **Two Deployment Modes**:
  - **Direct Partition Mode**: Extracts Windows directly onto the USB NTFS partition for maximum I/O throughput.
  - **VHD / VHDX Native Boot Mode**: Creates an expandable (Dynamic) or Fixed `.vhdx` container on the USB drive, formats it, deploys Windows inside, and configures native VHD boot via `BCDBoot` — ideal for portable snapshots and multi-boot setups.
- **Offline Windows To Go Tuning**:
  - Sets `PortableOperatingSystem = 1` in the offline `SYSTEM` registry hive (`ControlSet001\Control`).
  - Configures USB early-boot storage drivers (`USBSTOR`, `UASPStor`, `USBXHCI`, `USBHUB3`, etc.) to `Start = 0`.
  - Optionally disables hibernation (`HibernateEnabled = 0`) and removes offline `Winre.wim` to save ~500 MB on USB sticks.
  - Injects custom `.inf` drivers recursively via `DISM /Add-Driver`.
  - Optional **Windows 11 Compatibility Bypass** (`LabConfig` TPM 2.0 / SecureBoot / RAM / CPU / Storage checks + `BypassNRO` local account support).

### 2. 🧬 Live OS Hot-Clone (Clone Running Windows → USB)
- Creates a point-in-time **Volume Shadow Copy (VSS)** snapshot (`Win32_ShadowCopy`) of `C:\` while Windows is running.
- Captures the frozen system volume via `DISM /Capture-Image` with an automatic `WimScript.ini` exclusion list (skipping `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, `$Recycle.Bin`, and transient temp folders).
- Deploys directly to USB (or inside a `.vhdx`), applies Windows To Go registry tweaks, installs UEFI/BIOS bootloaders, and cleans up temporary snapshots and WIM artifacts automatically via RAII guards.

### 3. 💿 Bootable Windows Setup USB (Installer Mode)
- Formats target USB to bootable FAT32 and copies Windows Setup files from `.iso`.
- **Automatic >4 GB WIM Splitting**: Automatically detects `install.wim` / `install.esd` larger than 4 GB and splits it with `DISM /Split-Image /FileSize:3800` into `install*.swm` so UEFI boot works cleanly on FAT32 without third-party UEFI NTFS drivers.
- Optional **`autounattend.xml` Windows 11 Bypass**: Injects clean `windowsPE` `LabConfig` checks so clean installs work on non-TPM/legacy hardware.

### 4. 📐 4 Partitioning Schemes + Disk & VHDX Utility
- **GPT untuk UEFI**: 260 MB FAT32 EFI System Partition + NTFS Windows partition (modern UEFI PCs & Macs).
- **MBR untuk BIOS (Legacy)**: Single active NTFS MBR partition.
- **MBR untuk BIOS & UEFI (Dual Boot)**: Active 300 MB FAT32 System/EFI partition + NTFS Windows partition on MBR — boots across both Legacy BIOS and modern UEFI machines!
- **Pertahankan Partisi yang Ada (Keep Existing)**: Deploy onto user-prepared partitions without wiping the physical disk.
- **Disk & VHDX Utility Tab**: Standalone clean formatter (`NTFS`, `FAT32`, `exFAT`), bootable partition layout prep, and `.vhdx` generator.

---

## 🔒 Anti-Data-Loss Safety Architecture

```
                     ┌────────────────────────────────────┐
                     │       Physical Disk Inspector      │
                     │  (WMI / PowerShell JSON Telemetry) │
                     └─────────────────┬──────────────────┘
                                       │
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                        HARD SAFETY GUARDRAILS                               │
│  1. Disk 0 is ALWAYS blocked from destructive wipe/format operations.       │
│  2. IsSystem == true OR IsBoot == true is ALWAYS hard-blocked.              │
│  3. Any disk containing C:\, System, or Boot partitions is ALWAYS blocked.  │
│  4. Internal SATA / NVMe / RAID buses are locked by default.                │
│  5. Read-only / offline disks & undersized targets are rejected pre-flight. │
│  6. Dry-Run Preview generates exact DiskPart/DISM/BCDBoot scripts first.    │
│  7. Destructive wipes require typing the exact Disk # + confirmation box.   │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                   DEPLOYMENT & BOOTLOADER PIPELINE                          │
│  Mount ISO (RAII) ─▶ DiskPart (/s) ─▶ VHDX (optional) ─▶ DISM (Live % Bar)  │
│        ─▶ Driver / Offline Registry Hive Guard ─▶ BCDBoot (UEFI/BIOS/ALL)   │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 🌓 Interface & User Experience

- **Adaptive Light & Dark Themes**: Seamlessly tracks Windows System visuals (`🖥️ Sistem`) or toggles explicitly between `🌙 Gelap` and `☀️ Terang`.
- **On-Demand UAC Elevation**: Launches safely without unexpected UAC loops; displays live token elevation status and provides a one-click **`🛡️ Jalankan sebagai Admin (UAC)`** relaunch button.
- **Real-Time Telemetry**: Live DISM percentage stream (`\r` carriage-return progress parsing), elapsed timer, step tracker, color-coded copyable logs, and cooperative cancellation (`🛑 Batalkan Proses`) with full RAII cleanup.

---

## 🚀 Installation & Running

### Pre-built Binary
Download `WinToUSB-windows-x64.zip` from [GitHub Releases](https://github.com/GlitchWorlds/WinToUSB/releases), extract `WinToUSB.exe`, and launch it (click **Jalankan sebagai Admin (UAC)** when ready to partition/deploy USB drives).

### Build from Source

#### Prerequisites
- Windows 10 / 11 (x64)
- Rust toolchain 1.75+ (`stable-x86_64-pc-windows-gnu` or `msvc`)

#### Build & Test
```powershell
git clone https://github.com/GlitchWorlds/WinToUSB.git
cd WinToUSB
cargo test
cargo build --release
```
The compiled executable will be at `target/release/WinToUSB.exe`.

### Example Usage
1. Launch `WinToUSB.exe` and click **🛡️ Jalankan sebagai Admin (UAC)** if not already elevated.
2. Select mode (e.g., **🪟 Windows To Go**), browse to an image such as `C:\ISO\Win11.iso`, and click **🔍 Analisis Image**.
3. Pick your desired Windows edition index (e.g., `Windows 11 Pro`), choose your external USB drive (e.g., `E:\`), and select **GPT untuk UEFI** or **MBR untuk BIOS & UEFI (Dual Boot)**.
4. Optionally inspect **🔍 Preview Rencana Eksekusi (Dry-Run)**, confirm the target disk number, and click **🚀 Mulai Proses Pembuatan USB**.

---

## 📄 License

MIT License.
