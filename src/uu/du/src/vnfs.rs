// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VFSI-backed traversal for `du` on Linux NFS mounts.
//!
//! When `VNFS_IMPL=dummy|nfs` and the operand lies on an NFS mount, the whole
//! subtree is enumerated and stat'd through the high-level `vnfs` API. It
//! asks the server for file attributes in the `READDIR` reply and batches
//! `READDIR`/`GETATTR` into NFS compounds, instead of issuing one kernel
//! `lstat` per entry. Enabling it does not change the output: this module
//! reproduces the parent module's post-order walk, hard-link deduplication,
//! `--exclude`, `-S`, `-a`, `--time`, `--inodes`, and `--apparent-size`
//! handling. Every unsupported mode or backend failure falls back to the
//! standard `std::fs` traversal in the parent module.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use rustc_hash::FxHashSet as HashSet;
use uucore::display::Quotable;
use uucore::error::UResult;
use uucore::fsext::MetadataTimeField;
use uucore::translate;

use vnfs::{
    DirEntry, DirectoryListing, Metadata, MetadataFields, Mounted, Nfs, NfsClient, VfType,
    WalkOptions,
};

use crate::{Deref, FileInfo, StatPrintInfo, TraversalOptions, Usage};

enum Backend {
    Dummy(Mounted),
    Nfs(NfsClient),
}

impl Backend {
    /// No-follow attributes for one path (the operand or a single file).
    fn lstat(&self, path: &Path, fields: MetadataFields) -> vnfs::VfResult<Metadata> {
        match self {
            Self::Dummy(fs) => fs.symlink_metadata_with_fields(path, fields),
            Self::Nfs(fs) => fs.symlink_metadata_with_fields(path, fields),
        }
    }

    /// Enumerate the subtree with attributes delivered in READDIR replies.
    /// A tree beyond the default resource limits falls back to the kernel
    /// walker, which can process arbitrarily large trees without materializing
    /// every entry here.
    fn walk(&self, dir: &Path, fields: MetadataFields) -> vnfs::VfResult<Vec<DirectoryListing>> {
        match self {
            Self::Dummy(fs) => fs.walk_with_options(dir, fields, WalkOptions::default()),
            Self::Nfs(fs) => fs.walk_with_options(dir, fields, WalkOptions::default()),
        }
    }
}

/// Whether `VNFS_IMPL` selects a vectorized backend.
pub fn is_enabled() -> bool {
    matches!(std::env::var("VNFS_IMPL").as_deref(), Ok("dummy" | "nfs"))
}

/// Whether this traversal can reproduce `du` semantics for these options.
///
/// `-L`/`--dereference-args` follow symbolic links and need the cycle-safe
/// logic in the standard traversal, so they are left to `std::fs`.
pub fn supports(options: &TraversalOptions) -> bool {
    matches!(options.dereference, Deref::None)
}

fn attr_mask(options: &TraversalOptions) -> MetadataFields {
    let mut masks = MetadataFields::MODE
        | MetadataFields::SIZE
        | MetadataFields::NLINK
        | MetadataFields::FILEID
        | MetadataFields::BLOCKS;
    if options.time.is_some() {
        masks |= MetadataFields::MTIME | MetadataFields::ATIME | MetadataFields::CTIME;
    }
    masks
}

fn entry_time(metadata: &Metadata, field: MetadataTimeField) -> Option<SystemTime> {
    match field {
        MetadataTimeField::Modification => metadata.modified(),
        MetadataTimeField::Access => metadata.accessed(),
        MetadataTimeField::Change => metadata.changed(),
        // NFS has no creation timestamp.
        MetadataTimeField::Birth => None,
    }
}

fn usage_from_attrs(path: &Path, attrs: &Metadata, options: &TraversalOptions) -> Usage {
    Usage {
        path: path.to_path_buf(),
        // Directories report zero apparent size, like `Stat::new`.
        size: if attrs.file_type() == VfType::Directory {
            0
        } else {
            attrs.len()
        },
        blocks: attrs.blocks().unwrap_or_default(),
        inodes: 1,
        latest_time: options.time.and_then(|field| entry_time(attrs, field)),
    }
}

fn file_info(attrs: &Metadata) -> Option<FileInfo> {
    attrs.file_id().filter(|id| *id != 0).map(|id| FileInfo {
        file_id: u128::from(id),
        // A single NFS export is one device; `-x` cannot cross it here.
        dev_id: 0,
    })
}

fn max_time(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn send(
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
    usage: Usage,
    depth: usize,
) -> io::Result<()> {
    print_tx
        .send(Ok(StatPrintInfo { usage, depth }))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "du printer stopped"))
}

/// Recursively total `dir`'s subtree, emitting children in the same post-order
/// the standard walk uses, and return the directory's own accumulated usage.
#[allow(clippy::too_many_arguments)]
fn compute_dir(
    dir: &Path,
    depth: usize,
    dir_attrs: &Metadata,
    children: &HashMap<PathBuf, Vec<DirEntry>>,
    options: &TraversalOptions,
    print_path: &dyn Fn(&Path) -> PathBuf,
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
    seen: &mut HashSet<FileInfo>,
) -> io::Result<Usage> {
    let mut total = usage_from_attrs(&print_path(dir), dir_attrs, options);
    total.inodes = 1;

    for entry in children.get(dir).map_or(&[][..], Vec::as_slice) {
        let entry_path = entry.path();
        let metadata = entry.metadata();
        let rendered = print_path(entry_path);

        let name = rendered.file_name().map(|n| n.to_string_lossy());
        let excluded = options.excludes.iter().any(|pattern| {
            pattern.matches(&rendered.to_string_lossy())
                || name.as_deref().is_some_and(|name| pattern.matches(name))
        });
        if excluded {
            if options.verbose {
                println!(
                    "{}",
                    translate!("du-verbose-ignored", "path" => rendered.quote())
                );
            }
            continue;
        }

        if metadata.file_type() == VfType::Directory {
            let child = compute_dir(
                entry_path,
                depth + 1,
                metadata,
                children,
                options,
                print_path,
                print_tx,
                seen,
            )?;
            if !options.separate_dirs {
                total.size += child.size;
                total.blocks += child.blocks;
                total.inodes += child.inodes;
                total.latest_time = max_time(total.latest_time, child.latest_time);
            }
            send(print_tx, child, depth + 1)?;
        } else {
            if let Some(info) = file_info(metadata) {
                if seen.contains(&info) && !options.count_links {
                    continue;
                }
                seen.insert(info);
            }
            let usage = usage_from_attrs(&rendered, metadata, options);
            total.size += usage.size;
            total.blocks += usage.blocks;
            total.inodes += 1;
            total.latest_time = max_time(total.latest_time, usage.latest_time);
            if options.all {
                send(print_tx, usage, depth + 1)?;
            }
        }
    }

    Ok(total)
}

/// Traverse `path` through `backend` and emit each entry, returning the root's
/// accumulated usage. `vroot` is the operand's path inside the backend
/// namespace; emitted paths are mapped back onto `path` as the user typed it.
fn traverse(
    backend: &Backend,
    path: &Path,
    vroot: &Path,
    options: &TraversalOptions,
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
) -> io::Result<Usage> {
    let print_path = |backend_path: &Path| -> PathBuf {
        match backend_path.strip_prefix(vroot) {
            Ok(rest) if rest.as_os_str().is_empty() => path.to_path_buf(),
            Ok(rest) => path.join(rest),
            Err(_) => path.to_path_buf(),
        }
    };

    let masks = attr_mask(options);
    let root_attrs = backend.lstat(vroot, masks).map_err(io::Error::other)?;

    if root_attrs.file_type() != VfType::Directory {
        return Ok(usage_from_attrs(path, &root_attrs, options));
    }

    let directories = backend.walk(vroot, masks).map_err(io::Error::other)?;

    // Group entries under their parent directory, preserving each directory's
    // enumeration order. The root itself is not in `entries`.
    let mut children: HashMap<PathBuf, Vec<DirEntry>> = HashMap::new();
    for directory in directories {
        children.insert(directory.path, directory.entries);
    }

    let mut seen: HashSet<FileInfo> = HashSet::default();
    compute_dir(
        vroot,
        0,
        &root_attrs,
        &children,
        options,
        &print_path,
        print_tx,
        &mut seen,
    )
}

/// Compute `du` for `path` through VFSI.
///
/// Returns `Ok(None)` when the path is not on an NFS mount or the backend is
/// disabled. Any backend error is returned so the caller can fall back to
/// `std::fs`, which also produces the canonical error message.
pub fn try_du(
    path: &Path,
    options: &TraversalOptions,
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
) -> io::Result<Option<Usage>> {
    let Some(mount) = uucore::vnfs::nfs_mount(path) else {
        return Ok(None);
    };
    let resolved = path.canonicalize().map_err(io::Error::other)?;
    let Ok(relative) = resolved.strip_prefix(&mount.point) else {
        return Ok(None);
    };
    // Path inside the export namespace the backend resolves against.
    let vroot = Path::new("/").join(relative);

    let mut backend = match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Backend::Dummy(Mounted::new(&mount.point).map_err(io::Error::other)?),
        Ok("nfs") => Backend::Nfs(Nfs::connect(&mount.server).map_err(io::Error::other)?),
        _ => return Ok(None),
    };

    let result = traverse(&mut backend, path, &vroot, options, print_tx);
    print_stats();
    if std::env::var("VNFS_PROFILE").as_deref() == Ok("1")
        && let Err(error) = &result
    {
        eprintln!("[vnfs] traversal failed, falling back to std::fs: {error}");
    }
    result.map(Some)
}

/// Emit per-connection compound/RPC counts when `VNFS_STATS=1`.
fn print_stats() {
    if std::env::var("VNFS_STATS").as_deref() != Ok("1") {
        return;
    }
    let (compounds, ops, bytes, max_ops) = vnfs::backend::compound::compound_stats();
    if compounds > 0 {
        eprintln!(
            "[vnfs] compounds={compounds} avg_ops={:.2} max_ops={max_ops} avg_bytes={:.0} total_bytes={bytes}",
            ops as f64 / compounds as f64,
            bytes as f64 / compounds as f64
        );
    }
    let (calls, micros) = vnfs::backend::compound::rpc_stats();
    if calls > 0 {
        eprintln!(
            "[vnfs] rpc_calls={calls} avg_rpc_ms={:.2} total_rpc_ms={:.1}",
            micros as f64 / calls as f64 / 1000.0,
            micros as f64 / 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deref;

    fn options() -> TraversalOptions {
        TraversalOptions {
            all: false,
            separate_dirs: false,
            one_file_system: false,
            dereference: Deref::None,
            count_links: false,
            verbose: false,
            excludes: Vec::new(),
            time: None,
        }
    }

    /// Run the VFSI traversal over `root` with given options and return
    /// `(path, size, inodes, depth)` for every emitted entry plus the root
    /// total.
    fn run(root: &Path, options: &TraversalOptions) -> (Vec<(PathBuf, u64, u64, usize)>, Usage) {
        let mut backend = Backend::Dummy(Mounted::new(root).expect("dummy root"));
        let (tx, rx) = mpsc::channel();
        let total = traverse(&mut backend, root, Path::new("/"), options, &tx).expect("traverse");
        drop(tx);
        let mut emitted = Vec::new();
        for info in &rx {
            let info = info.expect("entry");
            emitted.push((
                info.usage.path,
                info.usage.size,
                info.usage.inodes,
                info.depth,
            ));
        }
        (emitted, total)
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn totals_apparent_sizes_like_std_du() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a"), 3);
        write(&dir.path().join("sub/b"), 5);
        write(&dir.path().join("sub/deep/c"), 7);

        let (emitted, total) = run(dir.path(), &options());
        let expected: u64 = 3 + 5 + 7;
        assert_eq!(total.size, expected);
        // The nested directory total is the sum of its own subtree.
        let sub = emitted
            .iter()
            .find(|(path, ..)| path.ends_with("sub"))
            .expect("sub emitted");
        assert_eq!(sub.1, 5 + 7);
        assert_eq!(sub.3, 1);
    }

    #[test]
    fn hardlinks_count_once_and_count_links_counts_each() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a"), 5);
        std::fs::hard_link(dir.path().join("a"), dir.path().join("a-link")).unwrap();

        let (_, total) = run(dir.path(), &options());
        assert_eq!(total.size, 5, "hard link counted once by default");

        std::fs::hard_link(dir.path().join("a"), dir.path().join("a-link2")).unwrap();
        let mut options = options();
        options.count_links = true;
        let (_, total) = run(dir.path(), &options);
        assert_eq!(total.size, 15, "--count-links counts every link");
    }

    #[test]
    fn separate_dirs_excludes_subdirectory_totals() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a"), 4);
        write(&dir.path().join("sub/b"), 6);

        let mut options = options();
        options.separate_dirs = true;
        let (_, total) = run(dir.path(), &options);
        assert_eq!(total.size, 4, "-S keeps only this directory's own files");

        let (emitted, _) = run(dir.path(), &options);
        assert!(
            emitted
                .iter()
                .any(|(path, size, ..)| path.ends_with("sub") && *size == 6)
        );
    }

    #[test]
    fn all_emits_files_and_default_does_not() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a"), 1);

        let (emitted, _) = run(dir.path(), &options());
        assert!(emitted.is_empty(), "files are not printed without -a");

        let mut options = options();
        options.all = true;
        let (emitted, _) = run(dir.path(), &options);
        assert_eq!(emitted.len(), 1);
        assert!(emitted[0].0.ends_with("a"));
    }
}
