// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! A filesystem-metadata abstraction so `ls` can render entries sourced
//! either from `std::fs` or (when the `vnfs` feature is enabled) from the
//! vectorized filesystem backend. This keeps the output layer independent of
//! how each entry's attributes were obtained.

use std::ffi::OsString;
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

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
    pub fn from_std(ft: &std::fs::FileType) -> LsFileType {
        if ft.is_dir() {
            LsFileType::Dir
        } else if ft.is_file() {
            LsFileType::File
        } else if ft.is_symlink() {
            LsFileType::Symlink
        } else if ft.is_block_device() {
            LsFileType::BlockDevice
        } else if ft.is_char_device() {
            LsFileType::CharDevice
        } else if ft.is_fifo() {
            LsFileType::Fifo
        } else if ft.is_socket() {
            LsFileType::Socket
        } else {
            LsFileType::Unknown
        }
    }

    /// NFSv4 `NF4*` file type code (see `nfsv41_sys`).
    #[cfg(feature = "vnfs")]
    pub fn from_ftype(ftype: u32) -> LsFileType {
        match ftype {
            1 => LsFileType::File,        // NF4REG
            2 => LsFileType::Dir,         // NF4DIR
            5 => LsFileType::Symlink,     // NF4LNK
            6 => LsFileType::BlockDevice, // NF4BLK
            7 => LsFileType::CharDevice,  // NF4CHR
            8 => LsFileType::Fifo,        // NF4FIFO
            9 => LsFileType::Socket,      // NF4SOCK
            _ => LsFileType::Unknown,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, LsFileType::Dir)
    }
    pub fn is_file(&self) -> bool {
        matches!(self, LsFileType::File)
    }
    pub fn is_symlink(&self) -> bool {
        matches!(self, LsFileType::Symlink)
    }
    pub fn is_block_device(&self) -> bool {
        matches!(self, LsFileType::BlockDevice)
    }
    pub fn is_char_device(&self) -> bool {
        matches!(self, LsFileType::CharDevice)
    }
    pub fn is_fifo(&self) -> bool {
        matches!(self, LsFileType::Fifo)
    }
    pub fn is_socket(&self) -> bool {
        matches!(self, LsFileType::Socket)
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
            LsMeta::Std(m) => m.len(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::SIZE) => v.size,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn nlink(&self) -> u64 {
        match self {
            LsMeta::Std(m) => m.nlink(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::NLINK) => v.nlink as u64,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 1,
        }
    }

    pub fn uid(&self) -> u32 {
        match self {
            LsMeta::Std(m) => m.uid(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::UID) => v.uid,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn gid(&self) -> u32 {
        match self {
            LsMeta::Std(m) => m.gid(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::GID) => v.gid,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn rdev(&self) -> u64 {
        match self {
            LsMeta::Std(m) => m.rdev(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::RDEV) => v.rdev,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn ino(&self) -> u64 {
        match self {
            LsMeta::Std(m) => m.ino(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::FILEID) => v.fileid,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn blocks(&self) -> u64 {
        match self {
            LsMeta::Std(m) => m.blocks(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::BLOCKS) => v.blocks,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn mode(&self) -> u32 {
        match self {
            LsMeta::Std(m) => m.mode(),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::MODE) => v.mode,
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => 0,
        }
    }

    pub fn file_type(&self) -> LsFileType {
        match self {
            LsMeta::Std(m) => LsFileType::from_std(&m.file_type()),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) => LsFileType::from_ftype(v.ftype.as_nfs()),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub fn mtime(&self) -> SystemTime {
        match self {
            LsMeta::Std(m) => Self::secs_nsecs(m.mtime(), m.mtime_nsec() as u32),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::MTIME) => {
                Self::secs_nsecs(v.mtime_sec, v.mtime_nsec)
            }
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => UNIX_EPOCH,
        }
    }

    pub fn atime(&self) -> SystemTime {
        match self {
            LsMeta::Std(m) => Self::secs_nsecs(m.atime(), m.atime_nsec() as u32),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::ATIME) => {
                Self::secs_nsecs(v.atime_sec, v.atime_nsec)
            }
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => UNIX_EPOCH,
        }
    }

    pub fn ctime(&self) -> SystemTime {
        match self {
            LsMeta::Std(m) => Self::secs_nsecs(m.ctime(), m.ctime_nsec() as u32),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(v) if v.returned.contains(vnfs::AttrMask::CTIME) => {
                Self::secs_nsecs(v.ctime_sec, v.ctime_nsec)
            }
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => UNIX_EPOCH,
        }
    }

    /// The `std::fs::Metadata`, when this is a local entry (used only for
    /// coloring; the vectorized backend cannot reconstruct one).
    pub fn as_std_metadata(&self) -> Option<&std::fs::Metadata> {
        match self {
            LsMeta::Std(m) => Some(m),
            #[cfg(feature = "vnfs")]
            LsMeta::Vf(_) => None,
        }
    }

    /// Stat `path` with `std::fs`, dereferencing when requested.
    pub fn from_path(path: &Path, dereference: bool) -> std::io::Result<LsMeta> {
        let md = if dereference {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }?;
        Ok(LsMeta::Std(md))
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
    pub fn from_std(rd: std::fs::ReadDir) -> LsReadDir {
        LsReadDir {
            inner: LsReadDirInner::Std(rd),
        }
    }

    #[cfg(feature = "vnfs")]
    pub fn from_vf(entries: Vec<LsDirEntry>) -> LsReadDir {
        LsReadDir {
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
