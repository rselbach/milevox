use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::config::{OutputConfig, OutputMode};
use crate::ipc::{DeliveryMethod, Notice};

const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct DeliveryResult {
    pub method: DeliveryMethod,
    pub notices: Vec<Notice>,
}

pub async fn deliver(config: &OutputConfig, text: &str) -> Result<DeliveryResult> {
    if text.trim().is_empty() {
        return Ok(DeliveryResult {
            method: DeliveryMethod::None,
            notices: Vec::new(),
        });
    }

    match config.mode {
        OutputMode::Clipboard => {
            copy_to_clipboard(text).await?;
            Ok(DeliveryResult {
                method: DeliveryMethod::Clipboard,
                notices: Vec::new(),
            })
        }
        OutputMode::Type => match type_text(text).await {
            Ok(()) => Ok(DeliveryResult {
                method: DeliveryMethod::Typed,
                notices: Vec::new(),
            }),
            Err(typing_error) if config.clipboard_fallback => {
                copy_to_clipboard(text).await.context(format!(
                    "typing failed ({typing_error:#}); clipboard fallback also failed"
                ))?;
                Ok(DeliveryResult {
                    method: DeliveryMethod::ClipboardFallback,
                    notices: vec![
                        Notice::warning(
                            "clipboard_fallback",
                            "Typing failed; transcript copied to the clipboard",
                        )
                        .with_detail(format!("{typing_error:#}")),
                    ],
                })
            }
            Err(error) => Err(error),
        },
    }
}

async fn type_text(text: &str) -> Result<()> {
    let child = Command::new("wtype")
        .arg("--")
        .arg(text)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start wtype")?;
    let output = tokio::time::timeout(OUTPUT_TIMEOUT, child.wait_with_output())
        .await
        .context("wtype timed out")?
        .context("failed to wait for wtype")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("wtype exited with {}: {stderr}", output.status)
}

async fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start wl-copy")?;
    let mut stdin = child.stdin.take().context("wl-copy stdin is unavailable")?;
    tokio::time::timeout(OUTPUT_TIMEOUT, stdin.write_all(text.as_bytes()))
        .await
        .context("wl-copy input timed out")?
        .context("failed to write transcript to wl-copy")?;
    drop(stdin);

    let status = wait_for_output(&mut child, "wl-copy").await?;
    if !status.success() {
        bail!("wl-copy exited with {status}");
    }
    Ok(())
}

async fn wait_for_output(child: &mut Child, name: &str) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(OUTPUT_TIMEOUT, child.wait()).await {
        Ok(result) => result.with_context(|| format!("failed to wait for {name}")),
        Err(_) => {
            child
                .kill()
                .await
                .with_context(|| format!("failed to stop timed-out {name}"))?;
            let _ = child.wait().await;
            bail!("{name} timed out")
        }
    }
}
