use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{OutputConfig, OutputMode};

pub async fn deliver(config: &OutputConfig, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }

    match config.mode {
        OutputMode::Clipboard => copy_to_clipboard(text).await,
        OutputMode::Type => match type_text(text).await {
            Ok(()) => Ok(()),
            Err(error) if config.clipboard_fallback => copy_to_clipboard(text).await.context(
                format!("typing failed ({error:#}); clipboard fallback also failed"),
            ),
            Err(error) => Err(error),
        },
    }
}

async fn type_text(text: &str) -> Result<()> {
    let output = Command::new("wtype")
        .arg("--")
        .arg(text)
        .output()
        .await
        .context("failed to run wtype")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("wtype exited with {}: {stderr}", output.status)
}

async fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run wl-copy")?;
    let mut stdin = child.stdin.take().context("wl-copy stdin is unavailable")?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);

    let status = child.wait().await?;
    if !status.success() {
        bail!("wl-copy exited with {status}");
    }
    Ok(())
}
