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
use uucore::vnfs::Session as Backend;
use vnfs::Error as VfError;
use vnfs::directory::{Attributes, DirEntry};
use vnfs::files::VfsiExt;
use vnfs::helpers::{PathMapper, ResolvePath};
#[cfg(test)]
use vnfs::posix::Posix;

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

/// Request metadata consumed by the selected rendering and sorting modes.
/// `Session::read_dirs` validates required fields before exposing any listing.
fn attr_mask(config: &crate::config::Config) -> Attributes {
    use crate::{config::Sort, display::Format};
    use uucore::fsext::MetadataTimeField;
    let mut mask = Attributes::MODE;
    if config.format == Format::Long {
        mask |= Attributes::SIZE
            | Attributes::NLINK
            | Attributes::BLOCKS
            | Attributes::RDEV
            | Attributes::NAMED_ATTR;
        if config.long.owner || config.long.author {
            mask |= Attributes::UID;
        }
        if config.long.group {
            mask |= Attributes::GID;
        }
    }
    if config.inode {
        mask |= Attributes::FILEID;
    }
    if config.alloc_size {
        mask |= Attributes::BLOCKS;
    }
    if config.color.is_some() {
        mask |= Attributes::NLINK;
    }
    if config.sort == Sort::Size {
        mask |= Attributes::SIZE;
    }
    if config.format == Format::Long || config.sort == Sort::Time {
        mask |= match config.time {
            MetadataTimeField::Modification => Attributes::MTIME,
            MetadataTimeField::Access => Attributes::ATIME,
            MetadataTimeField::Change => Attributes::CTIME,
            MetadataTimeField::Birth => Attributes::empty(),
        };
    }
    mask
}

struct VfContext {
    backend: Backend,
}

impl Drop for VfContext {
    fn drop(&mut self) {
        if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
            eprint!("{}", vnfs::diagnostics::take_and_reset());
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
    let backend = Backend::open(&mount)?;
    Ok(VfContext { backend })
}

/// Open `path` for listing through the vectorized backend when applicable.
/// Returns `Ok(None)` when the path is not NFS-mounted or the backend is off.
pub fn try_open_vf(path: &Path, config: &crate::config::Config) -> io::Result<Option<LsReadDir>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(mount) = nfs_mountpoint(path).and_then(uucore::vnfs::mount_root) else {
        return Ok(None);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.as_ref().is_none_or(|ctx| !ctx.backend.matches(&mount)) {
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

        let vpath = ctx.backend.map(path, ResolvePath::Follow)?;
        let listing = match ctx.backend.read_dirs(&[&vpath], attr_mask(config)) {
            Ok(mut listings) => match listings.pop() {
                Some(listing) => listing,
                None => return Err(io::Error::other("vnfs returned no directory listing")),
            },
            Err(error) if error.raw_code() == uucore::libc::EFBIG as u32 => return Ok(None),
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
    let mapped: Vec<_> = kernel_dirs
        .iter()
        .map(|path| ctx.backend.map(path, ResolvePath::Follow).ok())
        .collect();
    let mut out: Vec<Option<LsReadDir>> = std::iter::repeat_with(|| None)
        .take(kernel_dirs.len())
        .collect();
    let refs: Vec<&Path> = mapped.iter().filter_map(Option::as_deref).collect();
    if refs.is_empty() {
        return out;
    }
    let Ok(listings) = ctx.backend.read_dirs(&refs, masks) else {
        // A backend error does not prove which interleaved streams completed.
        // Only mapping failures can be isolated without replaying reads.
        return out;
    };
    if listings.len() != refs.len() {
        return out;
    }
    let indices = mapped
        .iter()
        .enumerate()
        .filter_map(|(index, path)| path.is_some().then_some(index));
    for (index, listing) in indices.zip(listings) {
        let mut list = Vec::with_capacity(listing.entries.len());
        for entry in listing.entries {
            let name = entry
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default();
            list.push(LsDirEntry::Vf {
                path: kernel_dirs[index].join(&name),
                name,
                entry,
            });
        }
        out[index] = Some(LsReadDir::from_vf(list));
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
    let groups = group_mounts(
        dirs.iter().map(|path| nfs_mountpoint(path)),
        uucore::vnfs::same_mount,
    );
    if groups.is_empty() {
        return Ok(None);
    }
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        let mut out: Vec<Option<LsReadDir>> =
            std::iter::repeat_with(|| None).take(dirs.len()).collect();
        for (mount, indices) in groups {
            let Some(mount) = uucore::vnfs::mount_root(mount) else {
                continue;
            };
            if ctx.as_ref().is_none_or(|ctx| !ctx.backend.matches(&mount)) {
                *ctx = Some(make_ctx(mount)?);
            }
            let grouped: Vec<&Path> = indices.iter().map(|&i| dirs[i]).collect();
            let listings = ctx_open_many(ctx.as_mut().unwrap(), &grouped, attr_mask(config));
            for (index, listing) in indices.into_iter().zip(listings) {
                out[index] = listing;
            }
        }
        Ok(Some(out))
    })
}

fn group_mounts<M>(
    mounts: impl IntoIterator<Item = Option<M>>,
    same: impl Fn(&M, &M) -> bool,
) -> Vec<(M, Vec<usize>)> {
    let mut groups: Vec<(M, Vec<usize>)> = Vec::new();
    for (index, mount) in mounts.into_iter().enumerate() {
        let Some(mount) = mount else {
            continue;
        };
        if let Some((_, indices)) = groups
            .iter_mut()
            .find(|(candidate, _)| same(candidate, &mount))
        {
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
    let Some(mount) = nfs_mountpoint(path).and_then(uucore::vnfs::mount_root) else {
        return Ok(false);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.as_ref().is_none_or(|ctx| !ctx.backend.matches(&mount)) {
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
            Backend::Dummy { fs, .. } => {
                visit_ordered(fs, ctx.backend.local_root(), path, config, &mut callback)?;
            }
            Backend::Nfs { session, .. } => {
                visit_ordered(
                    session.fs(),
                    ctx.backend.local_root(),
                    path,
                    config,
                    &mut callback,
                )?;
            }
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
    use vnfs::directory::WalkControl;
    let session = PathMapper::new(mount).map_err(vf_io_error)?;
    let root = session
        .map(path, ResolvePath::Follow)
        .map_err(vf_io_error)?;
    let mut output_error = None;
    let result = fs.visit_dirs_ordered(
        root,
        fs.limits().walk_options().with_fields(attr_mask(config)),
        crate::vf_entry_order(config, &session),
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
            backend: Backend::Dummy {
                fs: Posix::new(root).unwrap(),
                paths: PathMapper::new(root).unwrap(),
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
    fn masks_follow_rendering_and_sorting_requirements() {
        for (args, required, absent) in [
            (
                vec!["ls", "-1", "--color=never"],
                Attributes::MODE,
                Attributes::SIZE
                    | Attributes::ATIME
                    | Attributes::MTIME
                    | Attributes::UID
                    | Attributes::GID
                    | Attributes::FILEID
                    | Attributes::BLOCKS
                    | Attributes::NLINK
                    | Attributes::RDEV,
            ),
            (
                vec!["ls", "-i", "-s"],
                Attributes::FILEID | Attributes::BLOCKS,
                Attributes::SIZE | Attributes::MTIME,
            ),
            (
                vec!["ls", "-S"],
                Attributes::SIZE,
                Attributes::MTIME | Attributes::CTIME,
            ),
            (
                vec!["ls", "-tu"],
                Attributes::ATIME,
                Attributes::MTIME | Attributes::CTIME,
            ),
            (
                vec!["ls", "-g"],
                Attributes::NLINK | Attributes::SIZE | Attributes::GID,
                Attributes::UID | Attributes::FILEID,
            ),
            (
                vec!["ls", "-o"],
                Attributes::NLINK | Attributes::SIZE | Attributes::UID,
                Attributes::GID | Attributes::FILEID,
            ),
            (
                vec!["ls", "-go"],
                Attributes::NLINK | Attributes::SIZE,
                Attributes::UID | Attributes::GID,
            ),
            (
                vec!["ls", "-l"],
                Attributes::MODE
                    | Attributes::SIZE
                    | Attributes::MTIME
                    | Attributes::UID
                    | Attributes::GID,
                Attributes::ATIME | Attributes::CTIME,
            ),
        ] {
            let matches = crate::uu_app().get_matches_from(args);
            let config = crate::config::Config::from(&matches, None).unwrap();
            let mask = attr_mask(&config);
            assert!(mask.contains(required), "missing {required:?} in {mask:?}");
            assert!(!mask.intersects(absent), "unneeded attributes in {mask:?}");
        }
    }

    #[test]
    fn groups_mixed_mounts_and_duplicates_without_losing_original_indices() {
        let mount = ("a", "/export", "/mnt/a");
        let other = ("a", "/export", "/mnt/b");
        assert_eq!(
            group_mounts(
                [None, Some(mount), Some(other), Some(mount), None],
                |a, b| a == b
            ),
            [(mount, vec![1, 3]), (other, vec![2])]
        );
    }

    #[test]
    fn open_many_mapping_failure_preserves_other_operands_and_duplicates() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("good")).unwrap();
        std::fs::write(root.path().join("good/file"), b"data").unwrap();
        let mut ctx = dummy_ctx(root.path());
        let dirs = [
            root.path().join("good"),
            root.path().join("vanished"),
            root.path().join("good"),
        ];
        let paths: Vec<_> = dirs.iter().map(PathBuf::as_path).collect();
        let mut result = ctx_open_many(&mut ctx, &paths, Attributes::MODE);
        assert_eq!(result.len(), 3);
        assert_eq!(names(result[0].as_mut()), ["file"]);
        assert!(result[1].is_none());
        assert_eq!(names(result[2].as_mut()), ["file"]);
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
        // Mapping succeeds, but listing a regular file must fail.
        std::fs::write(root.path().join("d1"), b"not a directory").unwrap();
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
        let backend = Posix::new(root.path()).unwrap();
        let fields = Attributes::MODE | Attributes::SIZE | Attributes::BLOCKS;
        let mut tree = Vec::new();
        backend
            .visit_dirs_ordered(
                "/",
                backend.limits().walk_options().with_fields(fields),
                |a, b| b.path().cmp(a.path()),
                |_| true,
                |listing, _| {
                    tree.push(WalkEntry {
                        path: listing.path,
                        entries: listing.entries,
                    });
                    Ok(vnfs::directory::WalkControl::Continue)
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
    fn grouping_target_cache_is_scoped_to_one_directory() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["first", "second", "target-dir"] {
            std::fs::create_dir(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join("target-file"), b"").unwrap();
        std::fs::write(root.path().join("first/a-file"), b"").unwrap();
        std::fs::write(root.path().join("second/a-file"), b"").unwrap();
        std::fs::write(root.path().join("second/b-file"), b"").unwrap();
        let link = root.path().join("first/z-link");
        std::os::unix::fs::symlink("../target-file", &link).unwrap();
        let fs = Posix::new(root.path()).unwrap();
        let first = fs.read_dir("/first").unwrap();
        let second = fs.read_dir("/second").unwrap();
        let find = |name| {
            first
                .iter()
                .find(|entry| entry.file_name().unwrap() == name)
                .unwrap()
        };
        let config = crate::config::Config::from(
            &crate::uu_app().get_matches_from(["ls", "--group-directories-first"]),
            None,
        )
        .unwrap();
        let mapper = PathMapper::new(root.path()).unwrap();
        let mut compare = crate::vf_entry_order(&config, &mapper);
        assert_eq!(
            compare(find("z-link"), find("a-file")),
            std::cmp::Ordering::Greater
        );
        let _ = compare(&second[0], &second[1]);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("../target-dir", &link).unwrap();
        assert_eq!(
            compare(find("z-link"), find("a-file")),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn vector_order_matches_normal_sort_modes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        for (name, contents) in [("a10.txt", "x"), ("a2.rs", "longer"), ("bb.txt", "zz")] {
            std::fs::write(root.path().join(name), contents).unwrap();
        }
        std::os::unix::fs::symlink("dir", root.path().join("a-dir-link")).unwrap();
        std::os::unix::fs::symlink("missing", root.path().join("broken-link")).unwrap();
        let fs = Posix::new(root.path()).unwrap();
        let entries = fs.read_dir("/").unwrap();
        let mapper = PathMapper::new(root.path()).unwrap();
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
                    vector.sort_by(crate::vf_entry_order(&config, &mapper));
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
