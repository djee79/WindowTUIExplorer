//! Win32 implementations.

use super::{Attrs, DiskSpace, DriveInfo, DriveKind, FindEntry, ShareInfo};
use std::io;
use std::os::windows::fs::MetadataExt;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::{
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, WIN32_FIND_DATAW,
};
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

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Lists `dir` (OS form, `\\?\`-prefixed when long) with `FindFirstFileExW`:
/// `FindExInfoBasic` (no 8.3 names) + `FIND_FIRST_EX_LARGE_FETCH`. Measured by
/// `examples/enum_bench.rs` on SMB with big directories: 38 % faster than
/// `ignore::WalkParallel`, 10 % faster than `read_dir`. Blocks; walker threads only.
pub fn find_entries(dir: &str) -> io::Result<Vec<FindEntry>> {
    let pattern = HSTRING::from(format!("{}\\*", dir.trim_end_matches('\\')));
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` outlives the call; `data` is the WIN32_FIND_DATAW that
    // FindExInfoBasic fills.
    let handle = unsafe {
        FindFirstFileExW(
            &pattern,
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut std::ffi::c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }
    // HRESULT_FROM_WIN32: the Win32 code is the low 16 bits.
    .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    let mut out = Vec::new();
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&data.cFileName[..len]);
        if name != "." && name != ".." {
            let a = data.dwFileAttributes;
            out.push(FindEntry {
                name,
                size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
                modified: filetime(data.ftLastWriteTime),
                created: filetime(data.ftCreationTime),
                attrs: attrs_from_raw(a),
                reparse: a & FILE_ATTRIBUTE_REPARSE_POINT != 0,
            });
        }
        // SAFETY: `handle` is a live find handle and `data` is writable.
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            break; // ERROR_NO_MORE_FILES (or a mid-listing error: keep what we have)
        }
    }
    // SAFETY: `handle` came from FindFirstFileExW and is closed exactly once.
    let _ = unsafe { FindClose(handle) };
    Ok(out)
}

/// FILETIME (100 ns ticks since 1601) → SystemTime; zero means "not set".
fn filetime(ft: FILETIME) -> Option<std::time::SystemTime> {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    const EPOCH_DIFF_TICKS: u64 = 11_644_473_600 * 10_000_000;
    let since_unix = ticks.checked_sub(EPOCH_DIFF_TICKS)?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(
        since_unix.checked_mul(100)?,
    ))
}

fn attrs_from_raw(a: u32) -> Attrs {
    Attrs {
        hidden: a & FILE_ATTRIBUTE_HIDDEN != 0,
        system: a & FILE_ATTRIBUTE_SYSTEM != 0,
        readonly: a & FILE_ATTRIBUTE_READONLY != 0,
        directory: a & FILE_ATTRIBUTE_DIRECTORY != 0,
        archive: a & FILE_ATTRIBUTE_ARCHIVE != 0,
    }
}

fn wide(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: netapi32 hands back NUL-terminated wide strings inside its own buffer.
    unsafe { p.to_string() }.unwrap_or_default()
}

pub fn attrs(meta: &std::fs::Metadata) -> Attrs {
    attrs_from_raw(meta.file_attributes())
}

/// One page of a PDF, rendered by Windows (`Windows.Data.Pdf`, built into Windows 10+).
pub struct PdfPage {
    /// The page as a PNG. Asked `width_dip` wide in device-independent pixels, so its real
    /// width includes the display scaling (e.g. 540 px for 300 at 180 %).
    pub png: Vec<u8>,
    /// 0-based; clamped to the document.
    pub index: u32,
    pub count: u32,
    /// The page size in device-independent pixels (1/96 inch), rotation applied.
    pub width_dip: f32,
    pub height_dip: f32,
}

/// Renders page `index` of the PDF in `bytes` to a PNG `width` DIPs wide. Works from memory,
/// so the file read (the part that can hang on a share) stays with the caller's timeout.
pub fn pdf_page(bytes: &[u8], index: u32, width: u32) -> io::Result<PdfPage> {
    use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
    use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    let err = |e: windows::core::Error| io::Error::other(e.message());
    // WinRT needs the thread initialized; a blocking-pool thread may not be yet.
    // S_FALSE / RPC_E_CHANGED_MODE mean it already is, which is fine.
    // SAFETY: plain initialization call, no pointers.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };

    let input = InMemoryRandomAccessStream::new().map_err(err)?;
    let writer = DataWriter::CreateDataWriter(&input).map_err(err)?;
    writer.WriteBytes(bytes).map_err(err)?;
    writer.StoreAsync().map_err(err)?.join().map_err(err)?;
    writer.FlushAsync().map_err(err)?.join().map_err(err)?;
    writer.DetachStream().map_err(err)?;
    input.Seek(0).map_err(err)?;

    let doc = PdfDocument::LoadFromStreamAsync(&input)
        .map_err(err)?
        .join()
        .map_err(err)?;
    let count = doc.PageCount().map_err(err)?;
    if count == 0 {
        return Err(io::Error::other("the PDF has no pages"));
    }
    let index = index.min(count - 1);
    let page = doc.GetPage(index).map_err(err)?;
    let size = page.Size().map_err(err)?;
    let options = PdfPageRenderOptions::new().map_err(err)?;
    options.SetDestinationWidth(width.max(1)).map_err(err)?;
    let output = InMemoryRandomAccessStream::new().map_err(err)?;
    page.RenderWithOptionsToStreamAsync(&output, &options)
        .map_err(err)?
        .join()
        .map_err(err)?;

    let len = u32::try_from(output.Size().map_err(err)?)
        .map_err(|_| io::Error::other("rendered page too big"))?;
    let reader =
        DataReader::CreateDataReader(&output.GetInputStreamAt(0).map_err(err)?).map_err(err)?;
    reader.LoadAsync(len).map_err(err)?.join().map_err(err)?;
    let mut png = vec![0u8; len as usize];
    reader.ReadBytes(&mut png).map_err(err)?;
    Ok(PdfPage {
        png,
        index,
        count,
        width_dip: size.Width,
        height_dip: size.Height,
    })
}

/// A thumbnail as Windows Explorer would show it, in RGBA.
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The shell thumbnail of `path` (`IShellItemImageFactory`), at most `max_px` on its long
/// side. Works for anything an installed thumbnail handler covers (Office files, videos,
/// HEIC, DWG with Autodesk's handler…). No icon fallback: a file without a handler is an
/// error. Windows reads the file itself, so the caller times the whole call.
pub fn shell_thumbnail(path: &str, max_px: u32) -> io::Result<Thumbnail> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
        SIIGBF_THUMBNAILONLY,
    };

    /// Frees the bitmap however this function returns.
    struct Owned(HBITMAP);
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the bitmap came from GetImage and is ours to delete, once.
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
        }
    }

    let err = |e: windows::core::Error| io::Error::other(e.message());
    // The shell wants COM; S_FALSE / RPC_E_CHANGED_MODE (already initialized) are fine.
    // SAFETY: no reserved pointer; flags are valid COINIT values.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let wide = HSTRING::from(path);
    // SAFETY: `wide` is a valid NUL-terminated string that outlives the call.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&wide, None) }.map_err(err)?;
    let side = i32::try_from(max_px.clamp(16, 2560)).unwrap_or(256);
    // SAFETY: a plain COM call; the returned bitmap is owned below.
    let bitmap = unsafe {
        factory.GetImage(
            SIZE { cx: side, cy: side },
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .map_err(err)?;
    let bitmap = Owned(bitmap);

    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a BITMAP and we pass its exact size.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        return Err(io::Error::other("unreadable thumbnail bitmap"));
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // Negative: rows top to bottom.
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let (wu, hu) = (w.unsigned_abs(), h.unsigned_abs());
    let mut pixels = vec![0u8; wu as usize * hu as usize * 4];
    // SAFETY: a memory DC, freed right after; `pixels` holds w×h 32-bit pixels, which is
    // what `info` asks for.
    let lines = unsafe {
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            dc,
            bitmap.0,
            0,
            hu,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        lines
    };
    if lines == 0 {
        return Err(io::Error::other("couldn't read the thumbnail pixels"));
    }
    bgra_premultiplied_to_rgba(&mut pixels);
    Ok(Thumbnail {
        width: wu,
        height: hu,
        rgba: pixels,
    })
}

/// Shell bitmaps are BGRA with premultiplied alpha; a bitmap without alpha has 0 alpha
/// everywhere. Turns that into straight RGBA in place.
fn bgra_premultiplied_to_rgba(px: &mut [u8]) {
    let no_alpha = px.chunks_exact(4).all(|p| p[3] == 0);
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        if no_alpha {
            p[3] = 255;
        } else if p[3] > 0 && p[3] < 255 {
            let a = u32::from(p[3]);
            for c in &mut p[..3] {
                *c = (u32::from(*c) * 255 / a).min(255) as u8;
            }
        }
    }
}

/// The users logged on to `host` (`NetWkstaUserEnum`, level 1; needs admin rights on it),
/// as `user` names, machine accounts and repeats included (see `machines::people`).
///
/// **Blocks** like every NetApi call: a dead host parks the thread for ~20 s. Run it on the
/// blocking pool under a timeout, after the port-445 probe said the host is up.
pub fn logged_on_users(host: &str) -> io::Result<Vec<String>> {
    use windows::Win32::NetworkManagement::NetManagement::{NetWkstaUserEnum, WKSTA_USER_INFO_1};
    let name = HSTRING::from(format!("\\\\{}", host.trim_start_matches('\\')));
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    // SAFETY: `name` outlives the call; the out-pointers point at live locals. On success
    // netapi32 allocates `buf`, freed below.
    let rc = unsafe {
        NetWkstaUserEnum(
            &name,
            1,
            Some(&mut buf),
            MAX_PREFERRED_LENGTH,
            Some(&mut read),
            &mut total,
            None,
        )
    };
    if rc != ERROR_SUCCESS.0 && rc != ERROR_MORE_DATA.0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    if buf.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success netapi32 returned `read` consecutive WKSTA_USER_INFO_1 records.
    let items =
        unsafe { std::slice::from_raw_parts(buf as *const WKSTA_USER_INFO_1, read as usize) };
    let users = items.iter().map(|u| wide(u.wkui1_username)).collect();
    // SAFETY: `buf` came from netapi32 and is freed exactly once.
    let _ = unsafe { NetApiBufferFree(Some(buf.cast())) };
    Ok(users)
}

// ------------------------------------------------------------------- SMB connections

/// A live connection, as `net use` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConnection {
    /// `\\server\share`.
    pub remote: String,
    /// `P:` for a mapped drive.
    pub local: Option<String>,
    /// The account it was made with, when Windows says.
    pub user: Option<String>,
}

/// A NUL-terminated UTF-16 copy of `s`, for the W APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Connects to `remote` (`\\server\share`, or `\\server\IPC$` for the server itself) as
/// `user` with `password` (NUL-terminated UTF-16; the caller wipes it), or with the
/// current login when both are None. `CONNECT_TEMPORARY`, no drive letter: the
/// connection lasts until logoff and robocopy, Explorer and other programs reuse it.
/// Errors are Win32 codes (see `auth::error_message`). Can block ~20 s on a dead host.
pub fn net_connect(remote: &str, user: Option<&str>, password: Option<&[u16]>) -> Result<(), u32> {
    use windows::Win32::NetworkManagement::WNet::{
        CONNECT_TEMPORARY, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
    };
    let mut remote_w = to_wide(remote);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(remote_w.as_mut_ptr()),
        ..Default::default()
    };
    let user_w = user.map(to_wide);
    let user_p = user_w
        .as_ref()
        .map_or(PCWSTR::null(), |u| PCWSTR(u.as_ptr()));
    let pass_p = password.map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));
    // SAFETY: every pointer is to a NUL-terminated buffer alive for the whole call.
    let rc = unsafe { WNetAddConnection2W(&resource, pass_p, user_p, CONNECT_TEMPORARY) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(rc.0)
    }
}

/// Drops the connection to `remote` (`\\server\share` or a drive letter `P:`). Not
/// forced: Windows refuses while files are open (code 2401).
pub fn net_disconnect(remote: &str) -> Result<(), u32> {
    use windows::Win32::NetworkManagement::WNet::{NET_CONNECT_FLAGS, WNetCancelConnection2W};
    let name = to_wide(remote);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let rc = unsafe { WNetCancelConnection2W(PCWSTR(name.as_ptr()), NET_CONNECT_FLAGS(0), false) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(rc.0)
    }
}

/// Every live connection of this logon session (`WNetOpenEnum`, `RESOURCE_CONNECTED`),
/// with the account each one uses (`WNetGetUser`).
pub fn net_connections() -> Result<Vec<NetConnection>, u32> {
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, HANDLE};
    use windows::Win32::NetworkManagement::WNet::{
        NETRESOURCEW, RESOURCE_CONNECTED, RESOURCETYPE_ANY, WNET_OPEN_ENUM_USAGE, WNetCloseEnum,
        WNetEnumResourceW, WNetGetUserW, WNetOpenEnumW,
    };
    let mut handle = HANDLE::default();
    // SAFETY: `handle` receives the enumeration handle, closed below.
    let rc = unsafe {
        WNetOpenEnumW(
            RESOURCE_CONNECTED,
            RESOURCETYPE_ANY,
            WNET_OPEN_ENUM_USAGE(0),
            None,
            &mut handle,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(rc.0);
    }
    let mut out = Vec::new();
    // 16 KB per batch, as the docs suggest; u64 storage keeps NETRESOURCEW aligned.
    let mut buf = vec![0u64; 2048];
    let result = loop {
        let mut count = u32::MAX;
        let mut size = (buf.len() * 8) as u32;
        // SAFETY: `buf` is `size` bytes; Windows fills it with `count` NETRESOURCEW
        // records followed by the strings they point to.
        let rc =
            unsafe { WNetEnumResourceW(handle, &mut count, buf.as_mut_ptr().cast(), &mut size) };
        if rc == ERROR_NO_MORE_ITEMS {
            break Ok(());
        }
        if rc != ERROR_SUCCESS {
            break Err(rc.0);
        }
        let records = buf.as_ptr().cast::<NETRESOURCEW>();
        for i in 0..count as usize {
            // SAFETY: Windows wrote `count` records at the start of `buf`.
            let r = unsafe { &*records.add(i) };
            let text = |p: PWSTR| -> Option<String> {
                // SAFETY: a non-null pointer here is a NUL-terminated string inside `buf`.
                (!p.is_null())
                    .then(|| unsafe { p.to_string() }.ok())
                    .flatten()
            };
            let Some(remote) = text(r.lpRemoteName) else {
                continue;
            };
            let local = text(r.lpLocalName).filter(|l| !l.is_empty());
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let remote_w = to_wide(&remote);
            // SAFETY: `name` holds `len` UTF-16 units; `remote_w` is NUL-terminated.
            let rc = unsafe {
                WNetGetUserW(
                    PCWSTR(remote_w.as_ptr()),
                    PWSTR(name.as_mut_ptr()),
                    &mut len,
                )
            };
            let user = (rc == ERROR_SUCCESS)
                .then(|| {
                    let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
                    String::from_utf16_lossy(&name[..end])
                })
                .filter(|u| !u.is_empty());
            out.push(NetConnection {
                remote,
                local,
                user,
            });
        }
    };
    // SAFETY: closing the handle WNetOpenEnumW gave us, once.
    let _ = unsafe { WNetCloseEnum(handle) };
    result.map(|()| out)
}

/// "Remember in Windows Credential Manager": a `CRED_TYPE_DOMAIN_PASSWORD` entry for
/// `target` (`server`, or `*.domain` for every machine of a domain). Windows then uses it
/// by itself, in sentier and everywhere else. `password` is UTF-16 without the NUL; the
/// caller wipes it.
pub fn cred_write(target: &str, user: &str, password: &[u16]) -> io::Result<()> {
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_DOMAIN_PASSWORD, CREDENTIALW, CredWriteW,
    };
    let mut target_w = to_wide(target);
    let mut user_w = to_wide(user);
    let cred = CREDENTIALW {
        Type: CRED_TYPE_DOMAIN_PASSWORD,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(password.len() * 2)
            .map_err(|_| io::Error::other("password too long"))?,
        CredentialBlob: password.as_ptr().cast_mut().cast(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user_w.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer in `cred` is valid for the call; Windows copies the blob.
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| io::Error::other(e.message()))
}

/// robocopy's piped output is in the console's OEM code page (`é` is 0x82 under 437/850),
/// whatever `/UNICODE` says. Decodes it.
pub fn oem_to_string(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
    const CP_OEMCP: u32 = 1;
    if bytes.is_empty() {
        return String::new();
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: both buffers are valid slices; the first call only measures.
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, None) };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; n as usize];
    // SAFETY: `wide` holds the `n` units the first call asked for.
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, Some(&mut wide)) };
    String::from_utf16_lossy(&wide[..n.max(0) as usize])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid PDF with blank pages of the given sizes in points (1/72 inch), with a
    /// correct cross-reference table.
    pub(crate) fn tiny_pdf(pages: &[(u32, u32)]) -> Vec<u8> {
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                (0..pages.len())
                    .map(|i| format!("{} 0 R", i + 3))
                    .collect::<Vec<_>>()
                    .join(" "),
                pages.len()
            ),
        ];
        for (w, h) in pages {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] >>"
            ));
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for off in offsets {
            out.extend(format!("{off:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn renders_a_pdf_page_from_memory() {
        // A4 portrait (595×842 pt), then A3 landscape (1191×842 pt).
        let pdf = tiny_pdf(&[(595, 842), (1191, 842)]);
        let page = match pdf_page(&pdf, 0, 300) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        };
        assert_eq!((page.index, page.count), (0, 2));
        // Points × 96/72 = DIPs.
        assert!((page.width_dip - 793.3).abs() < 1.0, "{}", page.width_dip);
        assert!(
            (page.height_dip - 1122.7).abs() < 1.0,
            "{}",
            page.height_dip
        );
        let Ok(img) = image::load_from_memory(&page.png) else {
            panic!("not an image")
        };
        // The width is in device-independent pixels: Windows applies the display
        // scaling (≥ 100 %), so the PNG is at least that wide, with the page's shape.
        assert!(img.width() >= 300, "{}", img.width());
        let shape = f64::from(img.height()) / f64::from(img.width());
        assert!((shape - 842.0 / 595.0).abs() < 0.02, "{shape}");

        let Ok(last) = pdf_page(&pdf, 9, 100) else {
            panic!()
        };
        assert_eq!(last.index, 1, "clamped to the last page");
        assert!(last.width_dip > last.height_dip, "landscape");

        assert!(pdf_page(b"not a pdf", 0, 100).is_err());
    }

    #[test]
    fn bgra_premultiplied_becomes_rgba() {
        // Opaque blue, half-transparent premultiplied red (128,0,0,128), and a bitmap
        // without alpha.
        let mut px = vec![255, 0, 0, 255, 0, 0, 128, 128];
        bgra_premultiplied_to_rgba(&mut px);
        assert_eq!(px, [0, 0, 255, 255, 255, 0, 0, 128]);
        let mut no_alpha = vec![10, 20, 30, 0];
        bgra_premultiplied_to_rgba(&mut no_alpha);
        assert_eq!(no_alpha, [30, 20, 10, 255]);
    }

    #[test]
    fn shell_thumbnail_of_a_png() {
        let dir = std::env::temp_dir().join(format!("sentier-thumb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("wide.png");
        let img = image::RgbaImage::from_pixel(600, 300, image::Rgba([0, 128, 255, 255]));
        assert!(img.save(&path).is_ok());
        let got = shell_thumbnail(&path.to_string_lossy(), 256);
        let missing = shell_thumbnail(&dir.join("nope.png").to_string_lossy(), 256);
        let _ = std::fs::remove_dir_all(&dir);
        let th = match got {
            Ok(t) => t,
            Err(e) => panic!("{e}"),
        };
        assert!(
            th.width > th.height,
            "keeps the shape: {}×{}",
            th.width,
            th.height
        );
        assert_eq!(th.rgba.len(), (th.width * th.height * 4) as usize);
        assert!(missing.is_err());
    }
}
