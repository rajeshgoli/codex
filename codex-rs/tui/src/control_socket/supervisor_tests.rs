use super::*;
use crate::app_event::AppEvent;
use serde_json::Value;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::time::Instant;
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

fn start_handle(path: &Path) -> (ControlSocketHandle, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    (
        ControlSocketHandle::start(path.to_path_buf(), AppEventSender::new(tx)).unwrap(),
        rx,
    )
}

fn request(path: &Path, body: &[u8]) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(body)?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    let value: Value = serde_json::from_str(&response)?;
    if value["ok"] != true {
        return Err(std::io::Error::other(format!(
            "control request failed: {response}"
        )));
    }
    Ok(value)
}

fn get_epoch(path: &Path) -> std::io::Result<String> {
    let value = request(
        path,
        b"{\"request_id\":\"test-epoch\",\"command\":\"get_epoch\"}\n",
    )?;
    Ok(value["epoch"].as_str().unwrap().to_string())
}

fn wait_for_epoch(path: &Path, previous: Option<&str>) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(epoch) = get_epoch(path)
            && previous.is_none_or(|previous| previous != epoch)
        {
            return epoch;
        }
        assert!(Instant::now() < deadline, "control socket did not recover");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn missing_socket_path_is_rebound_with_a_fresh_epoch() {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path().join("control.sock");
    let (mut handle, mut rx) = start_handle(&path);
    let first_epoch = wait_for_epoch(&path, None);

    fs::remove_file(&path).unwrap();

    let second_epoch = wait_for_epoch(&path, Some(&first_epoch));
    assert_ne!(first_epoch, second_epoch);
    request(
        &path,
        b"{\"request_id\":\"test-submit\",\"command\":\"submit_message\",\"message\":\"after recovery\",\"thread_id\":null}\n",
    )
    .unwrap();
    let event = rx.try_recv().unwrap();
    assert!(matches!(
        event,
        AppEvent::SubmitExternalLiteralUserMessage { text } if text == "after recovery"
    ));
    handle.shutdown();
    assert!(!path.exists());
}

#[test]
fn cleanup_does_not_remove_a_replacement_socket() {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path().join("control.sock");
    let (mut handle, _rx) = start_handle(&path);
    wait_for_epoch(&path, None);

    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    handle.shutdown();

    assert!(path.exists());
    drop(replacement);
    fs::remove_file(path).unwrap();
}
