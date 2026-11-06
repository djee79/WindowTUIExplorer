//! Non-Windows dev-host stand-ins. See `mod.rs`.

use super::{Attrs, DiskSpace, DriveInfo, DriveKind};
use std::io;

pub fn drives() -> Vec<DriveInfo> {
    Vec::new()
}

pub fn drive_kind(_letter: char) -> DriveKind {
    DriveKind::Unknown
}

pub fn disk_space(_path: &str) -> io::Result<DiskSpace> {
    Err(io::Error::other("disk space needs Windows"))
}

pub fn shell_open(_path: &str) -> io::Result<()> {
    Err(io::Error::other("opening files needs Windows"))
}

pub fn attrs(meta: &std::fs::Metadata) -> Attrs {
    Attrs {
        readonly: meta.permissions().readonly(),
        directory: meta.is_dir(),
        ..Attrs::default()
    }
}
