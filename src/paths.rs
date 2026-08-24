use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn config_path() -> PathBuf {
    config_home().join("milevox/config.toml")
}

pub fn credentials_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("credentials.toml")
}

pub fn runtime_dir() -> PathBuf {
    if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(value).join("milevox");
    }

    env::temp_dir().join(format!("milevox-{}", current_uid()))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("milevox.sock")
}

pub fn prepare_runtime_dir() -> Result<PathBuf> {
    let path = runtime_dir();
    secure_runtime_dir(&path)?;
    Ok(path)
}

fn secure_runtime_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect runtime path {}", path.display()));
        }
    }
    // Inspect again after creation so an existing path and a path raced into place are held
    // to the same type and ownership checks.
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect runtime path {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("runtime path is a symlink: {}", path.display());
    }
    if !metadata.is_dir() {
        bail!("runtime path is not a directory: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != current_uid() {
            bail!("runtime path is owned by another user: {}", path.display());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure runtime path {}", path.display()))?;
    }
    Ok(())
}

pub fn default_model_path() -> PathBuf {
    data_home().join("milevox/models/parakeet-tdt-0.6b-v2-int8")
}

pub fn debug_log_path() -> PathBuf {
    state_home().join("milevox/debug.log")
}

fn config_home() -> PathBuf {
    if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(value);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
}

fn data_home() -> PathBuf {
    if let Some(value) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(value);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share")
}

fn state_home() -> PathBuf {
    if let Some(value) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(value);
    }

    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state")
}

#[cfg(unix)]
pub fn current_uid() -> u32 {
    std::os::unix::fs::MetadataExt::uid(
        &std::fs::metadata("/proc/self")
            .unwrap_or_else(|_| std::fs::metadata(".").expect("the current directory must exist")),
    )
}

#[cfg(not(unix))]
pub fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn fallback_runtime_directory_is_private_even_under_a_permissive_umask() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("milevox-runtime-test-{}-{id}", std::process::id()));
        secure_runtime_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_runtime_path() {
        use std::os::unix::fs::symlink;

        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "milevox-runtime-link-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&parent).unwrap();
        let target = parent.join("target");
        let link = parent.join("link");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        assert!(
            secure_runtime_dir(&link)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        fs::remove_dir_all(parent).unwrap();
    }
}
