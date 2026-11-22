// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Shared VFSI operations for Linux NFS mounts.

use std::path::{Path, PathBuf};

use vnfs::Result as VfResult;
use vnfs::directory::{Attributes, DirectoryListing, ListDirOptions};
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
/// snapshot for connection validation and later session reuse. Discovering
/// the root also rejects a direct backend when the kernel has a nested mount
/// anywhere below that root.
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
            Self::Dummy { paths: mapper, .. } => mapper
                .map_many(paths, policy)
                .map_err(vnfs::error::IndexedError::into_cause),
            Self::Nfs { session, .. } => session
                .map_many(paths, policy)
                .map_err(vnfs::error::IndexedError::into_cause),
        }
    }
    pub fn attrs_nofollow(
        &self,
        paths: &[PathBuf],
        fields: Attributes,
    ) -> VfResult<Vec<vnfs::directory::Attrs>> {
        let options = vnfs::directory::AttrsOptions::new()
            .with_fields(fields)
            .with_follow_final_symlink(false);
        match self {
            Self::Dummy { fs, .. } => fs
                .getattrs_all(paths, options)
                .map_err(vnfs::error::IndexedError::into_cause),
            Self::Nfs { session, .. } => session
                .fs()
                .getattrs_all(paths, options)
                .map_err(vnfs::error::IndexedError::into_cause),
        }
    }
    pub fn read_dirs(
        &self,
        dirs: &[&Path],
        fields: Attributes,
    ) -> vnfs::VecResult<DirectoryListing> {
        let options = ListDirOptions::new().with_fields(fields);
        let mut listings: Vec<_> = dirs
            .iter()
            .map(|path| DirectoryListing {
                path: path.to_path_buf(),
                entries: Vec::new(),
            })
            .collect();
        let mut collect = |index: usize, page: DirectoryListing| {
            let listing = listings.get_mut(index).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid directory root index",
                )
            })?;
            if page.path != listing.path {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid shallow directory path",
                ));
            }
            for entry in &page.entries {
                entry.attrs().require(fields - Attributes::NAMED_ATTR)?;
            }
            listing.entries.extend(page.entries);
            Ok(std::ops::ControlFlow::Continue(()))
        };
        let completion = match self {
            Self::Dummy { fs, .. } => fs.vlistdirs(dirs, options, &mut collect),
            Self::Nfs { session, .. } => session.fs().vlistdirs(dirs, options, &mut collect),
        }?;
        if completion.len() != dirs.len() {
            return Err(std::io::Error::other("invalid directory outcome count"));
        }
        Ok(completion
            .into_iter()
            .zip(listings)
            .map(|(outcome, listing)| match outcome {
                Ok(vnfs::directory::TraversalCompletion::Complete) => Ok(listing),
                Ok(_) => Err(vnfs::error::OpError::failed(
                    std::io::Error::other("incomplete shallow directory collection"),
                    None,
                )),
                Err(error) => Err(error),
            })
            .collect())
    }

    pub fn read_files(&self, paths: &[PathBuf], limit: usize) -> VfResult<Vec<Vec<u8>>> {
        let limit = std::num::NonZeroUsize::new(limit)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let options = ReadOptions::new().with_max_total_bytes(limit);
        match self {
            Self::Dummy { fs, .. } => fs
                .read_files_with_options(paths, options)
                .map_err(vnfs::error::IndexedError::into_cause),
            Self::Nfs { session, .. } => session
                .fs()
                .read_files_with_options(paths, options)
                .map_err(vnfs::error::IndexedError::into_cause),
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
    /// Copy into caller-admitted writers; destination policy stays with the utility.
    pub fn copy_to_writers<W: std::io::Write>(
        &self,
        paths: &[PathBuf],
        writers: &mut [W],
        options: vnfs::helpers::WriterCopyOptions,
    ) -> VfResult<Vec<u64>> {
        match self {
            Self::Dummy { fs, .. } => vnfs::helpers::copy_to_writers(fs, paths, writers, options)
                .map_err(vnfs::error::IndexedError::into_cause),
            Self::Nfs { session, .. } => {
                vnfs::helpers::copy_to_writers(session.fs(), paths, writers, options)
                    .map_err(vnfs::error::IndexedError::into_cause)
            }
        }
    }
    /// None means server COPY was not submitted and another method may be used.
    /// An outer rejection means no COPY input was dispatched. Inner failures
    /// retain each input's certainty; Failed/Unknown do not authorize replay.
    pub fn try_server_copy(&self, pairs: &[(PathBuf, PathBuf)]) -> Option<vnfs::VecResult<()>> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shallow_collection_preserves_empty_and_duplicate_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        std::fs::create_dir(root.path().join("full")).unwrap();
        std::fs::write(root.path().join("full/file"), b"data").unwrap();
        let backend = Session::Dummy {
            fs: Posix::new(root.path()).unwrap(),
            paths: PathMapper::new(root.path()).unwrap(),
        };
        let dirs = [
            Path::new("/full"),
            Path::new("/missing"),
            Path::new("/empty"),
            Path::new("/full"),
        ];
        let listings = backend.read_dirs(&dirs, Attributes::MODE).unwrap();
        assert_eq!(listings.len(), dirs.len());
        for index in [0, 2, 3] {
            assert_eq!(listings[index].as_ref().unwrap().path, dirs[index]);
        }
        assert_eq!(
            listings[1].as_ref().unwrap_err().cause().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(listings[2].as_ref().unwrap().entries.is_empty());
        let first = &listings[0].as_ref().unwrap().entries;
        let duplicate = &listings[3].as_ref().unwrap().entries;
        assert_eq!(first.len(), 1);
        assert_eq!(duplicate.len(), 1);
        assert_eq!(first[0].path(), duplicate[0].path());
        assert!(backend.read_dirs(&[], Attributes::MODE).unwrap().is_empty());
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
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
        assert!(
            copy_capabilities.get().is_none(),
            "listing must not query COPY capabilities"
        );
        if let Some(result) = backend.try_server_copy(&[]) {
            result.unwrap();
        }
        assert!(copy_capabilities.get().is_some());
        if let Some(result) = backend.try_server_copy(&[]) {
            result.unwrap();
        }
    }
}
