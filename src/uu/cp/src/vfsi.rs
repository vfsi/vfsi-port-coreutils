//! Optional VFSI data path for regular-file sources on an NFS mount.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use uucore::vnfs::{NfsMount as Mount, Session as Backend, mount_root, same_mount};

use vnfs::helpers::{PathMapper, ResolvePath};
#[cfg(test)]
use vnfs::posix::Posix;

const MAX_BATCH_FILES: usize = 4096;
// Whole-file prefetch retains the complete payload until cp reaches each operand.
const MAX_PREFETCH_FILE_BYTES: usize = 1024 * 1024;

fn impl_choice() -> Option<&'static str> {
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some("dummy"),
        Ok("nfs") => Some("nfs"),
        _ => None,
    }
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
    uucore::vnfs::nfs_mount(existing_ancestor(path))
}

struct PreparedCopy {
    dest: PathBuf,
    temp: PathBuf,
}

struct Context {
    backend: Backend,
    prefetched: HashMap<PathBuf, Vec<u8>>,
    prepared_copies: HashMap<PathBuf, PreparedCopy>,
}

impl Context {
    fn clear_batch(&mut self) {
        self.prefetched.clear();
        for (_, prepared) in self.prepared_copies.drain() {
            let _ = std::fs::remove_file(prepared.temp);
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.clear_batch();
    }
}

/// Clear staged copies when the command returns, including skipped operands.
pub(super) struct BatchGuard;

impl Drop for BatchGuard {
    fn drop(&mut self) {
        CTX.with(|slot| {
            if let Some(context) = slot.borrow_mut().as_mut() {
                context.clear_batch();
            }
        });
    }
}

pub(super) fn eligible_file(metadata: &std::fs::Metadata, sparse: super::SparseMode) -> bool {
    metadata.is_file()
        && match sparse {
            super::SparseMode::Never => true,
            super::SparseMode::Auto => !super::platform::metadata_is_sparse(metadata),
            super::SparseMode::Always => false,
        }
}

thread_local! {
    static CTX: RefCell<Option<Context>> = const { RefCell::new(None) };
}

fn make_context(mount: Mount) -> io::Result<Context> {
    let backend = Backend::open(&mount)?;
    Ok(Context {
        backend,
        prefetched: HashMap::new(),
        prepared_copies: HashMap::new(),
    })
}

fn configured_batch_bytes() -> Option<u64> {
    std::env::var("VNFS_CP_BATCH_BYTES").ok()?.parse().ok()
}

fn server_copy_enabled() -> bool {
    // Direct COPY followed by kernel metadata/rename does not share caches.
    // Keep this experimental path explicit until coherent publication exists.
    std::env::var("VNFS_CP_SERVER_COPY").as_deref() == Ok("1")
}

// Only a proven unsubmitted input permits another copy method. A definite
// failure can include partial effects; an uncertain failure can still be live.
fn server_copy_completed(outcome: vnfs::OpResult<()>) -> Result<bool, io::Error> {
    match outcome {
        Ok(()) => Ok(true),
        Err(error) if error.state() == vnfs::error::FailureState::NotAttempted => Ok(false),
        Err(error) => Err(error.into_cause()),
    }
}

fn retain_server_copies(
    prepared: &mut HashMap<PathBuf, PreparedCopy>,
    plans: Vec<(PathBuf, PreparedCopy)>,
    outcomes: vnfs::VecResult<()>,
) -> io::Result<bool> {
    // An outer rejection guarantees that no input COPY was dispatched.
    let Ok(outcomes) = outcomes else {
        return Ok(false);
    };
    if outcomes.len() != plans.len() {
        for (_, copy) in plans {
            let _ = std::fs::remove_file(copy.temp);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid server-copy outcome count",
        ));
    }
    let mut first_error = None;
    let mut copied = false;
    for (index, ((source, copy), outcome)) in plans.into_iter().zip(outcomes).enumerate() {
        match server_copy_completed(outcome) {
            Ok(true) => {
                copied = true;
                prepared.insert(source, copy);
            }
            Ok(false) => {}
            Err(error) => {
                // These are private, uniquely named staging paths. Cleanup is
                // best effort and does not claim unknown effects were undone.
                let _ = std::fs::remove_file(&copy.temp);
                first_error.get_or_insert_with(|| {
                    vnfs::error::IndexedError::at(index, error).with_context("copy", source)
                });
            }
        }
    }
    match first_error {
        Some(error) => {
            // The command will not publish this failed batch. Release known
            // completed staging files now; process exit may skip TLS Drop.
            for (_, copy) in prepared.drain() {
                let _ = std::fs::remove_file(copy.temp);
            }
            Err(error.into_cause())
        }
        None => Ok(copied),
    }
}

fn prepare_server_copy_batch(
    sources: &[PathBuf],
    target: &Path,
    all_sources_independent: &mut impl FnMut() -> bool,
) -> io::Result<bool> {
    if !server_copy_enabled() || !target.is_dir() {
        return Ok(false);
    }
    let Some(target_mount) = nfs_mount(target).and_then(mount_root) else {
        return Ok(false);
    };
    if !all_sources_independent() {
        return Ok(false);
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seen_destinations = HashSet::new();
    let mut plans = Vec::with_capacity(sources.len());
    let mut pairs = Vec::with_capacity(sources.len());
    let Ok(paths) = PathMapper::new(target_mount.mount_point()) else {
        return Ok(false);
    };
    for (i, source) in sources.iter().enumerate() {
        if !source.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Ok(false);
        }
        let Some(source_mount) = nfs_mount(source) else {
            return Ok(false);
        };
        if !same_mount(&source_mount, &target_mount) || !readable_regular_file(source) {
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
        let (Ok(mapped_source), Ok(mapped_temp)) = (
            paths.map(source, ResolvePath::Follow),
            paths.map(&temp, ResolvePath::NoFollow),
        ) else {
            return Ok(false);
        };
        pairs.push((mapped_source, mapped_temp));
        plans.push((source.clone(), PreparedCopy { dest, temp }));
    }

    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|ctx| !ctx.backend.matches(&target_mount))
        {
            let Ok(context) = make_context(target_mount) else {
                return Ok(false);
            };
            *slot = Some(context);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        let Some(outcomes) = ctx.backend.try_server_copy(&pairs) else {
            return Ok(false);
        };
        let copied = retain_server_copies(&mut ctx.prepared_copies, plans, outcomes)?;
        if copied && std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!(
                "[profile] cp_vfsi_batched_server_copies={}",
                ctx.prepared_copies.len()
            );
        }
        Ok(copied)
    })
}

/// Prefetch a bounded group of command-line sources in a vectorized read.
/// Large files stay on the streaming path so batching cannot consume
/// unbounded memory.
pub fn prepare_batch(
    sources: &[PathBuf],
    target: &Path,
    is_independent: impl Fn(&Path) -> bool,
) -> io::Result<()> {
    const DEFAULT_BATCH_BYTES: u64 = 64 * 1024 * 1024;

    if impl_choice().is_none() || !(2..=MAX_BATCH_FILES).contains(&sources.len()) {
        return Ok(());
    }
    // An earlier overwrite can modify a later source through a hard link.
    // Check all operands, even if only some are selected for prefetch.
    let mut independent = None;
    let mut all_sources_independent =
        || *independent.get_or_insert_with(|| sources.iter().all(|source| is_independent(source)));
    if prepare_server_copy_batch(sources, target, &mut all_sources_independent)? {
        return Ok(());
    }
    let configured_budget = configured_batch_bytes();
    let max_bytes = configured_budget.unwrap_or(DEFAULT_BATCH_BYTES);
    let max_file_bytes = if configured_budget.is_some() {
        usize::MAX
    } else {
        MAX_PREFETCH_FILE_BYTES
    };
    if max_bytes == 0 {
        return Ok(());
    }

    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut parents = HashMap::new();
    let mut selected_mount = None;
    for source in sources {
        if !seen.insert(source.clone()) {
            continue;
        }
        let Some(parent) = source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .or(Some(Path::new(".")))
        else {
            continue;
        };
        let mount = parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| uucore::vnfs::nfs_mount(parent));
        let Some(mount) = mount.as_ref() else {
            continue;
        };
        if selected_mount
            .as_ref()
            .is_some_and(|chosen| !same_mount(chosen, mount))
        {
            continue;
        }
        selected_mount.get_or_insert_with(|| mount.clone());
        selected.push(source.clone());
    }
    if selected.len() < 2 {
        return Ok(());
    }
    // A pair cannot prefetch if either operand is already known to be
    // ineligible. Reuse kernel metadata for this check before connecting.
    if configured_budget.is_none()
        && selected.len() == 2
        && selected.iter().any(|path| {
            path.symlink_metadata().is_ok_and(|metadata| {
                !metadata.is_file() || metadata.len() > MAX_PREFETCH_FILE_BYTES as u64
            })
        })
    {
        return Ok(());
    }
    let Some(mount) = mount_root(selected_mount.expect("selected VFSI sources have a mount"))
    else {
        return Ok(());
    };
    if !all_sources_independent() {
        return Ok(());
    }

    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_none_or(|ctx| !ctx.backend.matches(&mount)) {
            let Ok(context) = make_context(mount.clone()) else {
                return Ok(());
            };
            *slot = Some(context);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        let Ok(prefetched) = prefetch_regular_files(
            &ctx.backend,
            &selected,
            usize::try_from(max_bytes).unwrap_or(usize::MAX),
            max_file_bytes,
        ) else {
            return Ok(());
        };
        if !prefetched.is_empty() && std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            let prefetched_files = prefetched.len();
            let prefetched_bytes = prefetched.values().map(Vec::len).sum::<usize>();
            eprintln!(
                "[profile] cp_prefetch_files={prefetched_files} cp_prefetch_bytes={prefetched_bytes}"
            );
        }
        ctx.prefetched = prefetched;
        Ok(())
    })
}

fn prefetch_regular_files(
    backend: &Backend,
    sources: &[PathBuf],
    budget: usize,
    max_file_bytes: usize,
) -> vnfs::Result<HashMap<PathBuf, Vec<u8>>> {
    use vnfs::directory::Attributes;
    // No-follow metadata admits only real regular files. Symlinks retain cp's
    // ordinary follow policy and are handled by its existing singular path.
    let paths = backend.map_many(sources, ResolvePath::NoFollow)?;
    let attrs = backend.attrs_nofollow(&paths, Attributes::MODE | Attributes::SIZE)?;
    if attrs.len() != paths.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cp: invalid attribute count",
        ));
    }
    let mut remaining = budget;
    let mut selected = Vec::new();
    let mut operands = Vec::new();
    for ((source, path), attrs) in sources.iter().zip(paths).zip(attrs) {
        attrs.require(Attributes::MODE | Attributes::SIZE)?;
        if !attrs.is_file() {
            continue;
        }
        let Some(size) = attrs.len().and_then(|size| usize::try_from(size).ok()) else {
            continue;
        };
        if size > max_file_bytes || size > remaining {
            continue;
        }
        remaining -= size;
        selected.push(source.clone());
        operands.push(path);
    }
    if selected.len() < 2 {
        return Ok(HashMap::new());
    }
    let data = backend.read_files(&operands, budget)?;
    if data.len() != selected.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cp: invalid read count",
        ));
    }
    Ok(selected.into_iter().zip(data).collect())
}

fn readable_regular_file(path: &Path) -> bool {
    OpenOptions::new()
        .read(true)
        // A regular path can be replaced with a FIFO after the initial stat.
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| file.metadata())
        .is_ok_and(|metadata| metadata.is_file())
}

struct DeferredOutput<'a> {
    path: &'a Path,
    file: Option<File>,
    create_new: bool,
    attempted: bool,
}

impl<'a> DeferredOutput<'a> {
    fn new(path: &'a Path, create_new: bool) -> Self {
        Self {
            path,
            file: None,
            create_new,
            attempted: false,
        }
    }

    fn open(&mut self) -> io::Result<&mut File> {
        self.attempted = true;
        if self.file.is_none() {
            let mut options = OpenOptions::new();
            options.write(true).mode(0o600);
            if self.create_new {
                options.create_new(true);
            } else {
                options.create(true).truncate(true);
            }
            self.file = Some(options.open(self.path)?);
        }
        Ok(self.file.as_mut().expect("copy destination is open"))
    }
}

impl Write for DeferredOutput<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.open()?.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

fn copy_to_destination(backend: &Backend, source: &Path, dest: &Path) -> io::Result<bool> {
    let mut output = DeferredOutput::new(dest, false);
    match backend.copy_to_writer(source, &mut output) {
        Ok(_) => {
            // A readable empty source still creates or truncates its destination.
            output.open()?;
            Ok(true)
        }
        Err(_) if !output.attempted => Ok(false),
        Err(error) => Err(error),
    }
}

/// Read a regular file through VFSI and write it through the kernel. Keeping
/// the destination on one client avoids incoherent kernel/direct-NFS caches.
/// Returns `None` when the normal kernel path should be used, otherwise the
/// VFSI method name used for `--debug` output.
pub fn try_copy(source: &Path, dest: &Path, source_size: u64) -> io::Result<Option<&'static str>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let prefetched = CTX.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .and_then(|ctx| ctx.prefetched.remove(source))
    });
    if let Some(contents) = prefetched {
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest)?;
        output.write_all(&contents)?;
        return Ok(Some("vfsi-client-read"));
    }
    if source_size > MAX_PREFETCH_FILE_BYTES as u64
        && configured_batch_bytes().is_none()
        && !CTX.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|context| context.prepared_copies.contains_key(source))
        })
    {
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") && nfs_mount(source).is_some() {
            eprintln!("[profile] cp_vfsi_method=kernel_large");
        }
        return Ok(None);
    }
    let Some(source_mount) = nfs_mount(source) else {
        return Ok(None);
    };
    let Some(source_mount) = mount_root(source_mount) else {
        return Ok(None);
    };
    CTX.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|ctx| !ctx.backend.matches(&source_mount))
        {
            let Ok(context) = make_context(source_mount.clone()) else {
                return Ok(None);
            };
            *slot = Some(context);
        }
        let ctx = slot.as_mut().expect("VFSI context initialized");
        if let Some(prepared) = ctx.prepared_copies.remove(source) {
            if prepared.dest == dest {
                if let Err(error) = std::fs::rename(&prepared.temp, dest) {
                    let _ = std::fs::remove_file(&prepared.temp);
                    return Err(error);
                }
                if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                    eprintln!("[profile] cp_vfsi_method=batched_server_copy");
                }
                return Ok(Some("vfsi-batched-server-copy"));
            }
            let _ = std::fs::remove_file(prepared.temp);
        }
        let Ok(source_path) = ctx.backend.map(source, ResolvePath::Follow) else {
            return Ok(None);
        };
        if copy_to_destination(&ctx.backend, &source_path, dest)? {
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!("[profile] cp_vfsi_method=client_read");
            }
            Ok(Some("vfsi-client-read"))
        } else {
            Ok(None)
        }
    })
}

/// Experimental direct-to-destination batching. Explicit opt-in keeps ordinary
/// cp failure ordering unchanged. No fallback is permitted after output starts.
pub fn try_round_robin(
    sources: &[PathBuf],
    target: &Path,
    options: &super::Options,
) -> Option<io::Result<()>> {
    let budget = std::env::var("VNFS_CP_STREAM_BYTES")
        .ok()?
        .parse::<usize>()
        .ok()
        .and_then(std::num::NonZeroUsize::new)?;
    if impl_choice().is_none() || !plain_stream_options(options) || sources.len() < 2 {
        return None;
    }
    let entries = stream_entries(sources, target, options)?;
    // Admission requires one identical parent and rejects source symlinks.
    // Resolve that directory once instead of rediscovering it for every file.
    let mount = mount_root(nfs_mount(stream_parent(&sources[0]))?)?;
    let backend = Backend::open(&mount).ok()?;
    let paths = stream_source_paths(&backend, sources)?;
    copy_stream_entries(&backend, &paths, &entries, budget)
}

fn stream_source_paths(backend: &Backend, sources: &[PathBuf]) -> Option<Vec<PathBuf>> {
    // Check the whole operand list before the first chunk can create outputs.
    // A failed preflight leaves the native loop free to copy all readable files.
    if !sources.iter().all(|source| readable_regular_file(source)) {
        return None;
    }
    backend.map_many(sources, ResolvePath::Follow).ok()
}

fn plain_stream_options(options: &super::Options) -> bool {
    use super::{BackupMode, ClobberMode, CopyMode, OverwriteMode, ReflinkMode, SparseMode};
    options.copy_mode == CopyMode::Copy
        && options.update == super::UpdateMode::All
        && options.overwrite == OverwriteMode::Clobber(ClobberMode::Standard)
        && options.backup == BackupMode::None
        && options.attributes == super::Attributes::NONE
        && options.reflink_mode == ReflinkMode::Never
        && options.sparse_mode != SparseMode::Always
        && options.dereference(true)
        && !options.attributes_only
        && !options.parents
        && !options.recursive
        && !options.no_target_dir
        && !options.one_file_system
        && !options.copy_contents
        && !options.strip_trailing_slashes
        && !options.verbose
        && !options.debug
        && !options.progress_bar
        && !options.set_selinux_context
        && options.context.is_none()
}

fn stream_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn stream_entries(
    sources: &[PathBuf],
    target: &Path,
    options: &super::Options,
) -> Option<Vec<(PathBuf, std::fs::Permissions)>> {
    // All operands must be independent regular files and fresh destinations.
    // Anything requiring prompts, backups, symlink policy or metadata
    // preservation stays on cp's established admission and finalization path.
    if !target.is_dir() {
        return None;
    }
    let mut names = HashSet::new();
    let parent = stream_parent(sources.first()?);
    sources
        .iter()
        .map(|source| {
            let metadata = source.symlink_metadata().ok()?;
            if stream_parent(source) != parent
                || !eligible_file(&metadata, options.sparse_mode)
                || metadata.len() <= 64 * 1024 * 1024
            {
                return None;
            }
            let name = source.file_name()?;
            if !names.insert(name.to_owned())
                || source.as_os_str().as_encoded_bytes().ends_with(b"/")
            {
                return None;
            }
            let dest = target.join(name);
            if dest.symlink_metadata().err()?.kind() != io::ErrorKind::NotFound {
                return None;
            }
            let permissions =
                super::calculate_dest_permissions(None, &dest, &metadata, options, "");
            Some((dest, permissions))
        })
        .collect()
}

fn copy_stream_entries(
    backend: &Backend,
    paths: &[PathBuf],
    entries: &[(PathBuf, std::fs::Permissions)],
    budget: std::num::NonZeroUsize,
) -> Option<io::Result<()>> {
    // Bound open destinations as well as read storage.
    let capacity = (budget.get() / (1024 * 1024)).clamp(1, 32);
    let options = vnfs::helpers::WriterCopyOptions::new().with_round_robin(budget);
    let mut attempted = false;
    let result = (|| {
        for (paths, entries) in paths.chunks(capacity).zip(entries.chunks(capacity)) {
            let mut outputs: Vec<_> = entries
                .iter()
                .map(|(dest, _)| DeferredOutput::new(dest, true))
                .collect();
            let copied = backend.copy_to_writers(paths, &mut outputs, options);
            attempted |= outputs.iter().any(|output| output.attempted);
            copied?;
            for (output, (_, permissions)) in outputs.iter_mut().zip(entries) {
                // A successful empty source still needs a destination. Use the
                // owned descriptor for cp's best-effort chmod.
                attempted = true;
                output.open()?.set_permissions(permissions.clone()).ok();
            }
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!(
                    "[profile] cp_vfsi_method=round_robin cp_stream_files={} cp_stream_bytes={budget}",
                    paths.len()
                );
            }
        }
        Ok(())
    })();
    // A source failure before any destination open can use the ordinary
    // operand loop, including processing earlier readable and later operands.
    (attempted || result.is_ok()).then_some(result)
}

#[cfg(test)]
mod tests {

    #[test]
    fn command_scope_cleans_unclaimed_staging() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("staged");
        std::fs::write(&temp, b"staged contents").unwrap();
        {
            let _batch = BatchGuard;
            CTX.with(|slot| {
                *slot.borrow_mut() = Some(Context {
                    backend: Backend::Dummy {
                        fs: Posix::new(root.path()).unwrap(),
                        paths: PathMapper::new(root.path()).unwrap(),
                    },
                    prefetched: HashMap::from([(PathBuf::from("source"), b"contents".to_vec())]),
                    prepared_copies: HashMap::from([(
                        PathBuf::from("source"),
                        PreparedCopy {
                            dest: root.path().join("destination"),
                            temp: temp.clone(),
                        },
                    )]),
                });
            });
            assert!(temp.exists());
        }
        assert!(!temp.exists());
        assert!(!root.path().join("destination").exists());
        CTX.with(|slot| {
            let slot = slot.borrow();
            let context = slot.as_ref().unwrap();
            assert!(context.prefetched.is_empty());
            assert!(context.prepared_copies.is_empty());
        });
    }

    #[test]
    fn malformed_server_copy_reply_cleans_staging_without_publishing_or_fallback() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("staging");
        let dest = root.path().join("destination");
        std::fs::write(&temp, b"copied once").unwrap();
        std::fs::write(&dest, b"racing destination").unwrap();
        let mut prepared = HashMap::new();
        assert!(
            retain_server_copies(
                &mut prepared,
                vec![(
                    PathBuf::from("source"),
                    PreparedCopy {
                        dest: dest.clone(),
                        temp: temp.clone()
                    }
                )],
                Ok(vec![])
            )
            .is_err()
        );
        assert!(!temp.exists());
        assert_eq!(std::fs::read(dest).unwrap(), b"racing destination");
        assert!(prepared.is_empty());
    }

    #[test]
    fn server_copy_only_unsubmitted_errors_allow_fallback() {
        use vnfs::error::OpError;
        assert!(server_copy_completed(Ok(())).unwrap());
        assert!(
            !server_copy_completed(Err(OpError::not_attempted(io::Error::from(
                io::ErrorKind::Unsupported
            ))))
            .unwrap()
        );
        for unknown in [false, true] {
            let cause = io::Error::from(io::ErrorKind::Unsupported);
            let error = if unknown {
                OpError::unknown(cause, None)
            } else {
                OpError::failed(cause, None)
            };
            assert!(server_copy_completed(Err(error)).is_err());
        }
    }

    #[test]
    fn server_copy_failure_cleans_nonprefix_successes_without_fallback() {
        use vnfs::error::OpError;
        for unknown in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let plans: Vec<_> = (0..3)
                .map(|index| {
                    let source = PathBuf::from(format!("source-{index}"));
                    let temp = root.path().join(format!("staged-{index}"));
                    std::fs::write(&temp, b"completed once").unwrap();
                    (
                        source,
                        PreparedCopy {
                            dest: root.path().join(format!("target-{index}")),
                            temp,
                        },
                    )
                })
                .collect();
            let mut prepared = HashMap::new();
            let cause = io::Error::from(io::ErrorKind::Unsupported);
            let error = if unknown {
                OpError::unknown(cause, None)
            } else {
                OpError::failed(cause, None)
            };
            assert!(
                retain_server_copies(&mut prepared, plans, Ok(vec![Ok(()), Err(error), Ok(())]))
                    .is_err()
            );
            assert!(prepared.is_empty());
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn server_copy_rejection_and_unsubmitted_siblings_keep_safe_fallback() {
        use vnfs::error::OpError;
        let root = tempfile::tempdir().unwrap();
        let plans = || {
            (0..2)
                .map(|index| {
                    (
                        PathBuf::from(format!("source-{index}")),
                        PreparedCopy {
                            dest: root.path().join(format!("target-{index}")),
                            temp: root.path().join(format!("staged-{index}")),
                        },
                    )
                })
                .collect()
        };
        let mut prepared = HashMap::new();
        assert!(
            !retain_server_copies(
                &mut prepared,
                plans(),
                Err(io::Error::other("rejected before dispatch"))
            )
            .unwrap()
        );
        assert!(prepared.is_empty());
        assert!(
            retain_server_copies(
                &mut prepared,
                plans(),
                Ok(vec![
                    Err(OpError::not_attempted(io::Error::from(
                        io::ErrorKind::Unsupported
                    ))),
                    Ok(())
                ])
            )
            .unwrap()
        );
        assert_eq!(
            prepared.keys().collect::<Vec<_>>(),
            [&PathBuf::from("source-1")]
        );
        assert!(retain_server_copies(&mut prepared, plans(), Ok(vec![Ok(())])).is_err());
    }

    use super::*;

    fn stream_options() -> super::super::Options {
        super::super::Options {
            reflink_mode: super::super::ReflinkMode::Never,
            sparse_mode: super::super::SparseMode::Never,
            cli_dereference: true,
            ..Default::default()
        }
    }

    #[test]
    fn streaming_resolves_bare_relative_operands_against_the_current_directory() {
        assert_eq!(stream_parent(Path::new("one")), Path::new("."));
        assert_eq!(stream_parent(Path::new("./two")), Path::new("."));
        assert_eq!(stream_parent(Path::new("/dir/one")), Path::new("/dir"));
    }

    #[test]
    fn streaming_keeps_complex_copy_policies_on_the_existing_path() {
        let base = stream_options();
        assert!(plain_stream_options(&base));
        let mut options = stream_options();
        options.verbose = true;
        assert!(!plain_stream_options(&options));
        options = stream_options();
        options.parents = true;
        assert!(!plain_stream_options(&options));
        options = stream_options();
        options.overwrite = super::super::OverwriteMode::NoClobber;
        assert!(!plain_stream_options(&options));
        options = stream_options();
        options.attributes_only = true;
        assert!(!plain_stream_options(&options));
        options = stream_options();
        options.recursive = true;
        assert!(!plain_stream_options(&options));
        assert!(!plain_stream_options(&super::super::Options::default()));
    }

    #[test]
    fn streaming_admission_rejects_small_duplicate_existing_and_symlink_operands() {
        let root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let one = root.path().join("one");
        let two = root.path().join("two");
        File::create(&one)
            .unwrap()
            .set_len(65 * 1024 * 1024)
            .unwrap();
        File::create(&two)
            .unwrap()
            .set_len(65 * 1024 * 1024)
            .unwrap();
        let sources = [one.clone(), two.clone()];
        assert!(stream_entries(&sources, dest.path(), &stream_options()).is_some());
        let mut auto_sparse = stream_options();
        auto_sparse.sparse_mode = super::super::SparseMode::Auto;
        assert!(stream_entries(&sources, dest.path(), &auto_sparse).is_none());
        assert!(
            stream_entries(&[one.clone(), one.clone()], dest.path(), &stream_options()).is_none()
        );
        std::os::unix::fs::symlink("missing", dest.path().join("two")).unwrap();
        assert!(stream_entries(&sources, dest.path(), &stream_options()).is_none());
        std::fs::remove_file(dest.path().join("two")).unwrap();
        std::fs::write(dest.path().join("two"), b"existing").unwrap();
        assert!(stream_entries(&sources, dest.path(), &stream_options()).is_none());
        std::fs::remove_file(dest.path().join("two")).unwrap();
        std::fs::remove_file(&two).unwrap();
        std::os::unix::fs::symlink(&one, &two).unwrap();
        assert!(stream_entries(&sources, dest.path(), &stream_options()).is_none());
        std::fs::remove_file(&two).unwrap();
        std::fs::write(&two, b"small").unwrap();
        assert!(stream_entries(&sources, dest.path(), &stream_options()).is_none());
        let other = dest.path().join("another");
        File::create(&other)
            .unwrap()
            .set_len(65 * 1024 * 1024)
            .unwrap();
        assert!(stream_entries(&[one, other], dest.path(), &stream_options()).is_none());
    }

    #[test]
    fn streaming_writes_owned_destinations_and_applies_modes_without_staging() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("one"), b"first").unwrap();
        std::fs::write(root.path().join("two"), b"second").unwrap();
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        let paths = vec![PathBuf::from("/one"), PathBuf::from("/two")];
        let entries = vec![
            (
                dest.path().join("one"),
                std::fs::Permissions::from_mode(0o640),
            ),
            (
                dest.path().join("two"),
                std::fs::Permissions::from_mode(0o400),
            ),
        ];
        copy_stream_entries(
            &backend,
            &paths,
            &entries,
            std::num::NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&entries[0].0).unwrap(), b"first");
        assert_eq!(std::fs::read(&entries[1].0).unwrap(), b"second");
        for (path, mode) in &entries {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode.mode()
            );
        }
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 2);
        // A racing destination must never be truncated, including symlinks.
        assert!(
            copy_stream_entries(
                &backend,
                &paths,
                &entries,
                std::num::NonZeroUsize::new(2 * 1024 * 1024).unwrap()
            )
            .unwrap()
            .is_err()
        );
        assert_eq!(std::fs::read(&entries[0].0).unwrap(), b"first");
        std::fs::remove_file(&entries[0].0).unwrap();
        std::os::unix::fs::symlink(&entries[1].0, &entries[0].0).unwrap();
        assert!(
            copy_stream_entries(
                &backend,
                &paths,
                &entries,
                std::num::NonZeroUsize::new(2 * 1024 * 1024).unwrap()
            )
            .unwrap()
            .is_err()
        );
        assert_eq!(std::fs::read(&entries[1].0).unwrap(), b"second");
    }

    #[test]
    fn unreadable_source_preserves_existing_destination() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let dest = root.path().join("dest");
        std::fs::write(&source, b"source data").unwrap();
        std::fs::write(&dest, b"keep destination").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0)).unwrap();
        if File::open(&source).is_ok() {
            return; // A privileged test runner can read mode-000 files.
        }
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        assert!(!copy_to_destination(&backend, Path::new("/source"), &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"keep destination");

        let fresh = root.path().join("new");
        let entries = [(fresh.clone(), std::fs::Permissions::from_mode(0o644))];
        assert!(
            copy_stream_entries(
                &backend,
                &[PathBuf::from("/source")],
                &entries,
                std::num::NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            )
            .is_none()
        );
        assert!(!fresh.exists());
    }

    #[test]
    fn round_robin_preflight_declines_late_unreadable_input() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let sources = ["first", "unreadable", "last"].map(|name| root.path().join(name));
        for source in &sources {
            std::fs::write(source, b"source").unwrap();
        }
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        assert!(stream_source_paths(&backend, &sources).is_some());
        std::fs::set_permissions(&sources[1], std::fs::Permissions::from_mode(0)).unwrap();
        if File::open(&sources[1]).is_err() {
            assert!(stream_source_paths(&backend, &sources).is_none());
        }
        std::fs::remove_file(&sources[1]).unwrap();
        nix::unistd::mkfifo(&sources[1], nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(stream_source_paths(&backend, &sources).is_none());
    }

    #[test]
    fn readable_empty_source_truncates_destination() {
        let root = tempfile::tempdir().unwrap();
        File::create(root.path().join("source")).unwrap();
        let dest = root.path().join("dest");
        std::fs::write(&dest, b"old destination").unwrap();
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        assert!(copy_to_destination(&backend, Path::new("/source"), &dest).unwrap());
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 0);
    }

    #[test]
    fn streaming_source_failure_before_output_allows_native_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        let entries = vec![(
            dest.path().join("missing"),
            std::fs::Permissions::from_mode(0o644),
        )];
        assert!(
            copy_stream_entries(
                &backend,
                &[PathBuf::from("/missing")],
                &entries,
                std::num::NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            )
            .is_none()
        );
        assert!(!entries[0].0.exists());
    }

    #[test]
    fn prefetch_batches_regular_files_with_one_budget_and_leaves_symlinks_to_cp() {
        let root = tempfile::tempdir().unwrap();
        for (name, data) in [
            ("one", b"abc".as_slice()),
            ("two", b"defg"),
            ("large", b"0123456789"),
        ] {
            std::fs::write(root.path().join(name), data).unwrap();
        }
        OpenOptions::new()
            .write(true)
            .open(root.path().join("large"))
            .unwrap()
            .set_len((MAX_PREFETCH_FILE_BYTES + 1) as u64)
            .unwrap();
        std::os::unix::fs::symlink("one", root.path().join("link")).unwrap();
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        let sources: Vec<_> = ["one", "link", "two", "large"]
            .map(|name| root.path().join(name))
            .into();
        let cached =
            prefetch_regular_files(&backend, &sources, 7, MAX_PREFETCH_FILE_BYTES).unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[&sources[0]], b"abc");
        assert_eq!(cached[&sources[2]], b"defg");
        assert!(!cached.contains_key(&sources[1]));
        // An oversized input leaves budget available for its small siblings.
        let mixed = [&sources[3], &sources[0], &sources[2]].map(PathBuf::clone);
        assert_eq!(
            prefetch_regular_files(
                &backend,
                &mixed,
                MAX_PREFETCH_FILE_BYTES * 2,
                MAX_PREFETCH_FILE_BYTES,
            )
            .unwrap(),
            cached
        );
        // A configured aggregate-only budget retains the old selection policy.
        let configured =
            prefetch_regular_files(&backend, &mixed, MAX_PREFETCH_FILE_BYTES * 2, usize::MAX)
                .unwrap();
        assert_eq!(configured[&sources[3]].len(), MAX_PREFETCH_FILE_BYTES + 1);
        assert!(
            prefetch_regular_files(&backend, &sources, 3, MAX_PREFETCH_FILE_BYTES)
                .unwrap()
                .is_empty()
        );
        assert!(
            prefetch_regular_files(&backend, &sources, 0, MAX_PREFETCH_FILE_BYTES)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mounted_backend_maps_paths_and_streams_without_raw_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"hello world").unwrap();
        let backend = Backend::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        let mapped = PathMapper::new(root.path())
            .unwrap()
            .map(&file, ResolvePath::Follow)
            .unwrap();
        assert_eq!(mapped, Path::new("/file"));
        assert_eq!(
            backend
                .read_files(std::slice::from_ref(&mapped), 32)
                .unwrap(),
            vec![b"hello world".to_vec()]
        );
        // A disabled/empty budget must not become an inherited allocation limit.
        let error = backend
            .read_files(std::slice::from_ref(&mapped), 0)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            backend
                .read_files(std::slice::from_ref(&mapped), 4)
                .is_err()
        );

        let mut bytes = Vec::new();
        backend.copy_to_writer(&mapped, &mut bytes).unwrap();
        assert_eq!(bytes, b"hello world");
    }
}
