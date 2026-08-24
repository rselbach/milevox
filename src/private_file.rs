use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("private file path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let (temporary_path, mut temporary) = create_temporary(path)?;

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
        drop(temporary);
        fs::rename(&temporary_path, path)
            .with_context(|| format!("failed to replace private file at {}", path.display()))?;
        secure(path)?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub fn open_append(path: &Path) -> Result<File> {
    let parent = path.parent().context("private file path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open private file at {}", path.display()))?;
    secure_file(&file, path)?;
    Ok(file)
}

pub fn secure(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open private file at {}", path.display()))?;
    secure_file(&file, path)
}

fn create_temporary(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path.parent().context("private file path has no parent")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("private file name is not valid UTF-8")?;
    for _ in 0..100 {
        let nonce = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary_path) {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create private temporary file in {}",
                        parent.display()
                    )
                });
            }
        }
    }
    bail!(
        "could not create a unique private temporary file in {}",
        parent.display()
    )
}

fn secure_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .permissions();
        if permissions.mode() & 0o777 != 0o600 {
            permissions.set_mode(0o600);
            file.set_permissions(permissions)
                .with_context(|| format!("failed to secure {}", path.display()))?;
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open {} for syncing", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}
