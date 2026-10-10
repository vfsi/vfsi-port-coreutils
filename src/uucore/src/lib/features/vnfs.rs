// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VNFS removal support for paths on Linux NFS mounts.

use std::path::{Component, Path, PathBuf};

use vnfs::helpers::{PathMapper, ResolvePath};
use vnfs::{Mounted, Nfs, NfsClient, RemoveMode, RemoveOptions, Result as VfResult, Vfsi};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mount {
    point: PathBuf,
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
    let mount = Nfs::discover_mount(&resolved).ok()?;
    Some(NfsMount {
        server: mount.host().to_owned(),
        export: mount.export_root().to_path_buf(),
        point: mount.mount_point().to_path_buf(),
    })
}

enum Backend {
    Dummy(Mounted),
    Nfs(NfsClient),
}

impl Backend {
    fn remove(&self, path: &Path, recursive: bool) -> VfResult<()> {
        match self {
            Self::Dummy(fs) => fs.vremove(
                &[path],
                if recursive {
                    RemoveMode::Tree
                } else {
                    RemoveMode::Entry
                },
                RemoveOptions::default(),
            ),
            Self::Nfs(fs) => fs.vremove(
                &[path],
                if recursive {
                    RemoveMode::Tree
                } else {
                    RemoveMode::Entry
                },
                RemoveOptions::default(),
            ),
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
    let mount = Mount {
        point: info.mount_point().to_path_buf(),
    };
    let absolute = parent.join(name);

    // Removing an export's mount point through its server-side path would
    // target the export root instead of the local mount point.
    if absolute == mount.point {
        return None;
    }

    let session = PathMapper::new(&mount.point).ok()?;
    let dummy_path = session.map(path, ResolvePath::NoFollow).ok()?;
    Some((mount, dummy_path))
}

/// Try to remove `path` through VNFS.
///
/// `None` means no operation was submitted and the kernel path may be used.
/// Once submitted, return its result even on failure: a direct recursive
/// removal can have partial effects and must not be replayed via the kernel.
pub fn try_remove(path: &Path, recursive: bool) -> Option<std::io::Result<()>> {
    let (mount, relative_path) = remove_paths(path)?;

    let backend = match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => {
            let Ok(client) = Mounted::new(&mount.point) else {
                return None;
            };
            Backend::Dummy(client)
        }
        Ok("nfs") => {
            let Ok(client) = Nfs::from_mount(&mount.point) else {
                return None;
            };
            Backend::Nfs(client)
        }
        _ => return None,
    };

    Some(
        backend
            .remove(&relative_path, recursive)
            .map_err(Into::into),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let fs = Mounted::new(root.path()).unwrap();

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
        let fs = Mounted::new(root.path()).unwrap();

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
        let fs = Mounted::new(root.path()).unwrap();

        assert!(
            fs.vremove(&["/dir"], RemoveMode::Entry, RemoveOptions::default())
                .is_err()
        );
        assert!(dir.exists());
    }
}
