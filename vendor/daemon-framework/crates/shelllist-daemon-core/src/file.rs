//! Atomic single-file persistence. Multi-file transactions and recovery are caller policy.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
pub struct AtomicFilePolicy {
    /// Modes apply to the application-owned parent and newly created directories.
    /// None preserves default/umask permissions and does not chmod existing parents.
    pub directory_mode: Option<u32>,
    pub file_mode: Option<u32>,
    pub sync_parent: bool,
    pub sync_new_ancestors: bool,
    pub reject_symlink_destination: bool,
}
impl AtomicFilePolicy {
    pub const PRIVATE: Self = Self {
        directory_mode: Some(0o700),
        file_mode: Some(0o600),
        sync_parent: true,
        sync_new_ancestors: true,
        reject_symlink_destination: true,
    };
    pub const DURABLE: Self = Self {
        directory_mode: None,
        file_mode: None,
        ..Self::PRIVATE
    };
}

/// A complete, synced temporary file. Drop removes only the temporary file owned
/// by this object. A successful commit is the configured durability barrier.
/// An error syncing the parent can occur *after* replacement; callers needing
/// rollback must retain the previous contents themselves.
pub struct StagedFile {
    destination: PathBuf,
    temporary: Option<PathBuf>,
    policy: AtomicFilePolicy,
}

impl StagedFile {
    pub fn new(path: &Path, contents: &[u8], policy: AtomicFilePolicy) -> io::Result<Self> {
        let parent = parent_directory(path)?;
        let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        create_directory(parent, policy)?;
        if policy.reject_symlink_destination {
            reject_symlink(path)?;
        }
        let (mut output, temporary) = loop {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".{}-{}.tmp",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let temporary = parent.join(temporary_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            if let Some(mode) = policy.file_mode {
                options.mode(mode);
            }
            match options.open(&temporary) {
                Ok(file) => break (file, temporary),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let staged = Self {
            destination: path.into(),
            temporary: Some(temporary),
            policy,
        };
        output.write_all(contents)?;
        if let Some(mode) = policy.file_mode {
            output.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        output.sync_all()?;
        Ok(staged)
    }

    pub fn commit(&mut self) -> io::Result<()> {
        let temporary = self.temporary.as_ref().ok_or(io::ErrorKind::InvalidInput)?;
        if self.policy.reject_symlink_destination {
            reject_symlink(&self.destination)?;
        }
        fs::rename(temporary, &self.destination)?;
        self.temporary = None;
        if self.policy.sync_parent {
            sync_parent(&self.destination)?;
        }
        Ok(())
    }
}
impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(path) = &self.temporary {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn write_bytes_atomic(
    path: &Path,
    contents: &[u8],
    policy: AtomicFilePolicy,
) -> io::Result<()> {
    StagedFile::new(path, contents, policy)?.commit()
}

pub fn parent_directory(path: &Path) -> io::Result<&Path> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    Ok(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
}
pub fn sync_parent(path: &Path) -> io::Result<()> {
    sync_directory(parent_directory(path)?)
}
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn create_directory(path: &Path, policy: AtomicFilePolicy) -> io::Result<()> {
    let missing = path
        .ancestors()
        .take_while(|path| !path.as_os_str().is_empty() && !path.exists())
        .collect::<Vec<_>>();
    if policy.directory_mode.is_some() {
        reject_symlink(path)?;
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    if let Some(mode) = policy.directory_mode {
        builder.mode(mode);
    }
    builder.create(path)?;
    if let Some(mode) = policy.directory_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    if policy.sync_new_ancestors {
        for directory in missing.into_iter().rev() {
            sync_directory(directory)?;
            sync_parent(directory)?;
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing symlinked state path",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Opens without following the final symlink or blocking on a FIFO, requires a
/// regular file, and enforces the bound again while reading (including growth).
pub fn read_bytes_bounded(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    use rustix::fs::{Mode, OFlags};
    let file = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state is not a regular file",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state exceeds read limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state grew beyond read limit",
        ));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "framework-files-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn staged_abort_commit_and_failed_replacement_clean_up() {
        let root = Directory::new();
        let path = root.0.join("new/state/data");
        write_bytes_atomic(&path, b"old", AtomicFilePolicy::PRIVATE).unwrap();
        let staged = StagedFile::new(&path, b"not committed", AtomicFilePolicy::PRIVATE).unwrap();
        drop(staged);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        write_bytes_atomic(&path, b"new", AtomicFilePolicy::PRIVATE).unwrap();
        assert_eq!(read_bytes_bounded(&path, 3).unwrap().unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        let target = root.0.join("directory");
        fs::create_dir(&target).unwrap();
        assert!(write_bytes_atomic(&target, b"bad", AtomicFilePolicy::DURABLE).is_err());
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
        assert_eq!(
            parent_directory(Path::new("relative.json")).unwrap(),
            Path::new(".")
        );
    }

    #[test]
    fn bounded_reads_reject_large_nonregular_and_symlinked_files() {
        let root = Directory::new();
        let path = root.0.join("data");
        assert!(read_bytes_bounded(&path, 2).unwrap().is_none());
        fs::write(&path, b"large").unwrap();
        assert!(read_bytes_bounded(&path, 2).is_err());
        assert!(read_bytes_bounded(&root.0, 100).is_err());
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_bytes_bounded(&link, 100).is_err());
        assert!(write_bytes_atomic(&link, b"overwrite", AtomicFilePolicy::PRIVATE).is_err());
        assert_eq!(fs::read(path).unwrap(), b"large");
    }
}
