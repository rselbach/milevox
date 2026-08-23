use std::env;
use std::path::{Path, PathBuf};

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
fn current_uid() -> u32 {
    std::os::unix::fs::MetadataExt::uid(
        &std::fs::metadata("/proc/self")
            .unwrap_or_else(|_| std::fs::metadata(".").expect("the current directory must exist")),
    )
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}
