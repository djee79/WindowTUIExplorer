//! All OS calls live here so a port stays possible later.
//!
//! Everything in this module may block. Call it from the worker
//! (`spawn_blocking`), never from the UI thread.

mod windows;
pub use self::windows::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    CdRom,
    RamDisk,
    Unknown,
}

impl DriveKind {
    pub fn label(self) -> &'static str {
        match self {
            DriveKind::Fixed => "local disk",
            DriveKind::Removable => "removable",
            DriveKind::Network => "network drive",
            DriveKind::CdRom => "optical",
            DriveKind::RamDisk => "RAM disk",
            DriveKind::Unknown => "drive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriveInfo {
    pub letter: char,
    pub kind: DriveKind,
}

/// One share as `NetShareEnum` reports it.
#[derive(Debug, Clone)]
pub struct ShareInfo {
    /// The share name, without server or separators (`project`).
    pub name: String,
    /// The administrator's description, often empty.
    pub remark: String,
}

#[derive(Debug, Clone, Copy)]
pub struct DiskSpace {
    pub free: u64,
}

/// File attribute bits we care about, read from metadata.
#[derive(Debug, Clone, Copy, Default)]
pub struct Attrs {
    pub hidden: bool,
    pub system: bool,
    pub readonly: bool,
    /// Directory bit, also set on directory symlinks and junctions.
    pub directory: bool,
    pub archive: bool,
}
