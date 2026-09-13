// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Opt-in VNFS removal support for paths on Linux NFS mounts.

use std::path::{Component, Path, PathBuf};

use ::vnfs::{DummyVecFs, NfsVecFs, VecFs, VfRes};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mount {
    server: String,
    export: PathBuf,
    point: PathBuf,
}

enum Backend {
    Dummy(DummyVecFs),
    Nfs(Box<NfsVecFs>),
}

impl Backend {
    fn remove(&mut self, path: &Path, recursive: bool) -> VfRes {
        match self {
            Self::Dummy(fs) => remove_with_fs(fs, path, recursive),
            Self::Nfs(fs) => remove_with_fs(fs.as_mut(), path, recursive),
        }
    }
}

fn remove_with_fs(fs: &mut impl VecFs, path: &Path, recursive: bool) -> VfRes {
    fs.rm(&[path], recursive)
}

fn find_mount(path: &Path, mounts: &str) -> Option<Mount> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let spec = fields.next()?;
            let point = PathBuf::from(fields.next()?);
            let fstype = fields.next()?;
            if (fstype != "nfs" && fstype != "nfs4") || !path.starts_with(&point) {
                return None;
            }
            let (server, export) = spec.rsplit_once(':')?;
            Some(Mount {
                server: server.trim_matches(['[', ']']).to_owned(),
                export: PathBuf::from(export),
                point,
            })
        })
        .max_by_key(|mount| mount.point.as_os_str().len())
}

fn remove_paths(path: &Path) -> Option<(Mount, PathBuf, PathBuf)> {
    let Component::Normal(name) = path.components().next_back()? else {
        return None;
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let parent = parent.canonicalize().ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mount = find_mount(&parent, &mounts)?;
    let absolute = parent.join(name);

    // Removing an export's mount point through its server-side path would
    // target the export root instead of the local mount point.
    if absolute == mount.point {
        return None;
    }

    let relative = absolute.strip_prefix(&mount.point).ok()?;
    let dummy_path = Path::new("/").join(relative);
    let nfs_path = mount.export.join(relative);
    Some((mount, dummy_path, nfs_path))
}

/// Try to remove `path` through VNFS.
///
/// Returns `true` only when an enabled backend removed the path. Unsupported
/// paths, connection failures, and filesystem errors return `false`, allowing
/// the caller to preserve its normal platform-specific behavior as a fallback.
pub fn try_remove(path: &Path, recursive: bool) -> bool {
    let Some((mount, dummy_path, nfs_path)) = remove_paths(path) else {
        return false;
    };

    let (mut backend, path) = match std::env::var("VNFS_IMPL").as_deref() {
        Ok("dummy") => (Backend::Dummy(DummyVecFs::new(mount.point)), dummy_path),
        Ok("nfs") => {
            let Ok(fs) = NfsVecFs::connect(&mount.server) else {
                return false;
            };
            (Backend::Nfs(Box::new(fs)), nfs_path)
        }
        _ => return false,
    };

    backend.remove(&path, recursive).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_longest_nfs_mount() {
        let mounts = "server:/ /mnt nfs4 rw 0 0\nserver:/nested /mnt/nested nfs rw 0 0\n";
        let mount = find_mount(Path::new("/mnt/nested/tree"), mounts).unwrap();
        assert_eq!(mount.point, Path::new("/mnt/nested"));
        assert_eq!(mount.export, Path::new("/nested"));
    }

    #[test]
    fn removes_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let mut fs = DummyVecFs::new(root.path().to_path_buf());

        remove_with_fs(&mut fs, Path::new("/empty"), false).unwrap();

        assert!(!empty.exists());
    }

    #[test]
    fn removes_tree_recursively() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join("child")).unwrap();
        std::fs::write(tree.join("child/file"), b"data").unwrap();
        let mut fs = DummyVecFs::new(root.path().to_path_buf());

        remove_with_fs(&mut fs, Path::new("/tree"), true).unwrap();

        assert!(!tree.exists());
    }

    #[test]
    fn non_recursive_remove_keeps_nonempty_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file"), b"data").unwrap();
        let mut fs = DummyVecFs::new(root.path().to_path_buf());

        assert!(remove_with_fs(&mut fs, Path::new("/dir"), false).is_err());
        assert!(dir.exists());
    }
}
