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

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mount {
    config: vnfs::nfs::mount::NfsMount,
}

/// An NFS mount that contains a path: the server, the exported path, and the
/// local mountpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NfsMount {
    pub server: String,
    pub export: PathBuf,
    pub point: PathBuf,
}

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
    let mount = Nfs::discover_mount(directory).ok()?;
    Some(NfsMount {
        server: mount.host().to_owned(),
        export: mount.export_root().to_path_buf(),
        point: mount.mount_point().to_path_buf(),
    })
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
        supports_copy: bool,
    },
}

impl Session {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        match std::env::var("VNFS_IMPL").as_deref() {
            Ok("dummy") => Ok(Self::Dummy {
                fs: Posix::new(root)?,
                paths: PathMapper::new(root)?,
            }),
            Ok("nfs") => {
                let session = NfsMountSession::from_mount(root)?;
                let supports_copy = session
                    .fs()
                    .capabilities()?
                    .contains(Capabilities::SERVER_COPY);
                Ok(Self::Nfs {
                    session: Box::new(session),
                    supports_copy,
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
    pub fn matches(&self, root: &Path) -> bool {
        match self {
            Self::Dummy { paths, .. } => paths.local_root() == root,
            Self::Nfs { session, .. } => {
                Nfs::discover_mount(root).is_ok_and(|config| &config == session.mount())
            }
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
        match self {
            Self::Nfs {
                session,
                supports_copy: true,
            } => Some(session.fs().vcopy(pairs, CopyOptions::default())),
            _ => None,
        }
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

fn remove_paths(path: &Path) -> Option<(Mount, PathBuf)> {
    let Component::Normal(name) = path.components().next_back()? else {
        return None;
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let parent = parent.canonicalize().ok()?;
    let info = Nfs::discover_mount(&parent).ok()?;
    let point = info.mount_point().to_path_buf();
    let mount = Mount { config: info };
    let absolute = parent.join(name);

    // Removing an export's mount point through its server-side path would
    // target the export root instead of the local mount point.
    if absolute == point {
        return None;
    }

    let session = PathMapper::new(mount.config.local_path()).ok()?;
    let dummy_path = session.map(path, ResolvePath::NoFollow).ok()?;
    Some((mount, dummy_path))
}

thread_local! {
    static REMOVE_SESSION: std::cell::RefCell<Option<(String, vnfs::nfs::mount::NfsMount, Session)>> =
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
            .is_none_or(|(mode, config, _)| mode != &implementation || config != &mount.config)
        {
            let backend = match implementation.as_str() {
                "dummy" => Session::Dummy {
                    fs: Posix::new(mount.config.local_path()).ok()?,
                    paths: PathMapper::new(mount.config.local_path()).ok()?,
                },
                _ => {
                    let session = NfsMountSession::from_discovered(mount.config.clone()).ok()?;
                    Session::Nfs {
                        session: Box::new(session),
                        supports_copy: false,
                    }
                }
            };
            *slot = Some((implementation, mount.config, backend));
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
}
