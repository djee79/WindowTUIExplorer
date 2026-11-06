//! Win32 implementations.

use super::{Attrs, DiskSpace, DriveInfo, DriveKind};
use std::io;
use std::os::windows::fs::MetadataExt;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR};

// GetDriveTypeW results (WindowsProgramming constants, inlined to avoid a feature).
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

/// Present drive letters. `GetLogicalDrives` reads the mount table and never touches
/// the network, so a dead mapped drive can't stall it.
pub fn drives() -> Vec<DriveInfo> {
    // SAFETY: no arguments, returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let letter = char::from(b'A' + i);
            DriveInfo {
                letter,
                kind: drive_kind(letter),
            }
        })
        .collect()
}

pub fn drive_kind(letter: char) -> DriveKind {
    let root = HSTRING::from(format!("{letter}:\\"));
    // SAFETY: `root` is a valid NUL-terminated wide string that outlives the call.
    match unsafe { GetDriveTypeW(&root) } {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_CDROM => DriveKind::CdRom,
        DRIVE_RAMDISK => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    }
}

/// Free/total bytes for the volume holding `path` (a directory, OS form).
pub fn disk_space(path: &str) -> io::Result<DiskSpace> {
    let mut dir = path.to_string();
    if !dir.ends_with('\\') {
        dir.push('\\');
    }
    let wide = HSTRING::from(dir);
    let mut free = 0u64;
    // SAFETY: valid wide string; out-pointers point at live locals.
    unsafe { GetDiskFreeSpaceExW(&wide, Some(&mut free), None, None) }.map_err(io::Error::other)?;
    Ok(DiskSpace { free })
}

/// `ShellExecuteW("open", path)`: opens a file with its associated program.
pub fn shell_open(path: &str) -> io::Result<()> {
    // ShellExecute may use COM for some handlers. Initializing more than once on a pooled
    // thread just returns S_FALSE, which is fine.
    // SAFETY: no reserved pointer; flags are valid COINIT values.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let file = HSTRING::from(path);
    let verb = HSTRING::from("open");
    // SAFETY: all strings are valid NUL-terminated wide strings that outlive the call.
    let res = unsafe {
        ShellExecuteW(
            None,
            &verb,
            &file,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per the docs, values above 32 mean success; the rest are SE_ERR_* codes.
    let code = res.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(shell_execute_error(code)))
    }
}

fn shell_execute_error(code: isize) -> String {
    match code {
        0 | 8 => "out of memory".to_string(),
        2 => "file not found".to_string(),
        3 => "path not found".to_string(),
        5 => "access denied".to_string(),
        26 => "sharing violation".to_string(),
        27 | 31 => "no program is associated with this file type".to_string(),
        n => format!("ShellExecute failed (code {n})"),
    }
}

pub fn attrs(meta: &std::fs::Metadata) -> Attrs {
    let a = meta.file_attributes();
    Attrs {
        hidden: a & FILE_ATTRIBUTE_HIDDEN != 0,
        system: a & FILE_ATTRIBUTE_SYSTEM != 0,
        readonly: a & FILE_ATTRIBUTE_READONLY != 0,
        directory: a & FILE_ATTRIBUTE_DIRECTORY != 0,
        archive: a & FILE_ATTRIBUTE_ARCHIVE != 0,
    }
}
