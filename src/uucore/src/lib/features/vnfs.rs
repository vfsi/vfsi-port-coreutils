// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VNFS removal support for paths on Linux NFS mounts.

use std::path::{Component, Path, PathBuf};

use vnfs::{Mounted, Nfs, NfsClient, Result as VfResult};

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
            Self::Dummy(fs) => fs.remove_paths(&[path], recursive),
            Self::Nfs(fs) => fs.remove_paths(&[path], recursive),
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

    let relative = absolute.strip_prefix(&mount.point).ok()?;
    let dummy_path = Path::new("/").join(relative);
    Some((mount, dummy_path))
}

/// Try to remove `path` through VNFS.
///
/// Returns `true` only when an enabled backend removed the path. Unsupported
/// paths, connection failures, and filesystem errors return `false`, allowing
/// the caller to preserve its normal platform-specific behavior as a fallback.
pub fn try_remove(path: &Path, recursive: bool) -> bool {
    let Some((mount, relative_path)) = remove_paths(path) else {
        return false;
    };

    let backend = match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => {
            let Ok(client) = Mounted::new(&mount.point) else {
                return false;
            };
            Backend::Dummy(client)
        }
        Ok("nfs") => {
            let Ok(client) = Nfs::from_mount(&mount.point) else {
                return false;
            };
            Backend::Nfs(client)
        }
        _ => return false,
    };

    backend.remove(&relative_path, recursive).is_ok()
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

        fs.remove_paths(&["/empty"], false).unwrap();

        assert!(!empty.exists());
    }

    #[test]
    fn removes_tree_recursively() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join("child")).unwrap();
        std::fs::write(tree.join("child/file"), b"data").unwrap();
        let fs = Mounted::new(root.path()).unwrap();

        fs.remove_paths(&["/tree"], true).unwrap();

        assert!(!tree.exists());
    }

    #[test]
    fn non_recursive_remove_keeps_nonempty_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), b"data").unwrap();
        let fs = Mounted::new(root.path()).unwrap();

        assert!(fs.remove_paths(&["/dir"], false).is_err());
        assert!(dir.exists());
    }
}
