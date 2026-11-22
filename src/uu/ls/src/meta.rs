// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! A filesystem-metadata abstraction so `ls` can render entries sourced
//! either from `std::fs` or (when the `vnfs` feature is enabled) from the
//! vectorized filesystem backend. This keeps the output layer independent of
//! how each entry's attributes were obtained.

#[cfg(all(feature = "vnfs", target_os = "linux"))]
use std::ffi::OsString;
use std::fs::DirEntry;
use std::io;
use std::path::Path;
#[cfg(all(feature = "vnfs", target_os = "linux"))]
use std::path::PathBuf;
#[cfg(any(unix, all(feature = "vnfs", target_os = "linux")))]
use std::time::Duration;
use std::time::SystemTime;
#[cfg(any(unix, all(feature = "vnfs", target_os = "linux")))]
use std::time::UNIX_EPOCH;
#[cfg(all(feature = "vnfs", target_os = "linux"))]
use vnfs::directory::{Attrs as VnfsMetadata, FileType as VfType};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[cfg(unix)]
fn is_block_device(file_type: std::fs::FileType) -> bool {
    file_type.is_block_device()
}

#[cfg(not(unix))]
fn is_block_device(_: std::fs::FileType) -> bool {
    false
}

#[cfg(unix)]
fn is_char_device(file_type: std::fs::FileType) -> bool {
    file_type.is_char_device()
}

#[cfg(not(unix))]
fn is_char_device(_: std::fs::FileType) -> bool {
    false
}

#[cfg(unix)]
fn is_fifo(file_type: std::fs::FileType) -> bool {
    file_type.is_fifo()
}

#[cfg(not(unix))]
fn is_fifo(_: std::fs::FileType) -> bool {
    false
}

#[cfg(unix)]
fn is_socket(file_type: std::fs::FileType) -> bool {
    file_type.is_socket()
}

#[cfg(not(unix))]
fn is_socket(_: std::fs::FileType) -> bool {
    false
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn std_ctime(metadata: &std::fs::Metadata) -> Option<SystemTime> {
    Some(LsMeta::secs_nsecs(
        metadata.ctime(),
        metadata.ctime_nsec() as u32,
    ))
}

#[cfg(not(unix))]
fn std_ctime(_: &std::fs::Metadata) -> Option<SystemTime> {
    None
}

/// A coarse file type, derived from `std::fs::FileType` or an `NFSv4` type code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsFileType {
    Dir,
    File,
    Symlink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
    Unknown,
}

impl LsFileType {
    pub fn from_std(ft: std::fs::FileType) -> Self {
        if ft.is_dir() {
            Self::Dir
        } else if ft.is_file() {
            Self::File
        } else if ft.is_symlink() {
            Self::Symlink
        } else if cfg!(unix) && is_block_device(ft) {
            Self::BlockDevice
        } else if cfg!(unix) && is_char_device(ft) {
            Self::CharDevice
        } else if cfg!(unix) && is_fifo(ft) {
            Self::Fifo
        } else if cfg!(unix) && is_socket(ft) {
            Self::Socket
        } else {
            Self::Unknown
        }
    }

    /// File type reported by the application-facing vnfs API.
    #[cfg(all(feature = "vnfs", target_os = "linux"))]
    pub fn from_vf_type(ftype: VfType) -> Self {
        match ftype {
            VfType::Regular => Self::File,
            VfType::Directory => Self::Dir,
            VfType::Symlink => Self::Symlink,
            VfType::BlockDevice => Self::BlockDevice,
            VfType::CharDevice => Self::CharDevice,
            VfType::Fifo => Self::Fifo,
            VfType::Socket => Self::Socket,
            VfType::Other(_) => Self::Unknown,
        }
    }

    pub fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }
    pub fn is_file(self) -> bool {
        matches!(self, Self::File)
    }
    pub fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink)
    }
    #[cfg(unix)]
    pub fn is_block_device(self) -> bool {
        matches!(self, Self::BlockDevice)
    }
    #[cfg(unix)]
    pub fn is_char_device(self) -> bool {
        matches!(self, Self::CharDevice)
    }
    #[cfg(unix)]
    pub fn is_fifo(self) -> bool {
        matches!(self, Self::Fifo)
    }
    #[cfg(unix)]
    pub fn is_socket(self) -> bool {
        matches!(self, Self::Socket)
    }
}

/// File metadata, either from the local filesystem or from a vectorized
/// backend.
#[derive(Debug, Clone)]
pub enum LsMeta {
    Std(std::fs::Metadata),
    #[cfg(all(feature = "vnfs", target_os = "linux"))]
    Vf(VnfsMetadata),
}

impl LsMeta {
    #[cfg(any(unix, all(feature = "vnfs", target_os = "linux")))]
    fn secs_nsecs(secs: i64, nsecs: u32) -> SystemTime {
        let base = if secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(secs as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs((-secs) as u64)
        };
        base + Duration::from_nanos(nsecs as u64)
    }

    // Keep the sparse-attribute signature even when VFSI is not compiled in.
    #[cfg_attr(
        not(all(feature = "vnfs", target_os = "linux")),
        allow(clippy::unnecessary_wraps)
    )]
    pub fn len(&self) -> Option<u64> {
        match self {
            Self::Std(m) => Some(m.len()),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.len(),
        }
    }

    #[cfg(unix)]
    pub fn nlink(&self) -> u64 {
        match self {
            Self::Std(m) => m.nlink(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.nlink().map_or(1, u64::from),
        }
    }

    #[cfg(unix)]
    pub fn uid(&self) -> u32 {
        match self {
            Self::Std(m) => m.uid(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.uid().unwrap_or(0),
        }
    }

    #[cfg(unix)]
    pub fn gid(&self) -> u32 {
        match self {
            Self::Std(m) => m.gid(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.gid().unwrap_or(0),
        }
    }

    #[cfg(all(unix, not(target_os = "illumos")))]
    pub fn rdev(&self) -> u64 {
        match self {
            Self::Std(m) => m.rdev(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.device_id().unwrap_or(0),
        }
    }

    #[cfg(unix)]
    pub fn ino(&self) -> u64 {
        match self {
            Self::Std(m) => m.ino(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.file_id().unwrap_or(0),
        }
    }

    #[cfg(unix)]
    pub fn blocks(&self) -> u64 {
        match self {
            Self::Std(m) => m.blocks(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.blocks().unwrap_or(0),
        }
    }

    #[cfg(unix)]
    pub fn mode(&self) -> u32 {
        match self {
            Self::Std(m) => m.mode(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.mode().unwrap_or(0),
        }
    }

    pub fn file_type(&self) -> LsFileType {
        match self {
            Self::Std(m) => LsFileType::from_std(m.file_type()),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => LsFileType::from_vf_type(v.file_type()),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn mtime(&self) -> Option<SystemTime> {
        match self {
            Self::Std(m) => m.modified().ok(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.modified(),
        }
    }

    pub fn atime(&self) -> Option<SystemTime> {
        match self {
            Self::Std(m) => m.accessed().ok(),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.accessed(),
        }
    }

    pub fn ctime(&self) -> Option<SystemTime> {
        match self {
            Self::Std(m) => std_ctime(m),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(v) => v.changed(),
        }
    }

    /// The `std::fs::Metadata`, when this is a local entry (used only for
    /// coloring; the vectorized backend cannot reconstruct one).
    #[cfg_attr(
        not(all(feature = "vnfs", target_os = "linux")),
        allow(clippy::unnecessary_wraps)
    )]
    pub fn as_std_metadata(&self) -> Option<&std::fs::Metadata> {
        match self {
            Self::Std(m) => Some(m),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            Self::Vf(_) => None,
        }
    }

    /// Stat `path` with `std::fs`, dereferencing when requested.
    pub fn from_path(path: &Path, dereference: bool) -> io::Result<Self> {
        let md = if dereference {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }?;
        Ok(Self::Std(md))
    }
}

/// A directory entry, either from `std::fs` or from the vectorized backend.
// Keep listing attributes inline to avoid another heap allocation per entry.
#[allow(clippy::large_enum_variant)]
pub enum LsDirEntry {
    Std(DirEntry),
    #[cfg(all(feature = "vnfs", target_os = "linux"))]
    Vf {
        path: PathBuf,
        name: OsString,
        entry: vnfs::directory::DirEntry,
    },
}

/// A directory listing source that abstracts `std::fs::read_dir` over the
/// vectorized backend.
pub struct LsReadDir {
    inner: LsReadDirInner,
}

enum LsReadDirInner {
    Std(std::fs::ReadDir),
    #[cfg(all(feature = "vnfs", target_os = "linux"))]
    Vf(std::vec::IntoIter<LsDirEntry>),
}

impl LsReadDir {
    pub fn from_std(rd: std::fs::ReadDir) -> Self {
        Self {
            inner: LsReadDirInner::Std(rd),
        }
    }

    #[cfg(all(feature = "vnfs", target_os = "linux"))]
    pub fn from_vf(entries: Vec<LsDirEntry>) -> Self {
        Self {
            inner: LsReadDirInner::Vf(entries.into_iter()),
        }
    }
}

impl Iterator for LsReadDir {
    type Item = io::Result<LsDirEntry>;

    fn next(&mut self) -> Option<io::Result<LsDirEntry>> {
        match &mut self.inner {
            LsReadDirInner::Std(rd) => rd.next().map(|r| r.map(LsDirEntry::Std)),
            #[cfg(all(feature = "vnfs", target_os = "linux"))]
            LsReadDirInner::Vf(it) => it.next().map(Ok),
        }
    }
}
