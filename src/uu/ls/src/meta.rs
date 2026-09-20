// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! A filesystem-metadata abstraction so `ls` can render entries sourced
//! either from `std::fs` or (when the `vnfs` feature is enabled) from the
//! vectorized filesystem backend. This keeps the output layer independent of
//! how each entry's attributes were obtained.

#[cfg(feature = "vnfs")]
use std::ffi::OsString;
use std::fs::DirEntry;
use std::io;
use std::path::Path;
#[cfg(feature = "vnfs")]
use std::path::PathBuf;
#[cfg(any(unix, feature = "vnfs"))]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

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
fn std_nlink(metadata: &std::fs::Metadata) -> u64 {
    metadata.nlink()
}

#[cfg(not(unix))]
fn std_nlink(_: &std::fs::Metadata) -> u64 {
    1
}

#[cfg(unix)]
fn std_uid(metadata: &std::fs::Metadata) -> u32 {
    metadata.uid()
}

#[cfg(not(unix))]
fn std_uid(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn std_gid(metadata: &std::fs::Metadata) -> u32 {
    metadata.gid()
}

#[cfg(not(unix))]
fn std_gid(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn std_rdev(metadata: &std::fs::Metadata) -> u64 {
    metadata.rdev()
}

#[cfg(not(unix))]
fn std_rdev(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn std_ino(metadata: &std::fs::Metadata) -> u64 {
    metadata.ino()
}

#[cfg(not(unix))]
fn std_ino(_: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn std_blocks(metadata: &std::fs::Metadata) -> u64 {
    metadata.blocks()
}

#[cfg(not(unix))]
fn std_blocks(metadata: &std::fs::Metadata) -> u64 {
    metadata.len().div_ceil(512)
}

#[cfg(unix)]
fn std_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.mode()
}

#[cfg(not(unix))]
fn std_mode(_: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn std_ctime(metadata: &std::fs::Metadata) -> SystemTime {
    LsMeta::secs_nsecs(metadata.ctime(), metadata.ctime_nsec() as u32)
}

#[cfg(not(unix))]
fn std_ctime(metadata: &std::fs::Metadata) -> SystemTime {
    metadata.created().unwrap_or(UNIX_EPOCH)
}

/// A coarse file type, derived from `std::fs::FileType` or an NFSv4 type code.
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

    /// NFSv4 `NF4*` file type code (see `nfsv41_sys`).
    #[cfg(feature = "vnfs")]
    pub fn from_ftype(ftype: u32) -> Self {
        match ftype {
            1 => Self::File,        // NF4REG
            2 => Self::Dir,         // NF4DIR
            5 => Self::Symlink,     // NF4LNK
            6 => Self::BlockDevice, // NF4BLK
            7 => Self::CharDevice,  // NF4CHR
            8 => Self::Fifo,        // NF4FIFO
            9 => Self::Socket,      // NF4SOCK
            _ => Self::Unknown,
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
    pub fn is_block_device(self) -> bool {
        matches!(self, Self::BlockDevice)
    }
    pub fn is_char_device(self) -> bool {
        matches!(self, Self::CharDevice)
    }
    pub fn is_fifo(self) -> bool {
        matches!(self, Self::Fifo)
    }
    pub fn is_socket(self) -> bool {
        matches!(self, Self::Socket)
    }
}

/// File metadata, either from the local filesystem or from a vectorized
/// backend.
#[derive(Debug, Clone)]
pub enum LsMeta {
    Std(std::fs::Metadata),
    #[cfg(feature = "vnfs")]
    Vf(vnfs::VfAttrs),
}

impl LsMeta {
    #[cfg(any(unix, feature = "vnfs"))]
    fn secs_nsecs(secs: i64, nsecs: u32) -> SystemTime {
        let base = if secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(secs as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs((-secs) as u64)
        };
        base + Duration::from_nanos(nsecs as u64)
    }

    pub fn len(&self) -> u64 {
        match self {
            Self::Std(m) => m.len(),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::SIZE) => v.size,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn nlink(&self) -> u64 {
        match self {
            Self::Std(m) => std_nlink(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::NLINK) => v.nlink as u64,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 1,
        }
    }

    pub fn uid(&self) -> u32 {
        match self {
            Self::Std(m) => std_uid(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::UID) => v.uid,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn gid(&self) -> u32 {
        match self {
            Self::Std(m) => std_gid(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::GID) => v.gid,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn rdev(&self) -> u64 {
        match self {
            Self::Std(m) => std_rdev(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::RDEV) => v.rdev,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn ino(&self) -> u64 {
        match self {
            Self::Std(m) => std_ino(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::FILEID) => v.fileid,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn blocks(&self) -> u64 {
        match self {
            Self::Std(m) => std_blocks(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::BLOCKS) => v.blocks,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn mode(&self) -> u32 {
        match self {
            Self::Std(m) => std_mode(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::MODE) => v.mode,
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => 0,
        }
    }

    pub fn file_type(&self) -> LsFileType {
        match self {
            Self::Std(m) => LsFileType::from_std(m.file_type()),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) => LsFileType::from_ftype(v.ftype.as_nfs()),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn mtime(&self) -> SystemTime {
        match self {
            Self::Std(m) => m.modified().unwrap_or(UNIX_EPOCH),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::MTIME) => {
                Self::secs_nsecs(v.mtime_sec, v.mtime_nsec)
            }
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => UNIX_EPOCH,
        }
    }

    pub fn atime(&self) -> SystemTime {
        match self {
            Self::Std(m) => m.accessed().unwrap_or(UNIX_EPOCH),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::ATIME) => {
                Self::secs_nsecs(v.atime_sec, v.atime_nsec)
            }
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => UNIX_EPOCH,
        }
    }

    pub fn ctime(&self) -> SystemTime {
        match self {
            Self::Std(m) => std_ctime(m),
            #[cfg(feature = "vnfs")]
            Self::Vf(v) if v.returned.contains(vnfs::AttrMask::CTIME) => {
                Self::secs_nsecs(v.ctime_sec, v.ctime_nsec)
            }
            #[cfg(feature = "vnfs")]
            Self::Vf(_) => UNIX_EPOCH,
        }
    }

    /// The `std::fs::Metadata`, when this is a local entry (used only for
    /// coloring; the vectorized backend cannot reconstruct one).
    #[cfg_attr(not(feature = "vnfs"), allow(clippy::unnecessary_wraps))]
    pub fn as_std_metadata(&self) -> Option<&std::fs::Metadata> {
        match self {
            Self::Std(m) => Some(m),
            #[cfg(feature = "vnfs")]
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
pub enum LsDirEntry {
    Std(DirEntry),
    #[cfg(feature = "vnfs")]
    Vf {
        path: PathBuf,
        name: OsString,
        attrs: vnfs::VfAttrs,
    },
}

/// A directory listing source that abstracts `std::fs::read_dir` over the
/// vectorized backend.
pub struct LsReadDir {
    inner: LsReadDirInner,
}

enum LsReadDirInner {
    Std(std::fs::ReadDir),
    #[cfg(feature = "vnfs")]
    Vf(std::vec::IntoIter<LsDirEntry>),
}

impl LsReadDir {
    pub fn from_std(rd: std::fs::ReadDir) -> Self {
        Self {
            inner: LsReadDirInner::Std(rd),
        }
    }

    #[cfg(feature = "vnfs")]
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
            #[cfg(feature = "vnfs")]
            LsReadDirInner::Vf(it) => it.next().map(Ok),
        }
    }
}
