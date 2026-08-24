use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use crate::paths;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = open_parent(path)?;
    atomic_write_at(&parent, path, contents)
}

fn atomic_write_at(parent: &ParentDirectory, path: &Path, contents: &[u8]) -> Result<()> {
    let name = file_name(path)?;
    verify_existing(parent, &name, path)?;
    let (temporary_name, mut temporary) = create_temporary(parent, path)?;
    let temporary_path = parent.path.join(&temporary_name);

    let result = (|| -> Result<()> {
        temporary.write_all(contents).with_context(|| {
            format!(
                "failed to write private file at {}",
                temporary_path.display()
            )
        })?;
        temporary.sync_all().with_context(|| {
            format!(
                "failed to sync private file at {}",
                temporary_path.display()
            )
        })?;
        rename_at(&parent.file, &temporary_name, &parent.file, &name)
            .with_context(|| format!("failed to replace private file at {}", path.display()))?;
        drop(temporary);
        parent.sync()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = unlink_at(&parent.file, &temporary_name);
    }
    result
}

pub fn open_append(path: &Path) -> Result<File> {
    let parent = open_parent(path)?;
    let name = file_name(path)?;
    let file = open_at(
        &parent.file,
        &name,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )
    .with_context(|| format!("failed to open private file at {}", path.display()))?;
    secure_file(&file, path)?;
    Ok(file)
}

pub fn open_lock(path: &Path) -> Result<File> {
    let parent = open_parent(path)?;
    let name = file_name(path)?;
    let file = open_at(
        &parent.file,
        &name,
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )
    .with_context(|| format!("failed to open private lock at {}", path.display()))?;
    secure_file(&file, path)?;
    Ok(file)
}

pub fn read_to_string(path: &Path) -> Result<Option<String>> {
    let Some(mut file) = open_for_read(path)? else {
        return Ok(None);
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read private file at {}", path.display()))?;
    Ok(Some(contents))
}

pub fn read_to_string_bounded(path: &Path, max_bytes: usize) -> Result<Option<String>> {
    let Some(file) = open_for_read(path)? else {
        return Ok(None);
    };
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read private file at {}", path.display()))?;
    if bytes.len() > max_bytes {
        bail!(
            "private file exceeds the {max_bytes}-byte limit: {}",
            path.display()
        );
    }
    let contents = String::from_utf8(bytes)
        .with_context(|| format!("private file is not valid UTF-8: {}", path.display()))?;
    Ok(Some(contents))
}

fn open_for_read(path: &Path) -> Result<Option<File>> {
    let parent = open_parent(path)?;
    let name = file_name(path)?;
    open_existing(&parent, &name, path)
}

pub fn rotate(path: &Path, backup: &Path) -> Result<()> {
    let source_parent = parent_path(path)?;
    if source_parent != parent_path(backup)? {
        bail!("private-file rotation requires paths in the same directory");
    }

    let parent = open_parent(path)?;
    let name = file_name(path)?;
    let backup_name = file_name(backup)?;
    let source = open_existing(&parent, &name, path)?
        .with_context(|| format!("private file is missing: {}", path.display()))?;
    let previous_backup = open_existing(&parent, &backup_name, backup)?;
    rename_at(&parent.file, &name, &parent.file, &backup_name)
        .with_context(|| format!("failed to rotate private file at {}", path.display()))?;
    drop(source);
    drop(previous_backup);
    parent.sync()
}

pub fn remove(path: &Path) -> Result<()> {
    let parent = open_parent(path)?;
    let name = file_name(path)?;
    let Some(file) = open_existing(&parent, &name, path)? else {
        return Ok(());
    };
    unlink_at(&parent.file, &name)
        .with_context(|| format!("failed to remove private file at {}", path.display()))?;
    drop(file);
    parent.sync()
}

pub fn cap(path: &Path, max_bytes: u64) -> Result<()> {
    let parent = open_parent(path)?;
    let name = file_name(path)?;
    let file = match open_at(
        &parent.file,
        &name,
        libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open private file at {}", path.display()));
        }
    };
    secure_file(&file, path)?;
    if file
        .metadata()
        .with_context(|| format!("failed to inspect private file at {}", path.display()))?
        .len()
        > max_bytes
    {
        file.set_len(max_bytes)
            .with_context(|| format!("failed to cap private file at {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync private file at {}", path.display()))?;
        parent.sync()?;
    }
    Ok(())
}

struct ParentDirectory {
    file: File,
    path: PathBuf,
}

impl ParentDirectory {
    fn sync(&self) -> Result<()> {
        self.file
            .sync_all()
            .with_context(|| format!("failed to sync {}", self.path.display()))
    }
}

fn create_temporary(parent: &ParentDirectory, path: &Path) -> Result<(OsString, File)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("private file name is not valid UTF-8")?;
    for _ in 0..100 {
        let nonce = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary_name = OsString::from(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
        match open_at(
            &parent.file,
            &temporary_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => {
                let temporary_path = parent.path.join(&temporary_name);
                secure_file(&file, &temporary_path)?;
                return Ok((temporary_name, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create private temporary file in {}",
                        parent.path.display()
                    )
                });
            }
        }
    }
    bail!(
        "could not create a unique private temporary file in {}",
        parent.path.display()
    )
}

fn open_existing(parent: &ParentDirectory, name: &OsStr, path: &Path) -> Result<Option<File>> {
    let file = match open_at(
        &parent.file,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open private file at {}", path.display()));
        }
    };
    secure_file(&file, path)?;
    Ok(Some(file))
}

fn verify_existing(parent: &ParentDirectory, name: &OsStr, path: &Path) -> Result<()> {
    open_existing(parent, name, path).map(|_| ())
}

fn open_parent(path: &Path) -> Result<ParentDirectory> {
    let parent = parent_path(path)?;
    prepare_parent(&parent, paths::is_app_private_dir(&parent))
}

fn parent_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("private file path has no parent")?;
    if parent.as_os_str().is_empty() {
        return Ok(PathBuf::from("."));
    }
    Ok(parent.to_path_buf())
}

fn file_name(path: &Path) -> Result<OsString> {
    path.file_name()
        .map(OsStr::to_owned)
        .context("private file path has no file name")
}

fn prepare_parent(parent: &Path, app_managed: bool) -> Result<ParentDirectory> {
    let file = match open_directory(parent) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && app_managed => {
            create_app_directory(parent)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("private file parent does not exist: {}", parent.display());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to open private file parent {}", parent.display())
            });
        }
    };
    secure_parent(&file, parent, app_managed)?;
    Ok(ParentDirectory {
        file,
        path: parent.to_path_buf(),
    })
}

fn open_directory(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    options
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn create_app_directory(path: &Path) -> Result<File> {
    let anchor = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut current = open_directory(anchor)
        .with_context(|| format!("failed to open directory anchor for {}", path.display()))?;

    for component in path.components() {
        let Component::Normal(name) = component else {
            match component {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir | Component::Prefix(_) => {
                    bail!(
                        "app private directory contains an unsafe component: {}",
                        path.display()
                    );
                }
                Component::Normal(_) => unreachable!(),
            }
        };
        let next = match open_directory_at(&current, name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match mkdir_at(&current, name, 0o700) {
                    Ok(()) => current.sync_all().with_context(|| {
                        format!("failed to sync a parent of {}", path.display())
                    })?,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create app private directory {}", path.display())
                        });
                    }
                }
                open_directory_at(&current, name).with_context(|| {
                    format!("failed to open app private directory {}", path.display())
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to open app private directory {}", path.display())
                });
            }
        };
        current = next;
    }
    Ok(current)
}

fn open_directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

fn secure_parent(file: &File, path: &Path, app_managed: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect private file parent {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("private file parent is not a directory: {}", path.display());
    }
    verify_owner(metadata.uid(), path, "private file parent")?;
    if app_managed && metadata.mode() & 0o777 != 0o700 {
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", path.display()))?;
    } else if !app_managed && metadata.mode() & 0o022 != 0 {
        bail!(
            "private file parent is writable by other users: {}",
            path.display()
        );
    }

    let secured = file
        .metadata()
        .with_context(|| format!("failed to verify private file parent {}", path.display()))?;
    if !secured.is_dir() || secured.uid() != effective_uid() {
        bail!(
            "private file parent changed while opening: {}",
            path.display()
        );
    }
    if app_managed && secured.mode() & 0o777 != 0o700 {
        bail!("app private directory mode is not 0700: {}", path.display());
    }
    if !app_managed && secured.mode() & 0o022 != 0 {
        bail!(
            "private file parent is writable by other users: {}",
            path.display()
        );
    }
    Ok(())
}

fn secure_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !metadata.is_file() {
            bail!("private path is not a regular file: {}", path.display());
        }
        verify_owner(metadata.uid(), path, "private file")?;
        if metadata.nlink() != 1 {
            bail!("private file has multiple hard links: {}", path.display());
        }
        if metadata.mode() & 0o777 != 0o600 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)
                .with_context(|| format!("failed to secure {}", path.display()))?;
        }
        let secured = file
            .metadata()
            .with_context(|| format!("failed to verify {}", path.display()))?;
        if secured.mode() & 0o777 != 0o600 {
            bail!("private file mode is not 0600: {}", path.display());
        }
    }
    Ok(())
}

fn open_at(
    parent: &File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let name = c_string(name)?;
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn mkdir_at(parent: &File, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    let name = c_string(name)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_at(
    old_parent: &File,
    old_name: &OsStr,
    new_parent: &File,
    new_name: &OsStr,
) -> io::Result<()> {
    let old_name = c_string(old_name)?;
    let new_name = c_string(new_name)?;
    let result = unsafe {
        libc::renameat(
            old_parent.as_raw_fd(),
            old_name.as_ptr(),
            new_parent.as_raw_fd(),
            new_name.as_ptr(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = c_string(name)?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn verify_owner(uid: u32, path: &Path, kind: &str) -> Result<()> {
    if uid != effective_uid() {
        bail!("{kind} is owned by another user: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "milevox-private-{name}-{}-{id}",
            std::process::id()
        ))
    }

    fn create_test_directory(name: &str) -> PathBuf {
        let directory = test_directory(name);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn atomic_writes_create_private_files() {
        let directory = create_test_directory("atomic");
        let path = directory.join("credentials.toml");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(read_to_string(&path).unwrap().as_deref(), Some("second"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn creates_app_managed_directories_with_user_only_permissions() {
        let directory = test_directory("app-parent");

        let parent = prepare_parent(&directory, true).unwrap();

        assert_eq!(
            parent.file.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(parent);
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn rejects_missing_custom_parents() {
        let directory = test_directory("missing-parent");

        let error = atomic_write(&directory.join("config.toml"), b"enabled = false").unwrap_err();

        assert!(error.to_string().contains("parent does not exist"));
        assert!(!directory.exists());
    }

    #[test]
    fn bounded_reads_accept_the_exact_limit_and_reject_one_byte_more() {
        let directory = create_test_directory("bounded-read");
        let path = directory.join("credentials.toml");
        atomic_write(&path, b"TroyBarnes").unwrap();

        assert_eq!(
            read_to_string_bounded(&path, 10).unwrap().as_deref(),
            Some("TroyBarnes")
        );
        let error = read_to_string_bounded(&path, 9).unwrap_err();
        assert!(error.to_string().contains("9-byte limit"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_final_component_symlinks_directories_and_hard_links() {
        let directory = create_test_directory("unsafe-final");
        let target = directory.join("target");
        fs::write(&target, "Troy").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.join("link");
        symlink(&target, &link).unwrap();
        assert!(read_to_string(&link).is_err());
        assert!(atomic_write(&link, b"Abed").is_err());

        let child_directory = directory.join("child");
        fs::create_dir(&child_directory).unwrap();
        assert!(read_to_string(&child_directory).is_err());

        let hard_link_path = directory.join("hard-link");
        fs::hard_link(&target, &hard_link_path).unwrap();
        assert!(read_to_string(&target).is_err());
        assert!(read_to_string(&hard_link_path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_parent_symlinks_without_touching_the_target() {
        let directory = create_test_directory("parent-symlink");
        let target = directory.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = directory.join("link");
        symlink(&target, &link).unwrap();
        let path = link.join("credentials.toml");

        assert!(atomic_write(&path, b"Greendale").is_err());
        assert!(read_to_string(&path).is_err());
        assert!(!target.join("credentials.toml").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn holds_the_verified_parent_across_path_replacement() {
        let directory = create_test_directory("parent-swap");
        let moved = directory.with_extension("moved");
        let path = directory.join("config.toml");
        let parent = open_parent(&path).unwrap();
        fs::rename(&directory, &moved).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();

        atomic_write_at(&parent, &path, b"Troy and Abed").unwrap();

        assert_eq!(
            fs::read(moved.join("config.toml")).unwrap(),
            b"Troy and Abed"
        );
        assert!(!directory.join("config.toml").exists());
        drop(parent);
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn rejects_writable_custom_parents() {
        let directory = create_test_directory("writable-parent");
        let path = directory.join("config.toml");
        fs::write(&path, "enabled = false").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();

        let error = atomic_write(&path, b"enabled = true").unwrap_err();

        assert!(error.to_string().contains("writable by other users"));
        assert!(read_to_string(&path).is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn private_descriptors_are_closed_on_exec() {
        let directory = create_test_directory("cloexec");
        let path = directory.join("debug.log");
        let parent = open_parent(&path).unwrap();
        let file = open_append(&path).unwrap();

        for descriptor in [parent.file.as_raw_fd(), file.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        drop(parent);
        drop(file);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rotates_and_removes_files_through_the_verified_parent() {
        let directory = create_test_directory("rotation");
        let path = directory.join("debug.log");
        let backup = directory.join("debug.log.1");
        atomic_write(&path, b"new").unwrap();
        atomic_write(&backup, b"old").unwrap();

        rotate(&path, &backup).unwrap();

        assert!(!path.exists());
        assert_eq!(read_to_string(&backup).unwrap().as_deref(), Some("new"));
        remove(&backup).unwrap();
        remove(&backup).unwrap();
        assert!(!backup.exists());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn rejects_rotation_across_parent_directories() {
        let first = create_test_directory("rotation-first");
        let second = create_test_directory("rotation-second");
        let path = first.join("debug.log");
        atomic_write(&path, b"Greendale").unwrap();

        let error = rotate(&path, &second.join("debug.log.1")).unwrap_err();

        assert!(error.to_string().contains("same directory"));
        assert_eq!(read_to_string(&path).unwrap().as_deref(), Some("Greendale"));
        fs::remove_dir_all(first).unwrap();
        fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn rejects_foreign_ownership_metadata() {
        let path = Path::new("/tmp/greendale-private-file");
        let foreign_uid = effective_uid().wrapping_add(1);

        let error = verify_owner(foreign_uid, path, "private file").unwrap_err();

        assert!(error.to_string().contains("owned by another user"));
    }
}
