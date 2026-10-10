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
    Attributes, DirEntry, DirectoryListing, Error as VfError, ListDirOptions, Mounted, Nfs,
    NfsClient, VfsiExt,
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
/// typed [`vnfs::Error`] (transport vs. filesystem status) and keeping the
/// operation index in the message.
fn vf_io_error(e: VfError) -> io::Error {
    e.into()
}

/// Attributes requested for every entry (all supported fields). The
/// FATTR4_NAMED_ATTR boolean is only needed by the long format (for the `+`
/// access-indicator), and costs the server a per-entry xattr enumeration, so
/// it is only requested then.
fn full_mask(config: &crate::config::Config) -> Attributes {
    use vnfs::Attributes as AttrMask;
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
    fn read_dirs(&self, dirs: &[&Path], fields: Attributes) -> vnfs::Result<Vec<DirectoryListing>> {
        match self {
            Self::Dummy(fs) => {
                fs.read_dirs_with_options(dirs, ListDirOptions::new().fields(fields))
            }
            Self::Nfs(fs) => fs.read_dirs_with_options(dirs, ListDirOptions::new().fields(fields)),
        }
        .map(|trees| trees.into_iter().flatten().collect())
    }
}

struct VfContext {
    backend: Backend,
    mount: NfsMount,
    paths: vnfs::helpers::PathMapper,
}

impl Drop for VfContext {
    fn drop(&mut self) {
        if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
            let stats = vnfs::diagnostics::take_and_reset();
            let (n, ops, bytes, max) = (
                stats.compounds,
                stats.operations,
                stats.compound_bytes,
                stats.max_operations,
            );
            if n > 0 {
                let avg_bytes = bytes.map_or_else(
                    || "unknown".into(),
                    |b| format!("{:.0}", b as f64 / n as f64),
                );
                let bytes = bytes.map_or_else(|| "unknown".into(), |b| b.to_string());
                eprintln!(
                    "[vnfs] compounds={n} avg_ops={:.2} max_ops={max} avg_bytes={avg_bytes} total_bytes={bytes}",
                    ops as f64 / n as f64
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
        Some("nfs") => Backend::Nfs(Nfs::from_mount(&mount.point).map_err(vf_io_error)?),
        _ => return Err(io::Error::other("VNFS_IMPL disabled")),
    };
    let paths = vnfs::helpers::PathMapper::new(&mount.point)?;
    Ok(VfContext {
        backend,
        mount,
        paths,
    })
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

        let vpath = ctx.paths.map(path, vnfs::helpers::ResolvePath::Follow)?;
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
    masks: Attributes,
) -> Vec<Option<LsReadDir>> {
    let vpaths: vnfs::Result<Vec<PathBuf>> = kernel_dirs
        .iter()
        .map(|d| ctx.paths.map(d, vnfs::helpers::ResolvePath::Follow))
        .collect();
    let Ok(vpaths) = vpaths else {
        return std::iter::repeat_with(|| None)
            .take(kernel_dirs.len())
            .collect();
    };
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

/// Group operands by mount and batch each eligible group independently.
/// Mixed local/NFS inputs retain their original positions. Returns `Ok(None)`
/// when the batch path does not apply, and a per-directory result vector
/// otherwise (entries, or `None` where the caller must fall back).
pub fn try_open_many_vf(
    dirs: &[&Path],
    config: &crate::config::Config,
) -> io::Result<Option<Vec<Option<LsReadDir>>>> {
    if impl_choice().is_none() || dirs.len() < 2 {
        return Ok(None);
    }
    let groups = group_mounts(dirs.iter().map(|path| nfs_mountpoint(path)));
    if groups.is_empty() {
        return Ok(None);
    }
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        let mut out: Vec<Option<LsReadDir>> =
            std::iter::repeat_with(|| None).take(dirs.len()).collect();
        for (mount, indices) in groups {
            if ctx.as_ref().is_none_or(|ctx| ctx.mount != mount) {
                *ctx = Some(make_ctx(mount)?);
            }
            let grouped: Vec<&Path> = indices.iter().map(|&i| dirs[i]).collect();
            let listings = ctx_open_many(ctx.as_mut().unwrap(), &grouped, full_mask(config));
            for (index, listing) in indices.into_iter().zip(listings) {
                out[index] = listing;
            }
        }
        Ok(Some(out))
    })
}

fn group_mounts(mounts: impl IntoIterator<Item = Option<NfsMount>>) -> Vec<(NfsMount, Vec<usize>)> {
    let mut groups: Vec<(NfsMount, Vec<usize>)> = Vec::new();
    for (index, mount) in mounts.into_iter().enumerate() {
        let Some(mount) = mount else {
            continue;
        };
        if let Some((_, indices)) = groups.iter_mut().find(|(candidate, _)| *candidate == mount) {
            indices.push(index);
        } else {
            groups.push((mount, vec![index]));
        }
    }
    groups
}

/// Walk the whole subtree rooted at `path` through the vectorized backend,
/// returning each directory with its entries, ordered exactly as `ls` would
/// list them (via [`sort_entries`] semantics, so it is correct under any
/// locale and sort mode). Paths in the result are mapped back to kernel paths
/// (under the mountpoint).
pub fn try_visit_walk_vf(
    path: &Path,
    config: &crate::config::Config,
    mut callback: impl FnMut(WalkEntry) -> uucore::error::UResult<()>,
) -> uucore::error::UResult<bool> {
    // A comparator cannot reverse the server's unsorted enumeration order.
    if impl_choice().is_none() || (config.sort == crate::config::Sort::None && config.reverse) {
        return Ok(false);
    }
    let Some(mount) = nfs_mountpoint(path) else {
        return Ok(false);
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

        let t0 = std::time::Instant::now();
        match &ctx.backend {
            Backend::Dummy(fs) => visit_ordered(fs, &ctx.mount.point, path, config, &mut callback)?,
            Backend::Nfs(fs) => visit_ordered(fs, &ctx.mount.point, path, config, &mut callback)?,
        }
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!(
                "[profile] walk_ms={:.1}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }

        Ok(true)
    })
}

fn visit_ordered<F: vnfs::Vfsi>(
    fs: &F,
    mount: &Path,
    path: &Path,
    config: &crate::config::Config,
    callback: &mut impl FnMut(WalkEntry) -> uucore::error::UResult<()>,
) -> uucore::error::UResult<()> {
    use vnfs::{
        WalkControl,
        helpers::{PathMapper, ResolvePath},
    };
    let session = PathMapper::new(mount).map_err(vf_io_error)?;
    let root = session
        .map(path, ResolvePath::Follow)
        .map_err(vf_io_error)?;
    let mut output_error = None;
    let result = fs.visit_dirs_ordered(
        root,
        fs.limits().walk_options().fields(full_mask(config)),
        crate::vf_entry_order(config),
        |entry| {
            entry
                .file_name()
                .is_some_and(|name| crate::display::should_display(name, config))
        },
        |listing, depth| {
            let rendered = if depth == 0 {
                path.to_path_buf()
            } else {
                session.local_path(&listing.path)?
            };
            if let Err(error) = callback(WalkEntry {
                path: rendered,
                entries: listing.entries,
            }) {
                output_error = Some(error);
                return Ok(WalkControl::Stop);
            }
            Ok(WalkControl::Continue)
        },
    );
    if let Some(error) = output_error {
        return Err(error);
    }
    result.map_err(vf_io_error)?;
    Ok(())
}

#[cfg(all(test, all(feature = "vnfs", target_os = "linux")))]
mod tests {
    use super::*;
    use std::path::Path;
    use vnfs::Vfsi;

    fn dummy_ctx(root: &Path) -> VfContext {
        VfContext {
            paths: vnfs::helpers::PathMapper::new(root).unwrap(),
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
    fn groups_mixed_mounts_and_duplicates_without_losing_original_indices() {
        let mount = NfsMount {
            server: "a".into(),
            export: "/export".into(),
            point: "/mnt/a".into(),
        };
        let other = NfsMount {
            point: "/mnt/b".into(),
            ..mount.clone()
        };
        assert_eq!(
            group_mounts([
                None,
                Some(mount.clone()),
                Some(other.clone()),
                Some(mount.clone()),
                None
            ]),
            [(mount, vec![1, 3]), (other, vec![2])]
        );
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
        let masks = Attributes::MODE | Attributes::SIZE;
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
            Attributes::MODE | Attributes::SIZE | Attributes::NLINK,
        );
        assert_eq!(results.len(), 2);
        for result in &mut results {
            let entry = result.as_mut().unwrap().next().unwrap().unwrap();
            let LsDirEntry::Vf { name, entry, .. } = entry else {
                panic!("expected a vnfs entry");
            };
            assert_eq!(name, "item");
            let metadata = crate::meta::LsMeta::Vf(entry.attrs().clone());
            assert_eq!(metadata.len(), Some(5));
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
        let masks = Attributes::MODE | Attributes::SIZE;
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
        let backend = Mounted::new(root.path()).unwrap();
        let fields = Attributes::MODE | Attributes::SIZE | Attributes::BLOCKS;
        let mut tree = Vec::new();
        backend
            .visit_dirs_ordered(
                "/",
                backend.limits().walk_options().fields(fields),
                |a, b| b.path().cmp(a.path()),
                |_| true,
                |listing, _| {
                    tree.push(WalkEntry {
                        path: listing.path,
                        entries: listing.entries,
                    });
                    Ok(vnfs::WalkControl::Continue)
                },
            )
            .unwrap();
        let paths: Vec<_> = tree
            .iter()
            .map(|directory| directory.path.as_path())
            .collect();
        assert_eq!(paths, [Path::new("/"), Path::new("/b"), Path::new("/a")]);
        assert_eq!(tree[1].entries[0].attrs().len(), Some(4));
        assert!(tree[1].entries[0].attrs().blocks().is_some());
    }

    #[test]
    fn vector_order_matches_normal_sort_modes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        for (name, contents) in [("a10.txt", "x"), ("a2.rs", "longer"), ("bb.txt", "zz")] {
            std::fs::write(root.path().join(name), contents).unwrap();
        }
        let fs = Mounted::new(root.path()).unwrap();
        let entries = fs.read_dir("/").unwrap();
        let matches = crate::uu_app().get_matches_from(["ls"]);
        let mut config = crate::config::Config::from(&matches, None).unwrap();
        for sort in [
            "time",
            "size",
            "name",
            "version",
            "extension",
            "width",
            "none",
        ] {
            for reverse in [false, true] {
                if sort == "none" && reverse {
                    continue; // This mode retains the ordinary kernel traversal.
                }
                for group in [false, true] {
                    config.sort = match sort {
                        "time" => crate::config::Sort::Time,
                        "size" => crate::config::Sort::Size,
                        "name" => crate::config::Sort::Name,
                        "version" => crate::config::Sort::Version,
                        "extension" => crate::config::Sort::Extension,
                        "width" => crate::config::Sort::Width,
                        _ => crate::config::Sort::None,
                    };
                    config.reverse = reverse;
                    config.group_directories_first = group;
                    let mut vector = entries.clone();
                    vector.sort_by(crate::vf_entry_order(&config));
                    let mut normal: Vec<_> = entries
                        .iter()
                        .map(|entry| {
                            let name = entry.file_name().unwrap().to_os_string();
                            crate::PathData::from_vf(
                                root.path().join(&name),
                                name,
                                entry.clone(),
                                &config,
                            )
                        })
                        .collect();
                    crate::sort_entries(&mut normal, &config);
                    assert_eq!(
                        vector
                            .iter()
                            .map(|entry| entry.file_name().unwrap())
                            .collect::<Vec<_>>(),
                        normal
                            .iter()
                            .map(crate::PathData::file_name)
                            .collect::<Vec<_>>(),
                        "sort={sort}, reverse={reverse}, group={group}",
                    );
                }
            }
        }
    }
}
