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
use tokio::sync::mpsc::error::TryRecvError;
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
fn duplicate_request_is_not_redispatched_after_rebind() {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path().join("control.sock");
    let (mut handle, mut rx) = start_handle(&path);
    let first_epoch = wait_for_epoch(&path, None);
    let request_id = "submit-across-rebind";
    let first_request = format!(
        "{{\"request_id\":\"{request_id}\",\"expected_epoch\":\"{first_epoch}\",\"command\":\"submit_message\",\"message\":\"only once\",\"thread_id\":null}}\n"
    );

    request(&path, first_request.as_bytes()).unwrap();
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::SubmitExternalLiteralUserMessage { text }) if text == "only once"
    ));

    fs::remove_file(&path).unwrap();
    let second_epoch = wait_for_epoch(&path, Some(&first_epoch));
    let retry = format!(
        "{{\"request_id\":\"{request_id}\",\"expected_epoch\":\"{second_epoch}\",\"command\":\"submit_message\",\"message\":\"only once\",\"thread_id\":null}}\n"
    );
    let response = request(&path, retry.as_bytes()).unwrap();

    assert_eq!(response["epoch"], second_epoch);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    handle.shutdown();
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

#[test]
fn stale_replacement_socket_is_reaped_before_rebind() {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path().join("control.sock");
    let (mut handle, _rx) = start_handle(&path);
    let first_epoch = wait_for_epoch(&path, None);

    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    drop(replacement);

    let second_epoch = wait_for_epoch(&path, Some(&first_epoch));
    assert_ne!(first_epoch, second_epoch);
    handle.shutdown();
    assert!(!path.exists());
}

#[test]
fn old_generation_connections_are_closed_before_rebind() {
    let tempdir = TempDir::new().unwrap();
    let path = tempdir.path().join("control.sock");
    let (mut handle, _rx) = start_handle(&path);
    let first_epoch = wait_for_epoch(&path, None);

    let mut stale_stream = UnixStream::connect(&path).unwrap();
    stale_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut stale_reader = BufReader::new(stale_stream.try_clone().unwrap());
    stale_stream
        .write_all(b"{\"request_id\":\"old-generation\",\"command\":\"get_epoch\"}\n")
        .unwrap();
    let mut initial_response = String::new();
    stale_reader.read_line(&mut initial_response).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&initial_response).unwrap()["epoch"],
        first_epoch
    );

    fs::remove_file(&path).unwrap();
    let second_epoch = wait_for_epoch(&path, Some(&first_epoch));
    assert_ne!(first_epoch, second_epoch);

    let write_result =
        stale_stream.write_all(b"{\"request_id\":\"after-rebind\",\"command\":\"get_epoch\"}\n");
    if write_result.is_ok() {
        let mut stale_response = String::new();
        match stale_reader.read_line(&mut stale_response) {
            Ok(0) | Err(_) => {}
            Ok(_) => panic!("retired connection returned a response: {stale_response}"),
        }
    }

    assert_eq!(get_epoch(&path).unwrap(), second_epoch);
    handle.shutdown();
}
