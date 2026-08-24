use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn write_frame(writer: &mut impl Write, bytes: &[u8]) {
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .unwrap();
    writer.write_all(bytes).unwrap();
}

fn read_frame(reader: &mut impl Read) -> serde_json::Value {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).unwrap();
    let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
    reader.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn hidden_worker_uses_framed_metadata_and_little_endian_samples() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_milevox"))
        .args([
            "__transcription-worker",
            "--model-path",
            "/tmp/greendale-model",
            "--fake",
            "normal",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    assert_eq!(
        read_frame(&mut stdout),
        serde_json::json!({"type": "ready"})
    );
    write_frame(
        &mut stdin,
        &serde_json::to_vec(&serde_json::json!({
            "request_id": 9,
            "generation": 4,
            "sample_rate": 16_000,
            "sample_count": 2,
            "allow_empty": false
        }))
        .unwrap(),
    );
    write_frame(
        &mut stdin,
        &[0.25_f32.to_le_bytes(), (-0.5_f32).to_le_bytes()].concat(),
    );
    stdin.flush().unwrap();

    assert_eq!(
        read_frame(&mut stdout),
        serde_json::json!({
            "type": "result",
            "request_id": 9,
            "transcript": "Troy and Abed in the morning",
            "error": null
        })
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn inherited_fake_worker_environment_cannot_bypass_model_loading() {
    let missing = std::env::temp_dir().join(format!(
        "milevox-missing-worker-model-{}",
        std::process::id()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_milevox"))
        .args(["__transcription-worker", "--model-path"])
        .arg(&missing)
        .env("MILEVOX_FAKE_WORKER", "normal")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let message = read_frame(&mut child.stdout.take().unwrap());
    assert_eq!(message["type"], "load_error");
    assert!(
        message["error"]
            .as_str()
            .unwrap()
            .contains("does not exist")
    );
    assert!(child.wait().unwrap().success());
}
