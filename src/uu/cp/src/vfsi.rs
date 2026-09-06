//! Optional VFSI data path for regular-file sources on an NFS mount.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
use vnfs::{ReadOp, VecFs, VfFile, VfPathBase};

fn impl_choice() -> Option<&'static str> {
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some("dummy"),
        Ok("nfs") => Some("nfs"),
        _ => None,
    }
}

fn vf_io_error(e: vnfs::VfError) -> io::Error {
    let kind = match &e {
        vnfs::VfError::Transport { .. } => io::ErrorKind::ConnectionRefused,
        vnfs::VfError::Op { err_no, .. } => match *err_no {
            vnfs::ERR_NOENT => io::ErrorKind::NotFound,
            vnfs::ERR_ACCES => io::ErrorKind::PermissionDenied,
            vnfs::ERR_EXIST => io::ErrorKind::AlreadyExists,
            vnfs::ERR_INVAL => io::ErrorKind::InvalidInput,
            vnfs::ERR_NOTDIR => io::ErrorKind::NotADirectory,
            vnfs::ERR_ISDIR => io::ErrorKind::IsADirectory,
            _ => io::ErrorKind::Other,
        },
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mount {
    server: String,
    point: PathBuf,
}

fn existing_ancestor(path: &Path) -> &Path {
    let mut candidate = path;
    while !candidate.exists() {
        let Some(parent) = candidate.parent() else {
            break;
        };
        candidate = parent;
    }
    candidate
}

fn nfs_mount(path: &Path) -> Option<Mount> {
    let path = existing_ancestor(path)
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let spec = fields.next()?;
            let point = PathBuf::from(fields.next()?);
            let fstype = fields.next()?;
            if (fstype != "nfs" && fstype != "nfs4") || !path.starts_with(&point) {
                return None;
            }
            let server = spec.rsplit_once(':')?.0.trim_matches(['[', ']']).to_owned();
            Some(Mount { server, point })
        })
        .max_by_key(|mount| mount.point.as_os_str().len())
}

enum Backend {
    Dummy(DummyVecFs),
    Nfs(NfsVecFs),
}

impl Backend {
    fn open_readonly(&mut self, path: &Path) -> vnfs::VfResult<VfFile> {
        match self {
            Self::Dummy(fs) => fs.open_by_path(VfPathBase::Abs, path, libc::O_RDONLY, 0),
            Self::Nfs(fs) => fs.open_by_path(VfPathBase::Abs, path, libc::O_RDONLY, 0),
        }
    }

    fn close(&mut self, file: &VfFile) -> vnfs::VfResult<()> {
        match self {
            Self::Dummy(fs) => fs.close(file),
            Self::Nfs(fs) => fs.close(file),
        }
    }

    fn readv(&mut self, reads: &[ReadOp]) -> vnfs::VfResult<Vec<vnfs::ReadResult>> {
        match self {
            Self::Dummy(fs) => fs.readv(reads),
            Self::Nfs(fs) => fs.readv(reads),
        }
    }
}

struct Context {
    mount: Mount,
    backend: Backend,
}

thread_local! {
    static CTX: RefCell<Option<Context>> = const { RefCell::new(None) };
}

fn make_context(mount: Mount) -> io::Result<Context> {
    let backend = match impl_choice() {
        Some("dummy") => Backend::Dummy(DummyVecFs::new(mount.point.clone())),
        Some("nfs") => Backend::Nfs(NfsVecFs::connect(&mount.server).map_err(vf_io_error)?),
        _ => return Err(io::Error::other("VNFS_IMPL disabled")),
    };
    Ok(Context { mount, backend })
}

/// Read a regular file through VFSI and write it through the kernel. Keeping
/// the destination on one client avoids incoherent kernel/direct-NFS caches.
/// Returns `false` when the normal kernel path should be used.
pub fn try_copy(source: &Path, dest: &Path) -> io::Result<bool> {
    if impl_choice().is_none() {
        return Ok(false);
    }
    let Some(source_mount) = nfs_mount(source) else {
        return Ok(false);
    };
    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_none_or(|ctx| ctx.mount != source_mount) {
            *slot = Some(make_context(source_mount.clone())?);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        let source_rel = source.strip_prefix(&ctx.mount.point).unwrap_or(source);
        let source_path = Path::new("/").join(source_rel);
        let source_file = ctx
            .backend
            .open_readonly(&source_path)
            .map_err(vf_io_error)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest)?;
        let mut offset = 0;
        let copy_result: io::Result<()> = (|| {
            loop {
                let op = ReadOp::at(source_file.clone(), offset, 1024 * 1024);
                let mut results = ctx
                    .backend
                    .readv(std::slice::from_ref(&op))
                    .map_err(vf_io_error)?;
                let result = results.pop().expect("one VFSI read result");
                output.write_all(&result.data)?;
                offset += result.data.len() as u64;
                if result.eof {
                    break;
                }
            }
            Ok(())
        })();
        let close_result = ctx.backend.close(&source_file).map_err(vf_io_error);
        copy_result?;
        close_result?;
        Ok(true)
    })
}
