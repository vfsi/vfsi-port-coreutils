// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VFSI-backed traversal for `du` on Linux NFS mounts.
//!
//! When `VNFS_IMPL=dummy|nfs` and the operand lies on an NFS mount, traversal
//! visits one bounded directory at a time through the high-level `vnfs` API.
//! It asks the server for file attributes in the `READDIR` reply instead of issuing one kernel
//! `lstat` per entry. Enabling it does not change the output: this module
//! reproduces the parent module's post-order walk, hard-link deduplication,
//! `--exclude`, `-S`, `-a`, `--time`, `--inodes`, and `--apparent-size`
//! handling. Unsupported modes and connection setup failures use the standard
//! traversal. Errors after traversal starts are reported without replaying
//! already emitted output. Excluded directories are pruned before listing.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use rustc_hash::FxHashSet as HashSet;
use uucore::display::Quotable;
use uucore::error::UResult;
use uucore::fsext::MetadataTimeField;
use uucore::libc;
use uucore::translate;

use vnfs::{
    Attributes, Attrs as Metadata, AttrsOptions, FileType as VfType, Mounted, Nfs, Vfsi, VfsiExt,
    WalkControl, WalkEventKind,
};

use crate::{Deref, FileInfo, StatPrintInfo, TraversalOptions, Usage};

/// Whether `VNFS_IMPL` selects a vectorized backend.
pub fn is_enabled() -> bool {
    matches!(std::env::var("VNFS_IMPL").as_deref(), Ok("dummy" | "nfs"))
}

/// Whether this traversal can reproduce `du` semantics for these options.
///
/// `-L`/`--dereference-args` follow symbolic links and need the cycle-safe
/// logic in the standard traversal, so they are left to `std::fs`.
pub fn supports(options: &TraversalOptions) -> bool {
    matches!(options.dereference, Deref::None) && !options.one_file_system
}

fn attr_mask(options: &TraversalOptions) -> Attributes {
    let mut masks = Attributes::MODE
        | Attributes::SIZE
        | Attributes::NLINK
        | Attributes::FILEID
        | Attributes::BLOCKS;
    if options.time.is_some() {
        masks |= Attributes::MTIME | Attributes::ATIME | Attributes::CTIME;
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

/// Traverse `path` through `backend` and emit each entry, returning the root's
/// accumulated usage. `vroot` is the operand's path inside the backend
/// namespace; emitted paths are mapped back onto `path` as the user typed it.
fn traverse<F: Vfsi>(
    backend: &F,
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
    let root_attrs = backend
        .attrs_with_options(
            vroot,
            AttrsOptions::new().fields(masks).follow_symlinks(false),
        )
        .map_err(io::Error::other)?;

    if root_attrs.file_type() != VfType::Directory {
        return Ok(usage_from_attrs(path, &root_attrs, options));
    }

    let mut seen: HashSet<FileInfo> = HashSet::default();
    let mut stack: Vec<Option<Usage>> = Vec::new();
    let mut total = None;
    backend
        .walk_events_with_options(
            vroot,
            masks,
            backend.limits().walk_options(),
            false,
            |event| {
                let rendered = print_path(event.entry.path());
                if event.kind == WalkEventKind::Leave {
                    if let Some(Some(child)) = stack.pop() {
                        if let Some(Some(parent)) = stack.last_mut() {
                            if !options.separate_dirs {
                                parent.size += child.size;
                                parent.blocks += child.blocks;
                                parent.inodes += child.inodes;
                                parent.latest_time =
                                    max_time(parent.latest_time, child.latest_time);
                            }
                            send(print_tx, child, event.depth)
                                .map_err(|_| vnfs::Error::client(0, libc::EPIPE as u32))?;
                        } else if event.depth == 0 {
                            total = Some(child);
                        }
                    }
                    return Ok(WalkControl::Continue);
                }
                let name = rendered.file_name().map(|n| n.to_string_lossy());
                let excluded = options.excludes.iter().any(|pattern| {
                    pattern.matches(&rendered.to_string_lossy())
                        || name.as_deref().is_some_and(|n| pattern.matches(n))
                });
                if event.depth > 0 && excluded {
                    if options.verbose {
                        println!(
                            "{}",
                            translate!("du-verbose-ignored", "path" => rendered.quote())
                        );
                    }
                    if event.kind == WalkEventKind::Enter {
                        stack.push(None);
                    }
                    return Ok(WalkControl::SkipSubtree);
                }
                let metadata = event.entry.attrs();
                let usage = usage_from_attrs(&rendered, metadata, options);
                if event.kind == WalkEventKind::Enter {
                    stack.push(Some(usage));
                } else {
                    if let Some(info) = file_info(metadata)
                        && !options.count_links
                        && !seen.insert(info)
                    {
                        return Ok(WalkControl::Continue);
                    }
                    if let Some(Some(parent)) = stack.last_mut() {
                        parent.size += usage.size;
                        parent.blocks += usage.blocks;
                        parent.inodes += 1;
                        parent.latest_time = max_time(parent.latest_time, usage.latest_time);
                    }
                    if options.all {
                        send(print_tx, usage, event.depth)
                            .map_err(|_| vnfs::Error::client(0, libc::EPIPE as u32))?;
                    }
                }
                Ok(WalkControl::Continue)
            },
        )
        .map_err(io::Error::other)?;
    total.ok_or_else(|| io::Error::other("vnfs walk did not finish its root"))
}

/// Compute `du` for `path` through VFSI.
///
/// Returns `Ok(None)` when the path is not on an NFS mount or the backend is
/// disabled. Traversal errors are returned without restarting the walk.
pub fn try_du(
    path: &Path,
    options: &TraversalOptions,
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
) -> io::Result<Option<Usage>> {
    // Canonicalizing a no-follow symlink operand would walk its target.
    // Keep that uncommon case on the existing kernel implementation.
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Ok(None);
    }
    let Some(mount) = uucore::vnfs::nfs_mount(path) else {
        return Ok(None);
    };
    let resolved = path.canonicalize().map_err(io::Error::other)?;
    let Ok(relative) = resolved.strip_prefix(&mount.point) else {
        return Ok(None);
    };
    // Path inside the export namespace the backend resolves against.
    let vroot = Path::new("/").join(relative);

    let result = match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => traverse(
            &Mounted::new(&mount.point).map_err(io::Error::other)?,
            path,
            &vroot,
            options,
            print_tx,
        ),
        Ok("nfs") => match Nfs::from_mount(&mount.point) {
            Ok(fs) => traverse(&fs, path, &vroot, options, print_tx),
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };

    print_stats();
    if std::env::var("VNFS_PROFILE").as_deref() == Ok("1")
        && let Err(error) = &result
    {
        eprintln!("[vnfs] traversal failed: {error}");
    }
    result.map(Some)
}

/// Emit process-wide compound/RPC counts when `VNFS_STATS=1`.
fn print_stats() {
    if std::env::var("VNFS_STATS").as_deref() != Ok("1") {
        return;
    }
    let stats = vnfs::diagnostics::take_and_reset();
    let (compounds, ops, bytes, max_ops) = (
        stats.compounds,
        stats.operations,
        stats.compound_bytes,
        stats.max_operations,
    );
    if compounds > 0 {
        let avg_bytes = bytes.map_or_else(
            || "unknown".into(),
            |b| format!("{:.0}", b as f64 / compounds as f64),
        );
        let bytes = bytes.map_or_else(|| "unknown".into(), |b| b.to_string());
        eprintln!(
            "[vnfs] compounds={compounds} avg_ops={:.2} max_ops={max_ops} avg_bytes={avg_bytes} total_bytes={bytes}",
            ops as f64 / compounds as f64
        );
    }
    let (calls, micros) = (stats.rpc_calls, stats.rpc_micros);
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

    #[test]
    fn device_boundary_and_follow_modes_keep_kernel_semantics() {
        let mut options = options();
        assert!(supports(&options));
        options.one_file_system = true;
        assert!(!supports(&options), "filesystem identity is not available");
    }

    /// Run the VFSI traversal over `root` with given options and return
    /// `(path, size, inodes, depth)` for every emitted entry plus the root
    /// total.
    fn run(root: &Path, options: &TraversalOptions) -> (Vec<(PathBuf, u64, u64, usize)>, Usage) {
        let backend = Mounted::new(root).expect("dummy root");
        let (tx, rx) = mpsc::channel();
        let total = traverse(&backend, root, Path::new("/"), options, &tx).expect("traverse");
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
