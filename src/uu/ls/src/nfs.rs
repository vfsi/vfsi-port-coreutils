// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! NFS-aware entry source for `ls`: when a target directory is mounted on NFS
//! and the `VNFS_IMPL` environment variable selects a vectorized backend
//! (`dummy` or `nfs`), directory contents are enumerated through the `vnfs`
//! `VecFs` API instead of `std::fs`.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use crate::meta::{LsDirEntry, LsReadDir};
use vnfs::DummyVecFs;
use vnfs::NfsVecFs;
use vnfs::{VecFs, VfAttrs};

/// Runtime backend selection. `VNFS_IMPL=off` (or unset) uses `std::fs`;
/// `dummy` uses the `std::fs`-backed `DummyVecFs`; `nfs` talks to the NFS
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

/// Attributes requested for every entry (all supported fields). The
/// FATTR4_NAMED_ATTR boolean is only needed by the long format (for the `+`
/// access-indicator), and costs the server a per-entry xattr enumeration, so
/// it is only requested then.
fn full_mask(config: &crate::config::Config) -> vnfs::AttrMask {
    use vnfs::AttrMask;
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

/// A concrete vectorized backend. (`VecFs` is not object-safe, so dispatch is
/// done here by matching on the enum.)
enum Backend {
    Dummy(DummyVecFs),
    Nfs(NfsVecFs),
}

impl Backend {
    fn listdir(
        &mut self,
        dir: &Path,
        masks: vnfs::AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> vnfs::VfResult<Vec<VfAttrs>> {
        match self {
            Backend::Dummy(f) => f.listdir(dir, masks, max_count, recursive),
            Backend::Nfs(f) => f.listdir(dir, masks, max_count, recursive),
        }
    }

    fn walk(
        &mut self,
        root: &Path,
        masks: vnfs::AttrMask,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> vnfs::VfResult<Vec<vnfs::WalkEntry>> {
        match self {
            Self::Dummy(f) => f.walk(root, masks, sort),
            Self::Nfs(f) => f.walk(root, masks, sort),
        }
    }

    fn listdirv(
        &mut self,
        dirs: &[&Path],
        masks: vnfs::AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> vnfs::VfRes {
        match self {
            Self::Dummy(f) => f.listdirv(dirs, masks, max_entries, recursive, cb),
            Self::Nfs(f) => f.listdirv(dirs, masks, max_entries, recursive, cb),
        }
    }
}

struct VfContext {
    backend: Backend,
    /// The kernel mountpoint this context serves; kernel paths map onto it.
    mountpoint: PathBuf,
}

impl Drop for VfContext {
    fn drop(&mut self) {
        if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
            let (n, ops, bytes, max) = vnfs::legacy::compound::compound_stats();
            if n > 0 {
                eprintln!(
                    "[vnfs] compounds={} avg_ops={:.2} max_ops={} avg_bytes={:.0} total_bytes={}",
                    n,
                    ops as f64 / n as f64,
                    max,
                    bytes as f64 / n as f64,
                    bytes
                );
            }
            let (calls, us) = vnfs::legacy::compound::rpc_stats();
            if calls > 0 {
                eprintln!(
                    "[vnfs] rpc_calls={} avg_rpc_ms={:.2} total_rpc_ms={:.1}",
                    calls,
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

/// Find the NFS mount that contains `path`, returning its mountpoint.
fn nfs_mountpoint(path: &Path) -> Option<PathBuf> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let _spec = it.next()?;
        let mount = it.next()?;
        let fstype = it.next()?;
        if fstype == "nfs" || fstype == "nfs4" {
            let mp = PathBuf::from(mount);
            if path.starts_with(&mp) {
                return Some(mp);
            }
        }
    }
    None
}

fn make_ctx(mountpoint: &Path) -> io::Result<VfContext> {
    let backend = match impl_choice() {
        Some("dummy") => Backend::Dummy(DummyVecFs::new(mountpoint.to_path_buf())),
        Some("nfs") => Backend::Nfs(NfsVecFs::connect("127.0.0.1").map_err(vf_io_error)?),
        _ => return Err(io::Error::other("VNFS_IMPL disabled")),
    };
    Ok(VfContext {
        backend,
        mountpoint: mountpoint.to_path_buf(),
    })
}

/// Open `path` for listing through the vectorized backend when applicable.
/// Returns `Ok(None)` when the path is not NFS-mounted or the backend is off.
pub fn try_open_vf(path: &Path, config: &crate::config::Config) -> io::Result<Option<LsReadDir>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(mountpoint) = nfs_mountpoint(path) else {
        return Ok(None);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.is_none() {
            let t0 = std::time::Instant::now();
            *ctx = Some(make_ctx(&mountpoint)?);
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!(
                    "[profile] connect_ms={:.1}",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        let ctx = ctx.as_mut().unwrap();

        let rel = path.strip_prefix(&ctx.mountpoint).unwrap_or(path);
        let vpath = Path::new("/").join(rel);
        let attrs = ctx
            .backend
            .listdir(&vpath, full_mask(config), 0, false)
            .map_err(vf_io_error)?;

        let mut out = Vec::with_capacity(attrs.len());
        for a in attrs {
            let name = a
                .file
                .path()
                .and_then(|p| p.file_name())
                .map(|f| f.to_os_string())
                .unwrap_or_default();
            out.push(LsDirEntry::Vf {
                path: path.join(&name),
                name,
                attrs: a,
            });
        }
        Ok(Some(LsReadDir::from_vf(out)))
    })
}

/// List several directories in one vectorized batch and map the entries back
/// to kernel paths. On a mid-batch failure the directories before the failing
/// index are returned (the entries were already collected by the callback);
/// the failing directory and everything after it come back as `None` so the
/// caller can fall back to per-directory opens (preserving `ls`'s
/// print-errors-before-headings ordering).
fn ctx_open_many(
    ctx: &mut VfContext,
    kernel_dirs: &[&Path],
    masks: vnfs::AttrMask,
) -> io::Result<Vec<Option<LsReadDir>>> {
    let vpaths: Vec<PathBuf> = kernel_dirs
        .iter()
        .map(|d| {
            let rel = d.strip_prefix(&ctx.mountpoint).unwrap_or(d);
            Path::new("/").join(rel)
        })
        .collect();
    let refs: Vec<&Path> = vpaths.iter().map(PathBuf::as_path).collect();
    let mut slot: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for (i, v) in refs.iter().enumerate() {
        slot.insert(v.to_path_buf(), i);
    }
    let mut entries: Vec<Vec<VfAttrs>> = vec![Vec::new(); refs.len()];
    let mut cb = |a: &VfAttrs, dir: &Path| {
        if let Some(&i) = slot.get(dir) {
            entries[i].push(a.clone());
        }
        true
    };
    let mut available = vec![true; refs.len()];
    if let Err(e) = ctx.backend.listdirv(&refs, masks, 0, false, &mut cb) {
        // The prefix before the failing index was collected; the failing
        // directory and the rest fall back to per-directory opens.
        let i = e
            .index_opt()
            .unwrap_or(0)
            .min(available.len().saturating_sub(1));
        for a in available.iter_mut().skip(i) {
            *a = false;
        }
    }
    let mut out = Vec::with_capacity(refs.len());
    for (i, listed) in available.iter().enumerate() {
        if !listed {
            out.push(None);
            continue;
        }
        let mut list = Vec::with_capacity(entries[i].len());
        for attrs in &entries[i] {
            let name = attrs
                .file
                .path()
                .and_then(|p| p.file_name())
                .map(|f| f.to_os_string())
                .unwrap_or_default();
            list.push(LsDirEntry::Vf {
                path: kernel_dirs[i].join(&name),
                name,
                attrs: attrs.clone(),
            });
        }
        out.push(Some(LsReadDir::from_vf(list)));
    }
    Ok(out)
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
    let Some(mountpoint) = nfs_mountpoint(dirs[0]) else {
        return Ok(None);
    };
    if dirs[1..]
        .iter()
        .any(|d| nfs_mountpoint(d) != Some(mountpoint.clone()))
    {
        return Ok(None);
    }
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.is_none() {
            *ctx = Some(make_ctx(&mountpoint)?);
        }
        let ctx = ctx.as_mut().unwrap();
        Ok(Some(ctx_open_many(ctx, dirs, full_mask(config))?))
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
) -> io::Result<Option<Vec<vnfs::WalkEntry>>> {
    if impl_choice().is_none() {
        return Ok(None);
    }
    let Some(mountpoint) = nfs_mountpoint(path) else {
        return Ok(None);
    };
    CTX.with(|c| {
        let mut ctx = c.borrow_mut();
        if ctx.is_none() {
            let t0 = std::time::Instant::now();
            *ctx = Some(make_ctx(&mountpoint)?);
            if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
                eprintln!(
                    "[profile] connect_ms={:.1}",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        let ctx = ctx.as_mut().unwrap();

        let rel = path.strip_prefix(&ctx.mountpoint).unwrap_or(path);
        let vpath = Path::new("/").join(rel);
        let mut sort = |_dir: &Path, attrs: &mut Vec<VfAttrs>| {
            crate::sort_vf_entries(attrs, config);
        };
        let t0 = std::time::Instant::now();
        let tree = ctx
            .backend
            .walk(&vpath, full_mask(config), &mut sort)
            .map_err(vf_io_error)?;
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
            w.path = ctx.mountpoint.join(rel);
            out.push(w);
        }
        Ok(Some(out))
    })
}

#[cfg(all(test, feature = "vnfs"))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn dummy_ctx(root: &Path) -> VfContext {
        VfContext {
            backend: Backend::Dummy(DummyVecFs::new(root.to_path_buf())),
            mountpoint: root.to_path_buf(),
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
        let paths: Vec<&Path> = dirs.iter().map(|d| d.as_path()).collect();
        let masks = vnfs::AttrMask::MODE | vnfs::AttrMask::SIZE;
        let mut out = ctx_open_many(&mut ctx, &paths, masks).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(Option::is_some));
        assert_eq!(names(out[0].as_mut()), vec!["f.txt"]);
        assert_eq!(names(out[1].as_mut()), vec!["f.txt"]);
    }

    #[test]
    fn open_many_falls_back_after_failing_dir() {
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
        let paths: Vec<&Path> = dirs.iter().map(|d| d.as_path()).collect();
        let masks = vnfs::AttrMask::MODE | vnfs::AttrMask::SIZE;
        let mut out = ctx_open_many(&mut ctx, &paths, masks).unwrap();
        assert!(out[0].is_some(), "prefix dir listed before the failure");
        assert!(out[1].is_none(), "failing dir falls back to per-dir open");
        assert!(out[2].is_none(), "later dirs also fall back");
        assert_eq!(names(out[0].as_mut()), vec!["a.txt"]);
    }
}
