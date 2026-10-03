//! Optional VFSI data path for regular-file sources on an NFS mount.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use vnfs::{Error as VfError, Mounted, NfsClient, ReadAllOptions, Result as VfResult};

const MAX_SERVER_COPY_BATCH_FILES: usize = 4096;

fn impl_choice() -> Option<&'static str> {
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some("dummy"),
        Ok("nfs") => Some("nfs"),
        _ => None,
    }
}

fn vf_io_error(e: VfError) -> io::Error {
    if e.is_transport() {
        io::Error::new(io::ErrorKind::ConnectionRefused, e)
    } else {
        e.into()
    }
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
    let directory = if path.is_dir() {
        path.as_path()
    } else {
        path.parent()?
    };
    let mount = vnfs::Nfs::discover_mount(directory).ok()?;
    Some(Mount {
        server: mount.host().to_owned(),
        export: mount.export_root().to_path_buf(),
        point: mount.mount_point().to_path_buf(),
    })
}

enum Backend {
    Dummy(Mounted),
    Nfs { fs: NfsClient, supports_copy: bool },
}

impl Backend {
    fn mapped_path(&self, path: &Path, mount: &Mount) -> io::Result<PathBuf> {
        let absolute = path.canonicalize()?;
        let relative = absolute
            .strip_prefix(&mount.point)
            .map_err(|_| io::Error::other("path is outside VFSI mount"))?;
        match self {
            Self::Dummy(_) => Ok(Path::new("/").join(relative)),
            Self::Nfs { .. } => Ok(Path::new("/").join(relative)),
        }
    }

    fn read_files(&self, paths: &[PathBuf], limit: usize) -> VfResult<Vec<Vec<u8>>> {
        let options = ReadAllOptions::new().max_total_bytes(limit);
        match self {
            Self::Dummy(fs) => fs.read_files_with_options(paths, options),
            Self::Nfs { fs, .. } => fs.read_files_with_options(paths, options),
        }
    }

    fn read_stream(
        &self,
        path: &Path,
        mut callback: impl FnMut(&[u8]) -> VfResult<bool>,
    ) -> VfResult<()> {
        match self {
            Self::Dummy(fs) => fs.read_stream(path, |_, data| callback(data)),
            Self::Nfs { fs, .. } => fs.read_stream(path, |_, data| callback(data)),
        }
        .map(|_| ())
    }

    fn copy_files(&self, pairs: &[(PathBuf, PathBuf)]) -> Option<VfResult<()>> {
        match self {
            Self::Nfs {
                fs,
                supports_copy: true,
            } => Some(fs.copy_files(pairs)),
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
        Some("dummy") => Backend::Dummy(Mounted::new(&mount.point).map_err(vf_io_error)?),
        Some("nfs") => {
            // Inherit and retain the mount's security/version/port and root.
            let fs = vnfs::NfsBuilder::from_mount(&mount.point)
                .map_err(vf_io_error)?
                .connect()
                .map_err(vf_io_error)?;
            let supports_copy = fs
                .capabilities()
                .map_err(vf_io_error)?
                .contains(vnfs::Capabilities::SERVER_COPY);
            Backend::Nfs { fs, supports_copy }
        }
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
    Ok(Path::new("/").join(relative))
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
    Ok(Path::new("/").join(relative))
}

fn server_copy_enabled() -> bool {
    // Direct COPY followed by kernel metadata/rename does not share caches.
    // Keep this experimental path explicit until coherent publication exists.
    std::env::var("VNFS_CP_SERVER_COPY").as_deref() == Ok("1")
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
        if source_mount != target_mount {
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
        pairs.push((
            mounted_vf_path(source, &source_mount)?,
            new_mounted_vf_path(&temp, &target_mount)?,
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
        if let Some(Ok(())) = ctx.backend.copy_files(&pairs) {
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
        // vNFS partitions compounds from negotiated limits. The application
        // owns one aggregate byte budget, including files that grew since stat.
        ctx.prefetched.clear();
        let prefetched_bytes;
        {
            let batch = &selected;
            let Ok(files): io::Result<Vec<PathBuf>> = batch
                .iter()
                .map(|source| ctx.backend.mapped_path(source, &ctx.mount))
                .collect()
            else {
                return Ok(());
            };
            let Ok(contents) = ctx
                .backend
                .read_files(&files, usize::try_from(max_bytes).unwrap_or(usize::MAX))
            else {
                return Ok(());
            };
            prefetched_bytes = contents.iter().map(Vec::len).sum::<usize>();
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
                .is_some_and(|mount| *mount == source_mount)
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
            let pair = (
                mounted_vf_path(source, &source_mount)?,
                mounted_vf_path(dest, dest_mount)?,
            );
            if let Some(result) = ctx.backend.copy_files(std::slice::from_ref(&pair)) {
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
        let source_path = ctx.backend.mapped_path(source, &ctx.mount)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest)?;
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!("[profile] cp_vfsi_method=client_read");
        }
        let mut write_error = None;
        let read_result = ctx.backend.read_stream(&source_path, |data| {
            if let Err(error) = output.write_all(data) {
                write_error = Some(error);
                Ok(false)
            } else {
                Ok(true)
            }
        });
        if let Some(error) = write_error {
            return Err(error);
        }
        read_result.map_err(vf_io_error)?;
        Ok(Some("vfsi-client-read"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_backend_maps_paths_and_streams_without_raw_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"hello world").unwrap();
        let mount = Mount {
            server: "unused".to_string(),
            export: PathBuf::from("/different-server-export"),
            point: root.path().to_path_buf(),
        };
        let backend = Backend::Dummy(Mounted::new(root.path()).unwrap());
        let mapped = backend.mapped_path(&file, &mount).unwrap();
        assert_eq!(mapped, Path::new("/file"));
        assert_eq!(
            backend
                .read_files(std::slice::from_ref(&mapped), 32)
                .unwrap(),
            vec![b"hello world".to_vec()]
        );

        let mut bytes = Vec::new();
        backend
            .read_stream(&mapped, |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(true)
            })
            .unwrap();
        assert_eq!(bytes, b"hello world");
    }
}
