// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VNFS removal support for paths on Linux NFS mounts.

use std::path::{Component, Path, PathBuf};

use vnfs::Result as VfResult;
use vnfs::directory::{Attributes, DirectoryListing, ListDirOptions};
use vnfs::directory::{RemoveMode, RemoveOptions};
use vnfs::files::{Capabilities, CopyOptions, ReadOptions, StreamOptions, Vfsi, VfsiExt};
use vnfs::helpers::{PathMapper, ResolvePath};
use vnfs::nfs::Nfs;
use vnfs::nfs::mount::NfsMountSession;
use vnfs::posix::Posix;

pub use vnfs::nfs::mount::NfsMount;

/// Find the longest NFS mount (if any) that contains `path`.
///
/// The path is canonicalized first so that relative operands and symlinked
/// directories resolve to the same mount a kernel traversal would see.
pub fn nfs_mount(path: &Path) -> Option<NfsMount> {
    let resolved = path.canonicalize().ok()?;
    let directory = if resolved.is_dir() {
        resolved.as_path()
    } else {
        resolved.parent()?
    };
    Nfs::discover_mount(directory).ok()
}

/// Compare namespace grouping keys, not session reuse or authorization state.
pub fn same_mount(a: &NfsMount, b: &NfsMount) -> bool {
    a.host() == b.host() && a.export_root() == b.export_root() && a.mount_point() == b.mount_point()
}

/// Resolve one selected group's root once, retaining the complete discovery
/// snapshot for connection validation and later session reuse.
pub fn mount_root(mount: NfsMount) -> Option<NfsMount> {
    if mount.local_path() == mount.mount_point() {
        return Some(mount);
    }
    let root = Nfs::discover_mount(mount.mount_point()).ok()?;
    same_mount(&mount, &root).then_some(root)
}

/// One command's mounted namespace and backend. Output paths remain kernel
/// paths; direct NFS I/O still has a separate cache from the kernel client.
pub enum Session {
    Dummy {
        fs: Posix,
        paths: PathMapper,
    },
    Nfs {
        session: Box<NfsMountSession>,
        copy_capabilities: std::cell::OnceCell<Capabilities>,
    },
}

impl Session {
    pub fn open(root: &NfsMount) -> std::io::Result<Self> {
        match std::env::var("VNFS_IMPL").as_deref() {
            Ok("dummy") => Ok(Self::Dummy {
                fs: Posix::new(root.local_path())?,
                paths: PathMapper::new(root.local_path())?,
            }),
            Ok("nfs") => {
                let session = NfsMountSession::from_discovered(root.clone())?;
                Ok(Self::Nfs {
                    session: Box::new(session),
                    copy_capabilities: std::cell::OnceCell::new(),
                })
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "VNFS_IMPL disabled",
            )),
        }
    }
    /// Reuse only an unchanged mount/root/policy/credential discovery snapshot.
    /// A command may retain a matching session while keeping data caches scoped
    /// to that command. No filesystem effects are submitted by this check.
    pub fn matches(&self, root: &NfsMount) -> bool {
        match self {
            Self::Dummy { paths, .. } => paths.local_root() == root.local_path(),
            Self::Nfs { session, .. } => session.mount() == root,
        }
    }

    pub fn local_root(&self) -> &Path {
        match self {
            Self::Dummy { paths, .. } => paths.local_root(),
            Self::Nfs { session, .. } => session.local_root(),
        }
    }

    pub fn map(&self, path: &Path, policy: ResolvePath) -> VfResult<PathBuf> {
        match self {
            Self::Dummy { paths, .. } => paths.map(path, policy),
            Self::Nfs { session, .. } => session.map(path, policy),
        }
    }
    pub fn map_many(&self, paths: &[PathBuf], policy: ResolvePath) -> VfResult<Vec<PathBuf>> {
        match self {
            Self::Dummy { paths: mapper, .. } => mapper.map_many(paths, policy),
            Self::Nfs { session, .. } => session.map_many(paths, policy),
        }
    }
    pub fn attrs(
        &self,
        paths: &[PathBuf],
        fields: Attributes,
    ) -> VfResult<Vec<vnfs::directory::Attrs>> {
        let options = vnfs::directory::AttrsOptions::new()
            .with_fields(fields)
            .with_follow_final_symlink(false);
        match self {
            Self::Dummy { fs, .. } => fs.vgetattrs(paths, options),
            Self::Nfs { session, .. } => session.fs().vgetattrs(paths, options),
        }
    }
    pub fn read_dirs(&self, dirs: &[&Path], fields: Attributes) -> VfResult<Vec<DirectoryListing>> {
        let options = ListDirOptions::new().with_fields(fields);
        let trees = match self {
            Self::Dummy { fs, .. } => fs.read_dirs_with_options(dirs, options),
            Self::Nfs { session, .. } => session.fs().read_dirs_with_options(dirs, options),
        }?;
        let trees: Vec<DirectoryListing> = trees.into_iter().flatten().collect();
        for listing in &trees {
            for entry in &listing.entries {
                entry.attrs().require(fields - Attributes::NAMED_ATTR)?;
            }
        }
        Ok(trees)
    }
    pub fn read_files(&self, paths: &[PathBuf], limit: usize) -> VfResult<Vec<Vec<u8>>> {
        let limit = std::num::NonZeroUsize::new(limit)
            .ok_or_else(|| vnfs::Error::errno(0, libc::EINVAL as u32))?;
        let options = ReadOptions::new().with_max_total_bytes(limit);
        match self {
            Self::Dummy { fs, .. } => fs.read_files_with_options(paths, options),
            Self::Nfs { session, .. } => session.fs().read_files_with_options(paths, options),
        }
    }
    pub fn copy_to_writer(&self, path: &Path, writer: &mut impl std::io::Write) -> VfResult<u64> {
        match self {
            Self::Dummy { fs, .. } => {
                vnfs::helpers::copy_to_writer(fs, path, writer, StreamOptions::default())
            }
            Self::Nfs { session, .. } => {
                vnfs::helpers::copy_to_writer(session.fs(), path, writer, StreamOptions::default())
            }
        }
    }
    pub fn copy_files(&self, pairs: &[(PathBuf, PathBuf)]) -> Option<VfResult<()>> {
        let Self::Nfs {
            session,
            copy_capabilities,
        } = self
        else {
            return None;
        };
        let capabilities = if let Some(capabilities) = copy_capabilities.get() {
            *capabilities
        } else {
            let capabilities = match session.fs().capabilities() {
                Ok(capabilities) => capabilities,
                Err(error) => return Some(Err(error)),
            };
            let _ = copy_capabilities.set(capabilities);
            capabilities
        };
        capabilities
            .contains(Capabilities::SERVER_COPY)
            .then(|| session.fs().vcopy(pairs, CopyOptions::default()))
    }
    fn remove(&self, path: &Path, recursive: bool) -> VfResult<()> {
        let mode = if recursive {
            RemoveMode::Tree
        } else {
            RemoveMode::Entry
        };
        match self {
            Self::Dummy { fs, .. } => fs.vremove(&[path], mode, RemoveOptions::default()),
            Self::Nfs { session, .. } => {
                session
                    .fs()
                    .vremove(&[path], mode, RemoveOptions::default())
            }
        }
    }
}

fn remove_paths(path: &Path) -> Option<(NfsMount, PathBuf)> {
    let Component::Normal(name) = path.components().next_back()? else {
        return None;
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let parent = parent.canonicalize().ok()?;
    let info = Nfs::discover_mount(&parent).ok()?;
    let point = info.mount_point().to_path_buf();
    let mount = info;
    let absolute = parent.join(name);

    // Removing an export's mount point through its server-side path would
    // target the export root instead of the local mount point.
    if absolute == point {
        return None;
    }

    let session = PathMapper::new(mount.local_path()).ok()?;
    let dummy_path = session.map(path, ResolvePath::NoFollow).ok()?;
    Some((mount, dummy_path))
}

thread_local! {
    static REMOVE_SESSION: std::cell::RefCell<Option<(String, NfsMount, Session)>> =
        const { std::cell::RefCell::new(None) };
}

/// Try to remove `path` through VNFS.
///
/// `None` means no operation was submitted and the kernel path may be used.
/// Once submitted, return its result even on failure: a direct recursive
/// removal can have partial effects and must not be replayed via the kernel.
pub fn try_remove(path: &Path, recursive: bool) -> Option<std::io::Result<()>> {
    let (mount, relative_path) = remove_paths(path)?;

    let implementation = std::env::var("VNFS_IMPL").ok()?;
    if implementation != "dummy" && implementation != "nfs" {
        return None;
    }
    REMOVE_SESSION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|(mode, config, _)| mode != &implementation || config != &mount)
        {
            let backend = if implementation == "dummy" {
                Session::Dummy {
                    fs: Posix::new(mount.local_path()).ok()?,
                    paths: PathMapper::new(mount.local_path()).ok()?,
                }
            } else {
                let session = NfsMountSession::from_discovered(mount.clone()).ok()?;
                Session::Nfs {
                    session: Box::new(session),
                    copy_capabilities: std::cell::OnceCell::new(),
                }
            };
            *slot = Some((implementation, mount, backend));
        }
        Some(
            slot.as_ref()?
                .2
                .remove(&relative_path, recursive)
                .map_err(Into::into),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let fs = Posix::new(root.path()).unwrap();

        fs.vremove(&["/empty"], RemoveMode::Entry, RemoveOptions::default())
            .unwrap();

        assert!(!empty.exists());
    }

    #[test]
    fn removes_tree_recursively() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join("child")).unwrap();
        std::fs::write(tree.join("child/file"), b"data").unwrap();
        let fs = Posix::new(root.path()).unwrap();

        fs.vremove(&["/tree"], RemoveMode::Tree, RemoveOptions::default())
            .unwrap();

        assert!(!tree.exists());
    }

    #[test]
    fn non_recursive_remove_keeps_nonempty_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), b"data").unwrap();
        let fs = Posix::new(root.path()).unwrap();

        assert!(
            fs.vremove(&["/dir"], RemoveMode::Entry, RemoveOptions::default())
                .is_err()
        );
        assert!(dir.exists());
    }
    #[test]
    #[ignore = "requires VFSI_TEST_MOUNT pointing at a writable NFS fixture"]
    fn copy_capabilities_are_lazy_and_cached_after_a_successful_query() {
        let root = std::env::var("VFSI_TEST_MOUNT").expect("required NFS fixture");
        let mount = Nfs::discover_mount(&root).unwrap();
        let backend = Session::Nfs {
            session: Box::new(NfsMountSession::from_discovered(mount).unwrap()),
            copy_capabilities: std::cell::OnceCell::new(),
        };
        let Session::Nfs {
            copy_capabilities, ..
        } = &backend
        else {
            unreachable!()
        };
        assert!(copy_capabilities.get().is_none());
        backend
            .read_dirs(&[Path::new("/")], Attributes::MODE)
            .unwrap();
        assert!(
            copy_capabilities.get().is_none(),
            "listing must not query COPY capabilities"
        );
        if let Some(result) = backend.copy_files(&[]) {
            result.unwrap();
        }
        assert!(copy_capabilities.get().is_some());
        if let Some(result) = backend.copy_files(&[]) {
            result.unwrap();
        }
    }
}
