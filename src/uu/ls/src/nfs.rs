// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! NFS-aware entry source for `ls`: when a target directory is mounted on NFS
//! and the `VNFS_IMPL` environment variable selects a vectorized backend
//! (`dummy` or `nfs`), directory contents are enumerated through the `vnfs`
//! application API instead of `std::fs`.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use crate::meta::{LsDirEntry, LsReadDir};
use uucore::vnfs::NfsMount;
use vnfs::{
    DirEntry, DirectoryListing, MetadataFields, Mounted, Nfs, NfsClient, ReadDirOptions, VfError,
    WalkOptions,
};

pub(crate) struct WalkEntry {
    pub path: PathBuf,
    pub entries: Vec<DirEntry>,
}

/// Runtime backend selection. `VNFS_IMPL=off` (or unset) uses `std::fs`;
/// `dummy` uses the mounted kernel tree; `nfs` talks to the NFS
/// server directly with compound batching.
fn impl_choice() -> Option<&'static str> {
    match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => Some("dummy"),
        Ok("nfs") => Some("nfs"),
        _ => None,
    }
}

/// Map a vectorized error to an `io::Error`, choosing an `ErrorKind` from the
/// typed [`vnfs::VfError`] (transport vs. filesystem status) and keeping the
/// operation index in the message.
fn vf_io_error(e: VfError) -> io::Error {
    if e.is_transport() {
        io::Error::new(io::ErrorKind::ConnectionRefused, e)
    } else {
        e.into()
    }
}

/// Attributes requested for every entry (all supported fields). The
/// FATTR4_NAMED_ATTR boolean is only needed by the long format (for the `+`
/// access-indicator), and costs the server a per-entry xattr enumeration, so
/// it is only requested then.
fn full_mask(config: &crate::config::Config) -> MetadataFields {
    use vnfs::MetadataFields as AttrMask;
    let mut mask = AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::BLOCKS
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::RDEV
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME;
    if config.format == crate::display::Format::Long {
        mask |= AttrMask::NAMED_ATTR;
    }
    mask
}

/// The application-facing clients selected for this mount.
enum Backend {
    Dummy(Mounted),
    Nfs(NfsClient),
}

impl Backend {
    fn read_dirs(
        &self,
        dirs: &[&Path],
        fields: MetadataFields,
    ) -> vnfs::VfResult<Vec<DirectoryListing>> {
        match self {
            Self::Dummy(fs) => fs.read_dirs_with_options(dirs, fields, ReadDirOptions::default()),
            Self::Nfs(fs) => fs.read_dirs_with_options(dirs, fields, ReadDirOptions::default()),
        }
    }

    fn walk(
        &self,
        root: &Path,
        masks: MetadataFields,
        sort: &mut dyn FnMut(&Path, &mut Vec<DirEntry>),
    ) -> vnfs::VfResult<Vec<WalkEntry>> {
        let tree = match self {
            Self::Dummy(fs) => fs.walk_with_options(root, masks, WalkOptions::default()),
            Self::Nfs(fs) => fs.walk_with_options(root, masks, WalkOptions::default()),
        }?;
        let mut by_path = std::collections::HashMap::with_capacity(tree.len());
        for listing in tree {
            let mut entries = listing.entries;
            sort(&listing.path, &mut entries);
            by_path.insert(listing.path, entries);
        }
        let mut ordered = Vec::with_capacity(by_path.len());
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let Some(entries) = by_path.remove(&path) else {
                continue;
            };
            for child in entries.iter().rev() {
                if child.file_type() == vnfs::VfType::Directory {
                    pending.push(child.path().to_path_buf());
                }
            }
            ordered.push(WalkEntry { path, entries });
        }
        Ok(ordered)
    }
}

struct VfContext {
    backend: Backend,
    mount: NfsMount,
}

impl Drop for VfContext {
    fn drop(&mut self) {
        if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
            let stats = vnfs::diagnostics::snapshot();
            let (n, ops, bytes, max) = (
                stats.compounds,
                stats.operations,
                stats.compound_bytes,
                stats.max_operations,
            );
            if n > 0 {
                eprintln!(
                    "[vnfs] compounds={n} avg_ops={:.2} max_ops={max} avg_bytes={:.0} total_bytes={bytes}",
                    ops as f64 / n as f64,
                    bytes as f64 / n as f64
                );
            }
            let (calls, us) = (stats.rpc_calls, stats.rpc_micros);
            if calls > 0 {
                eprintln!(
                    "[vnfs] rpc_calls={calls} avg_rpc_ms={:.2} total_rpc_ms={:.1}",
                    us as f64 / calls as f64 / 1000.0,
                    us as f64 / 1000.0
                );
            }
        }
    }
}

// Lazily-created, process-wide backend context (ls is single-threaded).
thread_local! {
    static CTX: RefCell<Option<VfContext>> = const { RefCell::new(None) };
}

fn nfs_mountpoint(path: &Path) -> Option<NfsMount> {
    uucore::vnfs::nfs_mount(path)
}

fn make_ctx(mount: NfsMount) -> io::Result<VfContext> {
    let backend = match impl_choice() {
        Some("dummy") => Backend::Dummy(Mounted::new(&mount.point).map_err(vf_io_error)?),
        Some("nfs") => Backend::Nfs(
            Nfs::builder(&mount.server)
                .root(&mount.export)
                .connect()
                .map_err(vf_io_error)?,
        ),
        _ => return Err(io::Error::other("VNFS_IMPL disabled")),
    };
    Ok(VfContext { backend, mount })
}

/// Open `path` for listing through the vectorized backend when applicable.
/// Returns `Ok(None)` when the path is not NFS-mounted or the backend is off.
pub fn try_open_vf(path: &Path, config: &crate::config::Config) -> io::Result<Option<LsReadDir>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(mount) = nfs_mountpoint(path) else {
        return Ok(None);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.as_ref().is_none_or(|ctx| ctx.mount != mount) {
            let t0 = std::time::Instant::now();
            *ctx = Some(make_ctx(mount)?);
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!(
                    "[profile] connect_ms={:.1}",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        let ctx = ctx.as_mut().unwrap();

        let rel = path.strip_prefix(&ctx.mount.point).unwrap_or(path);
        let vpath = Path::new("/").join(rel);
        let listing = match ctx.backend.read_dirs(&[&vpath], full_mask(config)) {
            Ok(mut listings) => match listings.pop() {
                Some(listing) => listing,
                None => return Err(io::Error::other("vnfs returned no directory listing")),
            },
            Err(error) if error.err_no() == uucore::libc::EFBIG as u32 => return Ok(None),
            Err(error) => return Err(vf_io_error(error)),
        };

        let mut out = Vec::with_capacity(listing.entries.len());
        for entry in listing.entries {
            let name = entry
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            out.push(LsDirEntry::Vf {
                path: path.join(&name),
                name,
                entry,
            });
        }
        Ok(Some(LsReadDir::from_vf(out)))
    })
}

/// List several directories in one vectorized batch and map the entries back
/// to kernel paths. A failed vector request may have emitted only part of a
/// directory before stopping, so all operands fall back to ordinary listings.
/// No partial directory is presented to `ls` as complete.
fn ctx_open_many(
    ctx: &mut VfContext,
    kernel_dirs: &[&Path],
    masks: MetadataFields,
) -> Vec<Option<LsReadDir>> {
    let vpaths: Vec<PathBuf> = kernel_dirs
        .iter()
        .map(|d| {
            let rel = d.strip_prefix(&ctx.mount.point).unwrap_or(d);
            Path::new("/").join(rel)
        })
        .collect();
    let refs: Vec<&Path> = vpaths.iter().map(PathBuf::as_path).collect();
    let Ok(listings) = ctx.backend.read_dirs(&refs, masks) else {
        // Callback entries can interleave across directories by READDIR page.
        // An error index identifies the failed operand, not which earlier
        // callback streams are complete.
        return std::iter::repeat_with(|| None).take(refs.len()).collect();
    };
    let mut out = Vec::with_capacity(refs.len());
    for (i, listing) in listings.into_iter().enumerate() {
        let mut list = Vec::with_capacity(listing.entries.len());
        for entry in listing.entries {
            let name = entry
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            list.push(LsDirEntry::Vf {
                path: kernel_dirs[i].join(&name),
                name,
                entry,
            });
        }
        out.push(Some(LsReadDir::from_vf(list)));
    }
    out
}

/// Open several directory operands in one vectorized batch when they all live
/// on the same NFS mount (and the backend is enabled). Returns `Ok(None)`
/// when the batch path does not apply, and a per-directory result vector
/// otherwise (entries, or `None` where the caller must fall back).
pub fn try_open_many_vf(
    dirs: &[&Path],
    config: &crate::config::Config,
) -> io::Result<Option<Vec<Option<LsReadDir>>>> {
    if impl_choice().is_none() || dirs.len() < 2 {
        return Ok(None);
    }
    let Some(mount) = nfs_mountpoint(dirs[0]) else {
        return Ok(None);
    };
    if dirs[1..]
        .iter()
        .any(|d| nfs_mountpoint(d) != Some(mount.clone()))
    {
        return Ok(None);
    }
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.as_ref().is_none_or(|ctx| ctx.mount != mount) {
            *ctx = Some(make_ctx(mount)?);
        }
        let ctx = ctx.as_mut().unwrap();
        Ok(Some(ctx_open_many(ctx, dirs, full_mask(config))))
    })
}

/// Walk the whole subtree rooted at `path` through the vectorized backend,
/// returning each directory with its entries, ordered exactly as `ls` would
/// list them (via [`sort_entries`] semantics, so it is correct under any
/// locale and sort mode). Paths in the result are mapped back to kernel paths
/// (under the mountpoint).
pub fn try_walk_vf(
    path: &Path,
    config: &crate::config::Config,
) -> io::Result<Option<Vec<WalkEntry>>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(mount) = nfs_mountpoint(path) else {
        return Ok(None);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.as_ref().is_none_or(|ctx| ctx.mount != mount) {
            let t0 = std::time::Instant::now();
            *ctx = Some(make_ctx(mount)?);
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!(
                    "[profile] connect_ms={:.1}",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        let ctx = ctx.as_mut().unwrap();

        let rel = path.strip_prefix(&ctx.mount.point).unwrap_or(path);
        let vpath = Path::new("/").join(rel);
        let mut sort = |_dir: &Path, entries: &mut Vec<DirEntry>| {
            crate::sort_vf_entries(entries, config);
        };
        let t0 = std::time::Instant::now();
        let tree = match ctx.backend.walk(&vpath, full_mask(config), &mut sort) {
            Ok(tree) => tree,
            Err(error) if error.err_no() == uucore::libc::EFBIG as u32 => return Ok(None),
            Err(error) => return Err(vf_io_error(error)),
        };
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!(
                "[profile] walk_ms={:.1}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }

        let mut out = Vec::with_capacity(tree.len());
        for mut w in tree {
            // Rewrite root-relative vnfs paths back to kernel paths.
            let rel = w.path.strip_prefix("/").unwrap_or(&w.path);
            w.path = ctx.mount.point.join(rel);
            out.push(w);
        }
        Ok(Some(out))
    })
}

#[cfg(all(test, all(feature = "vnfs", target_os = "linux")))]
mod tests {
    use super::*;
    use std::path::Path;

    fn dummy_ctx(root: &Path) -> VfContext {
        VfContext {
            backend: Backend::Dummy(Mounted::new(root).unwrap()),
            mount: NfsMount {
                server: "dummy".to_string(),
                export: PathBuf::from("/"),
                point: root.to_path_buf(),
            },
        }
    }

    fn names(rd: Option<&mut LsReadDir>) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(rd) = rd {
            for e in rd {
                if let Ok(LsDirEntry::Vf { name, .. }) = e {
                    out.push(name.to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn open_many_lists_all_dirs() {
        let root = tempfile::tempdir().unwrap();
        for d in ["d1", "d2"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
            std::fs::write(root.path().join(d).join("f.txt"), b"x").unwrap();
        }
        let mut ctx = dummy_ctx(root.path());
        let dirs = [root.path().join("d1"), root.path().join("d2")];
        let paths: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        let masks = MetadataFields::MODE | MetadataFields::SIZE;
        let mut out = ctx_open_many(&mut ctx, &paths, masks);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(Option::is_some));
        assert_eq!(names(out[0].as_mut()), vec!["f.txt"]);
        assert_eq!(names(out[1].as_mut()), vec!["f.txt"]);
    }

    #[test]
    fn open_many_preserves_duplicate_operands_and_high_level_metadata() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::fs::write(root.path().join("d/item"), b"hello").unwrap();
        let mut ctx = dummy_ctx(root.path());
        let path = root.path().join("d");
        let mut results = ctx_open_many(
            &mut ctx,
            &[path.as_path(), path.as_path()],
            MetadataFields::MODE | MetadataFields::SIZE | MetadataFields::NLINK,
        );
        assert_eq!(results.len(), 2);
        for result in &mut results {
            let entry = result.as_mut().unwrap().next().unwrap().unwrap();
            let LsDirEntry::Vf { name, entry, .. } = entry else {
                panic!("expected a vnfs entry");
            };
            assert_eq!(name, "item");
            let metadata = crate::meta::LsMeta::Vf(entry.metadata().clone());
            assert_eq!(metadata.len(), 5);
            assert!(metadata.mode() != 0);
            assert!(metadata.nlink() >= 1);
            assert!(result.as_mut().unwrap().next().is_none());
        }
    }

    #[test]
    fn open_many_discards_partial_listings_after_failing_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("d0")).unwrap();
        std::fs::write(root.path().join("d0/a.txt"), b"a").unwrap();
        // d1 does not exist.
        std::fs::create_dir_all(root.path().join("d2")).unwrap();
        std::fs::write(root.path().join("d2/b.txt"), b"b").unwrap();
        let mut ctx = dummy_ctx(root.path());
        let dirs = [
            root.path().join("d0"),
            root.path().join("d1"),
            root.path().join("d2"),
        ];
        let paths: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
        let masks = MetadataFields::MODE | MetadataFields::SIZE;
        let out = ctx_open_many(&mut ctx, &paths, masks);
        assert!(out.iter().all(Option::is_none));
    }

    #[test]
    fn high_level_walk_retains_sorted_directory_order_and_entry_metadata() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
            std::fs::write(root.path().join(name).join("file"), b"data").unwrap();
        }
        let backend = Backend::Dummy(Mounted::new(root.path()).unwrap());
        let fields = MetadataFields::MODE | MetadataFields::SIZE | MetadataFields::BLOCKS;
        let tree = backend
            .walk(Path::new("/"), fields, &mut |_, entries| {
                entries.sort_by(|a, b| b.path().cmp(a.path()));
            })
            .unwrap();
        let paths: Vec<_> = tree
            .iter()
            .map(|directory| directory.path.as_path())
            .collect();
        assert_eq!(paths, [Path::new("/"), Path::new("/b"), Path::new("/a")]);
        assert_eq!(tree[1].entries[0].metadata().len(), 4);
        assert!(tree[1].entries[0].metadata().blocks().is_some());
    }
}
