//! Optional VFSI data path for regular-file sources on an NFS mount.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
use vnfs::{ExtentPair, ReadOp, VecFs, VfFile, VfPathBase};

const READ_ALL_FILES_PER_BATCH: usize = 8;
const MAX_SERVER_COPY_BATCH_FILES: usize = 4096;

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
    export: PathBuf,
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
            let (server, export) = spec.rsplit_once(':')?;
            let server = server.trim_matches(['[', ']']).to_owned();
            Some(Mount {
                server,
                export: PathBuf::from(export),
                point,
            })
        })
        .max_by_key(|mount| mount.point.as_os_str().len())
}

enum Backend {
    Dummy(DummyVecFs),
    Nfs { fs: NfsVecFs, supports_copy: bool },
}

impl Backend {
    fn open_readonly(&mut self, path: &Path) -> vnfs::VfResult<VfFile> {
        match self {
            Self::Dummy(fs) => fs.open_by_path(VfPathBase::Abs, path, libc::O_RDONLY, 0),
            Self::Nfs { fs, .. } => fs.open_by_path(VfPathBase::Abs, path, libc::O_RDONLY, 0),
        }
    }

    fn close(&mut self, file: &VfFile) -> vnfs::VfResult<()> {
        match self {
            Self::Dummy(fs) => fs.close(file),
            Self::Nfs { fs, .. } => fs.close(file),
        }
    }

    fn readv(&mut self, reads: &[ReadOp]) -> vnfs::VfResult<Vec<vnfs::ReadResult>> {
        match self {
            Self::Dummy(fs) => fs.readv(reads),
            Self::Nfs { fs, .. } => fs.readv(reads),
        }
    }

    fn read_allv(&mut self, files: &[VfFile]) -> vnfs::VfResult<Vec<Vec<u8>>> {
        match self {
            Self::Dummy(fs) => fs.read_allv(files),
            Self::Nfs { fs, .. } => fs.read_allv(files),
        }
    }

    fn copyv(&mut self, pairs: &[ExtentPair]) -> Option<vnfs::VfResult<()>> {
        match self {
            Self::Nfs {
                fs,
                supports_copy: true,
            } => Some(fs.copyv(pairs)),
            _ => None,
        }
    }
}

struct PreparedCopy {
    dest: PathBuf,
    temp: PathBuf,
}

struct Context {
    mount: Mount,
    backend: Backend,
    prefetched: HashMap<PathBuf, Vec<u8>>,
    prepared_copies: HashMap<PathBuf, PreparedCopy>,
}

impl Drop for Context {
    fn drop(&mut self) {
        for prepared in self.prepared_copies.values() {
            let _ = std::fs::remove_file(&prepared.temp);
        }
    }
}

thread_local! {
    static CTX: RefCell<Option<Context>> = const { RefCell::new(None) };
}

fn make_context(mount: Mount) -> io::Result<Context> {
    let backend = match impl_choice() {
        Some("dummy") => Backend::Dummy(DummyVecFs::new(mount.point.clone())),
        Some("nfs") => match NfsVecFs::connect_minor(&mount.server, 2) {
            Ok(fs) => Backend::Nfs {
                fs,
                supports_copy: true,
            },
            Err(_) => Backend::Nfs {
                fs: NfsVecFs::connect(&mount.server).map_err(vf_io_error)?,
                supports_copy: false,
            },
        },
        _ => return Err(io::Error::other("VNFS_IMPL disabled")),
    };
    Ok(Context {
        mount,
        backend,
        prefetched: HashMap::new(),
        prepared_copies: HashMap::new(),
    })
}

fn mounted_vf_path(path: &Path, mount: &Mount) -> io::Result<PathBuf> {
    let absolute = path.canonicalize()?;
    let relative = absolute
        .strip_prefix(&mount.point)
        .map_err(|_| io::Error::other("path is outside VFSI mount"))?;
    Ok(mount.export.join(relative))
}

fn new_mounted_vf_path(path: &Path, mount: &Mount) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("destination has no parent"))?
        .canonicalize()?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("destination has no file name"))?;
    let absolute = parent.join(name);
    let relative = absolute
        .strip_prefix(&mount.point)
        .map_err(|_| io::Error::other("path is outside VFSI mount"))?;
    Ok(mount.export.join(relative))
}

fn server_copy_enabled() -> bool {
    std::env::var("VNFS_CP_SERVER_COPY").as_deref() != Ok("0")
}

fn prepare_server_copy_batch(sources: &[PathBuf], target: &Path) -> io::Result<bool> {
    if !server_copy_enabled()
        || sources.len() < 2
        || sources.len() > MAX_SERVER_COPY_BATCH_FILES
        || !target.is_dir()
    {
        return Ok(false);
    }
    let Some(target_mount) = nfs_mount(target) else {
        return Ok(false);
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seen_destinations = HashSet::new();
    let mut plans = Vec::with_capacity(sources.len());
    let mut pairs = Vec::with_capacity(sources.len());
    for (i, source) in sources.iter().enumerate() {
        if !source.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Ok(false);
        }
        let Some(source_mount) = nfs_mount(source) else {
            return Ok(false);
        };
        if source_mount.server != target_mount.server {
            return Ok(false);
        }
        let Some(name) = source.file_name() else {
            return Ok(false);
        };
        let dest = target.join(name);
        if dest.exists() || !seen_destinations.insert(dest.clone()) {
            return Ok(false);
        }
        let temp = target.join(format!(".vfsi-cp-{}-{nonce}-{i}", std::process::id()));
        pairs.push(ExtentPair::from_os_paths(
            &mounted_vf_path(source, &source_mount)?,
            0,
            &new_mounted_vf_path(&temp, &target_mount)?,
            0,
            None,
        ));
        plans.push((source.clone(), PreparedCopy { dest, temp }));
    }

    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        let source_mount = nfs_mount(&sources[0]).expect("validated NFS source");
        if slot.as_ref().is_none_or(|ctx| ctx.mount != source_mount) {
            let Ok(context) = make_context(source_mount) else {
                return Ok(false);
            };
            *slot = Some(context);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        if let Some(Ok(())) = ctx.backend.copyv(&pairs) {
            ctx.prepared_copies.extend(plans);
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                let copied = pairs.len();
                eprintln!("[profile] cp_vfsi_batched_server_copies={copied}");
            }
            Ok(true)
        } else {
            for (_, prepared) in plans {
                let _ = std::fs::remove_file(prepared.temp);
            }
            Ok(false)
        }
    })
}

/// Prefetch a bounded group of command-line sources in a vectorized read.
/// Large files stay on the streaming path so batching cannot consume
/// unbounded memory.
pub fn prepare_batch(sources: &[PathBuf], target: &Path) -> io::Result<()> {
    const DEFAULT_BATCH_BYTES: u64 = 64 * 1024 * 1024;
    const MAX_BATCH_FILES: usize = 4096;

    if impl_choice().is_none() || sources.len() < 2 {
        return Ok(());
    }
    if prepare_server_copy_batch(sources, target)? {
        return Ok(());
    }
    let max_bytes = std::env::var("VNFS_CP_BATCH_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BATCH_BYTES);
    if max_bytes == 0 {
        return Ok(());
    }

    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut total = 0u64;
    let mut selected_mount = None;
    for source in sources {
        if !seen.insert(source.clone()) {
            continue;
        }
        let Ok(metadata) = source.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > max_bytes.saturating_sub(total) {
            continue;
        }
        let Some(mount) = nfs_mount(source) else {
            continue;
        };
        if selected_mount
            .as_ref()
            .is_some_and(|chosen| chosen != &mount)
        {
            continue;
        }
        selected_mount.get_or_insert_with(|| mount.clone());
        total += metadata.len();
        selected.push(source.clone());
        if selected.len() == MAX_BATCH_FILES || total == max_bytes {
            break;
        }
    }
    if selected.len() < 2 {
        return Ok(());
    }
    let mount = selected_mount.expect("selected VFSI sources have a mount");

    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_none_or(|ctx| ctx.mount != mount) {
            let Ok(context) = make_context(mount.clone()) else {
                return Ok(());
            };
            *slot = Some(context);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        // Keep each read-all vector modest: path OPEN/READ/CLOSE sequences
        // also consume the server's negotiated compound-operation budget.
        let mut prefetched_bytes = 0usize;
        for batch in selected.chunks(READ_ALL_FILES_PER_BATCH) {
            let Ok(files): io::Result<Vec<VfFile>> = batch
                .iter()
                .map(|source| mounted_vf_path(source, &ctx.mount).map(|p| VfFile::from_os_path(&p)))
                .collect()
            else {
                return Ok(());
            };
            let Ok(contents) = ctx.backend.read_allv(&files) else {
                return Ok(());
            };
            prefetched_bytes += contents.iter().map(Vec::len).sum::<usize>();
            ctx.prefetched.extend(batch.iter().cloned().zip(contents));
        }
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            let selected_files = selected.len();
            eprintln!(
                "[profile] cp_prefetch_files={selected_files} cp_prefetch_bytes={prefetched_bytes}"
            );
        }
        Ok(())
    })
}

/// Read a regular file through VFSI and write it through the kernel. Keeping
/// the destination on one client avoids incoherent kernel/direct-NFS caches.
/// Returns `None` when the normal kernel path should be used, otherwise the
/// VFSI method name used for `--debug` output.
pub fn try_copy(source: &Path, dest: &Path) -> io::Result<Option<&'static str>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(source_mount) = nfs_mount(source) else {
        return Ok(None);
    };
    let dest_mount = nfs_mount(dest);
    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_none_or(|ctx| ctx.mount != source_mount) {
            *slot = Some(make_context(source_mount.clone())?);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        if let Some(prepared) = ctx.prepared_copies.remove(source) {
            if prepared.dest == dest {
                std::fs::rename(&prepared.temp, dest)?;
                if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                    eprintln!("[profile] cp_vfsi_method=batched_server_copy");
                }
                return Ok(Some("vfsi-batched-server-copy"));
            }
            let _ = std::fs::remove_file(prepared.temp);
        }
        if server_copy_enabled()
            && dest_mount
                .as_ref()
                .is_some_and(|mount| mount.server == source_mount.server)
        {
            // Create/truncate through the kernel first so cp retains its normal
            // destination-creation semantics and the mount has no stale tail.
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(dest)?;
            let dest_mount = dest_mount.as_ref().expect("same-server NFS destination");
            let pair = ExtentPair::from_os_paths(
                &mounted_vf_path(source, &source_mount)?,
                0,
                &mounted_vf_path(dest, dest_mount)?,
                0,
                None,
            );
            if let Some(result) = ctx.backend.copyv(std::slice::from_ref(&pair)) {
                match result {
                    Ok(()) => {
                        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                            eprintln!("[profile] cp_vfsi_method=server_copy");
                        }
                        return Ok(Some("vfsi-server-copy"));
                    }
                    Err(error) => {
                        // COPY is an optional v4.2 facility. Re-truncate and
                        // use the established client-side VFSI path when an
                        // export or server declines it.
                        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                            eprintln!("[profile] cp_vfsi_server_copy_fallback={error}");
                        }
                    }
                }
            }
        }
        if let Some(contents) = ctx.prefetched.remove(source) {
            let mut output = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(dest)?;
            output.write_all(&contents)?;
            return Ok(Some("vfsi-client-read"));
        }
        let source_path = mounted_vf_path(source, &ctx.mount)?;
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
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!("[profile] cp_vfsi_method=client_read");
        }
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
        Ok(Some("vfsi-client-read"))
    })
}
