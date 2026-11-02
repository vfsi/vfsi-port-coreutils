// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VFSI-backed traversal for `du` on Linux NFS mounts.
//!
//! When `VNFS_IMPL=dummy|nfs` and the operand lies on an NFS mount, traversal
//! batches bounded directory pages through the high-level `vnfs` API.
//! It asks the server for file attributes in the `READDIR` reply instead of issuing one kernel
//! `lstat` per entry. Enabling it does not change the output: this module
//! preserves post-order directory totals, hard-link deduplication,
//! `--exclude`, `-S`, `-a`, `--time`, `--inodes`, and `--apparent-size`
//! handling. Unsupported modes and connection setup failures use the standard
//! traversal. Errors after traversal starts are reported without replaying
//! already emitted output. Excluded directories are pruned before listing.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use uucore::display::Quotable;
use uucore::error::UResult;
use uucore::fsext::MetadataTimeField;
use uucore::libc;
use uucore::translate;

use vnfs::directory::{
    Attributes, Attrs as Metadata, AttrsOptions, DirectoryListing, FileType as VfType,
    ListDirControl, TraversalCompletion,
};
use vnfs::files::{Vfsi, VfsiExt};
use vnfs::nfs::Nfs;
use vnfs::posix::Posix;

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

fn usage_from_attrs(
    path: &Path,
    attrs: &Metadata,
    options: &TraversalOptions,
) -> io::Result<Usage> {
    Ok(Usage {
        path: path.to_path_buf(),
        // Directories report zero apparent size, like `Stat::new`.
        size: if attrs.file_type() == VfType::Directory {
            0
        } else {
            attrs.len().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "file size was not supplied")
            })?
        },
        blocks: attrs.blocks().unwrap_or_default(),
        inodes: 1,
        latest_time: options.time.and_then(|field| entry_time(attrs, field)),
    })
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

fn accumulate(total: &mut Usage, usage: &Usage) {
    total.size += usage.size;
    total.blocks += usage.blocks;
    total.inodes += usage.inodes;
    total.latest_time = max_time(total.latest_time, usage.latest_time);
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
    let masks = attr_mask(options);
    let root_attrs = backend
        .attrs_with_options(
            vroot,
            AttrsOptions::new()
                .with_fields(masks)
                .with_follow_final_symlink(false),
        )
        .map_err(io::Error::other)?;

    if root_attrs.file_type() != VfType::Directory {
        return usage_from_attrs(path, &root_attrs, options);
    }

    traverse_pages(path, vroot, &root_attrs, options, print_tx, |visitor| {
        backend.vlistdirs(
            &[vroot],
            backend
                .limits()
                .walk_options()
                .with_fields(masks)
                .with_allow_symlink_components(false),
            visitor,
        )
    })
}

/// Aggregate pages independently of transport, so identity changes between
/// observations can be tested deterministically without racing the filesystem.
fn traverse_pages(
    path: &Path,
    vroot: &Path,
    root_attrs: &Metadata,
    options: &TraversalOptions,
    print_tx: &mpsc::Sender<UResult<StatPrintInfo>>,
    visit: impl FnOnce(
        &mut dyn FnMut(usize, DirectoryListing) -> vnfs::Result<ListDirControl>,
    ) -> vnfs::Result<Vec<TraversalCompletion>>,
) -> io::Result<Usage> {
    // Admission and accounting are separate: the native recursive pager may
    // batch approved siblings, but never lists excluded child directories.
    // Retain only one accumulator per directory, bounded by the walk's shared
    // entry/path budgets; file metadata and pages are consumed immediately.
    struct Directory {
        usage: Usage,
        parent: usize,
        depth: usize,
        position: usize,
        entries: usize,
    }
    struct LinkedFile {
        order: Vec<usize>,
        usage: Usage,
        parent: usize,
        depth: usize,
    }
    let print_path = |backend_path: &Path| -> PathBuf {
        match backend_path.strip_prefix(vroot) {
            Ok(rest) if rest.as_os_str().is_empty() => path.to_path_buf(),
            Ok(rest) => path.join(rest),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut directories = vec![Directory {
        usage: usage_from_attrs(path, root_attrs, options)?,
        parent: 0,
        depth: 0,
        position: 0,
        entries: 0,
    }];
    let mut positions = HashMap::default();
    positions.insert(vroot.to_path_buf(), 0usize);
    let mut linked: HashMap<FileInfo, LinkedFile> = HashMap::default();
    let mut counted: HashSet<FileInfo> = HashSet::default();
    let completion = visit(&mut |index, listing| {
        if index != 0 {
            return Err(vnfs::Error::invalid_reply(None, "du: invalid root index"));
        }
        let parent = *positions
            .get(&listing.path)
            .ok_or_else(|| vnfs::Error::invalid_reply(None, "du: unadmitted directory page"))?;
        let depth = directories[parent].depth + 1;
        let mut pruned = Vec::new();
        for entry in listing.entries {
            let position = directories[parent].entries;
            directories[parent].entries += 1;
            let rendered = print_path(entry.path());
            let name = rendered.file_name().map(|n| n.to_string_lossy());
            let excluded = options.excludes.iter().any(|pattern| {
                pattern.matches(&rendered.to_string_lossy())
                    || name.as_deref().is_some_and(|n| pattern.matches(n))
            });
            if excluded {
                if options.verbose {
                    println!(
                        "{}",
                        translate!("du-verbose-ignored", "path" => rendered.quote())
                    );
                }
                if entry.attrs().is_dir() {
                    pruned.push(entry.path().to_path_buf());
                }
                continue;
            }
            let usage = usage_from_attrs(&rendered, entry.attrs(), options)
                .map_err(|_| vnfs::Error::errno(0, libc::ENOTSUP as u32))?;
            if entry.attrs().is_dir() {
                let child = directories.len();
                if positions
                    .insert(entry.path().to_path_buf(), child)
                    .is_some()
                {
                    return Err(vnfs::Error::invalid_reply(None, "du: duplicate directory"));
                }
                directories.push(Directory {
                    usage,
                    parent,
                    depth,
                    position,
                    entries: 0,
                });
            } else {
                if !options.count_links
                    && let Some(info) = file_info(entry.attrs())
                {
                    // A move can expose the same single-link inode in
                    // a later page. Link counts may also change between
                    // observations: immediate and deferred ownership
                    // must never count the same identity independently.
                    if counted.contains(&info) {
                        continue;
                    }
                    if entry.attrs().nlink() != Some(1) || linked.contains_key(&info) {
                        // Discovery is batched, but hard-link attribution
                        // follows the original DFS. Retain only linked files;
                        // ordinary one-link files are consumed immediately.
                        let mut order = vec![position];
                        let mut ancestor = parent;
                        while ancestor != 0 {
                            order.push(directories[ancestor].position);
                            ancestor = directories[ancestor].parent;
                        }
                        order.reverse();
                        if linked
                            .get(&info)
                            .is_none_or(|previous| order < previous.order)
                        {
                            linked.insert(
                                info,
                                LinkedFile {
                                    order,
                                    usage,
                                    parent,
                                    depth,
                                },
                            );
                        }
                        continue;
                    }
                    counted.insert(info);
                }
                accumulate(&mut directories[parent].usage, &usage);
                if options.all {
                    send(print_tx, usage, depth)
                        .map_err(|_| vnfs::Error::errno(0, libc::EPIPE as u32))?;
                }
            }
        }
        Ok(ListDirControl::Prune(pruned))
    })
    .map_err(io::Error::other)?;
    if completion.as_slice() != [TraversalCompletion::Complete] {
        return Err(io::Error::other("vnfs walk did not finish its root"));
    }
    drop(positions);
    for file in linked.into_values() {
        accumulate(&mut directories[file.parent].usage, &file.usage);
        if options.all {
            send(print_tx, file.usage, file.depth)?;
        }
    }
    // Discovery always precedes descendants. Reverse reduction therefore emits
    // every directory after its files and subdirectories, independent of page
    // boundaries and sibling prefetch order. No complete tree of file entries
    // or output lines is retained.
    while directories.len() > 1 {
        let child = directories.pop().unwrap();
        if !options.separate_dirs {
            accumulate(&mut directories[child.parent].usage, &child.usage);
        }
        send(print_tx, child.usage, child.depth)?;
    }
    Ok(directories.pop().unwrap().usage)
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
            &Posix::new(&mount.point).map_err(io::Error::other)?,
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
    fn missing_size_is_an_error_but_empty_file_is_zero() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("empty"), b"").unwrap();
        let fs = Posix::new(root.path()).unwrap();
        let partial = fs
            .attrs_with_options("/empty", AttrsOptions::new().with_fields(Attributes::MODE))
            .unwrap();
        assert_eq!(
            usage_from_attrs(Path::new("empty"), &partial, &options())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
        let full = fs
            .attrs_with_options(
                "/empty",
                AttrsOptions::new().with_fields(attr_mask(&options())),
            )
            .unwrap();
        assert_eq!(
            usage_from_attrs(Path::new("empty"), &full, &options())
                .unwrap()
                .size,
            0
        );
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
        let backend = Posix::new(root).expect("dummy root");
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
    fn cross_directory_hardlinks_keep_depth_first_attribution() {
        use vnfs::directory::{WalkControl, WalkEventKind, WalkOptions};
        let mut subtree_first = false;
        // Cover both insertion-ordered and reverse-insertion-ordered filesystems.
        for directory_first in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            if directory_first {
                write(&dir.path().join("a/first"), 5);
                std::fs::hard_link(dir.path().join("a/first"), dir.path().join("z-link")).unwrap();
            } else {
                write(&dir.path().join("z-link"), 5);
                std::fs::create_dir(dir.path().join("a")).unwrap();
                std::fs::hard_link(dir.path().join("z-link"), dir.path().join("a/first")).unwrap();
            }
            // The existing lazy event walker defines the original DFS order;
            // filesystem enumeration order itself is deliberately unspecified.
            let fs = Posix::new(dir.path()).unwrap();
            let mut first = None;
            fs.listdir(
                "/",
                WalkOptions::new(fs.limits().walk_options()).with_enter_leave(true),
                |event| {
                    if event.kind == WalkEventKind::Entry && first.is_none() {
                        first = Some(event.entry.path().to_path_buf());
                    }
                    Ok(WalkControl::Continue)
                },
            )
            .unwrap();
            let first = first.unwrap();
            let inside = first == Path::new("/a/first");
            subtree_first |= inside;
            let mut opts = options();
            opts.all = true;
            let (emitted, total) = run(dir.path(), &opts);
            assert_eq!(total.size, 5);
            assert_eq!(
                emitted.iter().find(|(p, ..)| p.ends_with("a")).unwrap().1,
                if inside { 5 } else { 0 }
            );
            let winner = dir.path().join(first.strip_prefix("/").unwrap());
            assert!(emitted.iter().any(|(p, ..)| *p == winner));
            assert_eq!(emitted.len(), 2, "one alias and its directory");
            opts.count_links = true;
            let (emitted, total) = run(dir.path(), &opts);
            assert_eq!(total.size, 10);
            assert_eq!(emitted.len(), 3);
        }
        assert!(
            subtree_first,
            "exercise a descendant winning over a root alias"
        );
    }

    #[test]
    fn changing_names_and_link_counts_count_each_file_identity_once() {
        use vnfs::directory::DirEntry;
        for (first_links, second_links) in [(1, 1), (2, 1), (1, 2)] {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join("source"), 5);
            if first_links == 2 {
                std::fs::hard_link(dir.path().join("source"), dir.path().join("extra")).unwrap();
            }
            let fs = Posix::new(dir.path()).unwrap();
            let root = fs.attrs("/").unwrap();
            let first = fs.attrs("/source").unwrap();
            if second_links > first_links {
                std::fs::hard_link(dir.path().join("source"), dir.path().join("extra")).unwrap();
            } else if second_links < first_links {
                std::fs::remove_file(dir.path().join("extra")).unwrap();
            }
            std::fs::rename(dir.path().join("source"), dir.path().join("destination")).unwrap();
            let second = fs.attrs("/destination").unwrap();
            assert_eq!(first.file_id(), second.file_id());
            assert_eq!(first.nlink(), Some(first_links));
            assert_eq!(second.nlink(), Some(second_links));
            for count_links in [false, true] {
                let mut opts = options();
                opts.all = true;
                opts.count_links = count_links;
                let (tx, rx) = mpsc::channel();
                // Supply real before/after metadata in separate virtual pages.
                // This models a move into a later cohort, without timer races.
                let pages = vec![
                    DirectoryListing {
                        path: "/".into(),
                        entries: vec![
                            DirEntry::new("/a".into(), root.clone()),
                            DirEntry::new("/b".into(), root.clone()),
                        ],
                    },
                    DirectoryListing {
                        path: "/a".into(),
                        entries: vec![DirEntry::new("/a/first".into(), first.clone())],
                    },
                    DirectoryListing {
                        path: "/b".into(),
                        entries: vec![DirEntry::new("/b/second".into(), second.clone())],
                    },
                ];
                let total = traverse_pages(
                    Path::new("out"),
                    Path::new("/"),
                    &root,
                    &opts,
                    &tx,
                    |visitor| {
                        for page in pages {
                            assert_eq!(visitor(0, page)?, ListDirControl::Prune(Vec::new()));
                        }
                        Ok(vec![TraversalCompletion::Complete])
                    },
                )
                .unwrap();
                let emitted: Vec<_> = rx.try_iter().map(|item| item.unwrap()).collect();
                assert_eq!(
                    total.size,
                    if count_links { 10 } else { 5 },
                    "nlink {first_links}->{second_links}, count_links={count_links}"
                );
                assert_eq!(total.inodes, if count_links { 5 } else { 4 });
                assert_eq!(emitted.len(), if count_links { 4 } else { 3 });
                assert!(
                    emitted
                        .iter()
                        .any(|item| item.usage.path == Path::new("out/a/first"))
                );
                assert_eq!(
                    emitted
                        .iter()
                        .any(|item| item.usage.path == Path::new("out/b/second")),
                    count_links
                );
            }
        }
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

    #[test]
    fn batched_pages_keep_post_order_totals_across_cohorts_and_large_listings() {
        let dir = tempfile::tempdir().unwrap();
        // More siblings than the 32-directory cohort; more files than a page.
        for i in 0..40 {
            write(&dir.path().join(format!("d{i:02}/nested/file")), 3);
        }
        for i in 0..260 {
            write(&dir.path().join(format!("top-{i:03}")), 2);
        }
        let mut options = options();
        options.all = true;
        let (emitted, total) = run(dir.path(), &options);
        assert_eq!((total.size, total.inodes), (640, 381));
        assert_eq!(emitted.len(), 380);
        for (i, (path, size, inodes, depth)) in emitted.iter().enumerate() {
            if path.is_dir() {
                assert!(
                    !emitted[i + 1..]
                        .iter()
                        .any(|(later, ..)| later.starts_with(path)),
                    "directory {path:?} must follow every descendant"
                );
                assert_eq!(*size, 3);
                assert_eq!(*inodes, if *depth == 1 { 3 } else { 2 });
            }
        }
    }

    #[test]
    fn excluded_directories_are_not_listed_or_charged_to_totals() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("keep/leaf"), 7);
        write(&dir.path().join("drop/hidden"), 999);
        write(&dir.path().join("omit.tmp"), 99);
        std::fs::set_permissions(
            dir.path().join("drop"),
            std::fs::Permissions::from_mode(0o0),
        )
        .unwrap();
        let mut options = options();
        options.all = true;
        options.excludes = vec![
            glob::Pattern::new("drop").unwrap(),
            glob::Pattern::new("*.tmp").unwrap(),
        ];
        let (emitted, total) = run(dir.path(), &options);
        // Restore permissions even though TempDir cleanup can usually unlink
        // the empty parent itself; the hidden child needs directory access.
        std::fs::set_permissions(
            dir.path().join("drop"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert_eq!((total.size, total.inodes), (7, 3));
        assert_eq!(emitted.len(), 2);
        assert!(emitted[0].0.ends_with("keep/leaf"));
        assert!(emitted[1].0.ends_with("keep"));
    }
}
