//! Win32 implementations.

use super::{Attrs, DiskSpace, DriveInfo, DriveKind, ShareInfo};
use std::io;
use std::os::windows::fs::MetadataExt;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, NetShareEnum,
    SHARE_INFO_1, STYPE_DISKTREE, STYPE_MASK, STYPE_SPECIAL, STYPE_TEMPORARY,
};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, PWSTR};

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

/// Tells netapi32 to allocate whatever the result needs (MAX_PREFERRED_LENGTH).
const MAX_PREFERRED_LENGTH: u32 = u32::MAX;

/// The browsable disk shares of `server` (no leading `\\`), through `NetShareEnum` level 1.
///
/// Admin and IPC shares (`C$`, `ADMIN$`, `IPC$` — flagged `STYPE_SPECIAL`) are left out;
/// they aren't places anyone browses to.
///
/// **This blocks for as long as the OS takes to give up.** Measured with
/// `examples/probe_netapi.rs`: a dead host parks the calling thread for **21 s** before
/// returning error 53. It can't be cancelled, so the caller must be a blocking-pool task
/// the supervisor is willing to abandon (rules 2 and 7).
pub fn shares(server: &str) -> io::Result<Vec<ShareInfo>> {
    let name = HSTRING::from(format!("\\\\{}", server.trim_start_matches('\\')));
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    // SAFETY: `name` outlives the call and the out-pointers point at live locals. On
    // success netapi32 allocates `buf`, freed below before returning.
    let rc = unsafe {
        NetShareEnum(
            &name,
            1,
            &mut buf,
            MAX_PREFERRED_LENGTH,
            &mut read,
            &mut total,
            None,
        )
    };
    // With MAX_PREFERRED_LENGTH there should be no second page, but a server may still
    // answer ERROR_MORE_DATA; take what it gave us rather than failing.
    if rc != ERROR_SUCCESS.0 && rc != ERROR_MORE_DATA.0 {
        // NetShareEnum returns Win32 codes, so this carries the real message, and
        // `ListError::from` maps 53/67 to "unreachable" and 5 to "access denied".
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    if buf.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success netapi32 returned `read` consecutive SHARE_INFO_1 records.
    let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
    let shares = items
        .iter()
        .filter(|s| {
            let t = s.shi1_type.0;
            t & STYPE_MASK.0 == STYPE_DISKTREE.0
                && t & STYPE_SPECIAL.0 == 0
                && t & STYPE_TEMPORARY.0 == 0
        })
        .map(|s| ShareInfo {
            name: wide(s.shi1_netname),
            remark: wide(s.shi1_remark),
        })
        .collect();
    // SAFETY: `buf` came from netapi32 and nothing borrows it after `shares` is built
    // (the strings above are copied).
    let _ = unsafe { NetApiBufferFree(Some(buf as *const _)) };
    if read != total {
        tracing::debug!(server, read, total, "NetShareEnum returned a partial list");
    }
    Ok(shares)
}

/// The UNC target of a mapped drive letter, or None for a local one.
///
/// This reads the mapping table, so it answers for a *disconnected* mapping too (which is
/// the point: the Locations root shows `S: → \\srv\scans  disconnected`). A local drive
/// answers `ERROR_NOT_CONNECTED` in about 40 µs.
pub fn mapped_target(letter: char) -> Option<String> {
    let local = HSTRING::from(format!("{letter}:"));
    let mut len: u32 = 0;
    // SAFETY: a null buffer with length 0 asks for the required length only.
    let probe = unsafe { WNetGetConnectionW(&local, None, &mut len) };
    if probe != ERROR_MORE_DATA && probe != ERROR_SUCCESS {
        return None; // ERROR_NOT_CONNECTED (2250) for a local drive
    }
    // Guard against an absurd reported length, and leave room for the NUL.
    let cap = (len.max(1) as usize).min(32 * 1024) + 1;
    let mut buf = vec![0u16; cap];
    let mut len = cap as u32;
    // SAFETY: `buf` has `len` writable UTF-16 units and outlives the call.
    let rc = unsafe { WNetGetConnectionW(&local, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let target = String::from_utf16_lossy(&buf[..end]);
    (!target.is_empty()).then_some(target)
}

/// The volume label of a local drive (`Windows`, `Data`), if it has one.
///
/// Only call this for drives that aren't network ones: on a mapped drive it goes to the
/// server and can stall.
pub fn volume_label(letter: char) -> Option<String> {
    let root = HSTRING::from(format!("{letter}:\\"));
    let mut name = [0u16; 256];
    // SAFETY: `root` outlives the call; `name` is a writable buffer of its stated length
    // and every other out-parameter is omitted.
    let ok =
        unsafe { GetVolumeInformationW(&root, Some(&mut name), None, None, None, None).is_ok() };
    if !ok {
        return None;
    }
    let end = name.iter().position(|&c| c == 0).unwrap_or(0);
    let label = String::from_utf16_lossy(&name[..end]);
    (!label.is_empty()).then_some(label)
}

fn wide(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: netapi32 hands back NUL-terminated wide strings inside its own buffer.
    unsafe { p.to_string() }.unwrap_or_default()
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
