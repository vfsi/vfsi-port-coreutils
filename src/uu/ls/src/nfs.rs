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
use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
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

/// Attributes requested for every entry (all supported fields). The
/// FATTR4_NAMED_ATTR boolean is only needed by the long format (for the `+`
/// access-indicator), and costs the server a per-entry xattr enumeration, so
/// it is only requested then.
fn full_mask(config: &crate::config::Config) -> vnfs::AttrMask {
    use vnfs::AttrMask;
    AttrMask {
        has_mode: true,
        has_size: true,
        has_nlink: true,
        has_fileid: true,
        has_blocks: true,
        has_uid: true,
        has_gid: true,
        has_rdev: true,
        has_atime: true,
        has_mtime: true,
        has_ctime: true,
        has_named_attr: config.format == crate::display::Format::Long,
    }
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
        dir: &str,
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
        root: &str,
        masks: vnfs::AttrMask,
        sort: &dyn Fn(&str, &mut Vec<VfAttrs>),
    ) -> vnfs::VfResult<Vec<vnfs::WalkEntry>> {
        match self {
            Self::Dummy(f) => f.walk(root, masks, sort),
            Self::Nfs(f) => f.walk(root, masks, sort),
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
            let (n, ops, bytes, max) = vnfs::compound::compound_stats();
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
            let (calls, us) = vnfs::compound::rpc_stats();
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
        Some("nfs") => Backend::Nfs(
            NfsVecFs::connect("127.0.0.1").map_err(|e| io::Error::other(e.to_string()))?,
        ),
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
        let vpath = format!("/{}", rel.to_string_lossy());
        let attrs = ctx
            .backend
            .listdir(&vpath, full_mask(config), 0, false)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut out = Vec::with_capacity(attrs.len());
        for a in attrs {
            let name = a
                .file
                .path()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(LsDirEntry::Vf {
                path: path.join(&name),
                name: name.into(),
                attrs: a,
            });
        }
        Ok(Some(LsReadDir::from_vf(out)))
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
        let vpath = format!("/{}", rel.to_string_lossy());
        let sort = |_dir: &str, attrs: &mut Vec<VfAttrs>| {
            crate::sort_vf_entries(attrs, config);
        };
        let t0 = std::time::Instant::now();
        let tree = ctx
            .backend
            .walk(&vpath, full_mask(config), &sort)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if std::env::var("VNFS_PROFILE").as_deref() == Ok("1") {
            eprintln!(
                "[profile] walk_ms={:.1}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }

        let mut out = Vec::with_capacity(tree.len());
        for mut w in tree {
            // Rewrite root-relative vnfs paths back to kernel paths.
            w.path = ctx
                .mountpoint
                .join(w.path.trim_start_matches('/'))
                .to_string_lossy()
                .into_owned();
            out.push(w);
        }
        Ok(Some(out))
    })
}
