use std::time::Duration;

/// A program that prints `hello pty` and exits, an interactive one that echoes its input, and the default shell.
#[cfg(unix)]
fn programs() -> ((String, Vec<String>), String, String) {
    (("/bin/echo".into(), vec!["hello pty".into()]), "/bin/cat".into(), "/bin/sh".into())
}

#[cfg(windows)]
fn programs() -> ((String, Vec<String>), String, String) {
    (("cmd.exe".into(), vec!["/c".into(), "echo hello pty".into()]), "cmd.exe".into(), "cmd.exe".into())
}

/// ConPTY asks the terminal where its cursor is (`ESC [ 6 n`) and starts the program once a terminal answers, as the
/// TUI's and the app's terminals do; here the test is the terminal.
const CURSOR_REPORT: &str = "\x1b[1;1R";

fn answer_cursor_request(ptys: &oc_core::pty::Ptys, id: &str) {
    if cfg!(windows) {
        ptys.write(id, CURSOR_REPORT);
    }
}

#[tokio::test]
async fn pty_runs_and_replays() {
    let db = oc_storage::Database::memory().unwrap();
    let bus = oc_core::bus::Bus::new(db);
    let ptys = oc_core::pty::Ptys::new(bus);
    let ((echo, echo_args), cat, shell) = programs();
    let cwd = std::env::temp_dir().display().to_string();
    let info = ptys
        .create(oc_core::pty::CreateInput { command: Some(echo), args: Some(echo_args), ..Default::default() }, &cwd, &shell)
        .unwrap();
    assert!(info.title.starts_with("Terminal "));
    answer_cursor_request(&ptys, &info.id);
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if ptys.get(&info.id).unwrap().status == "exited" {
            break;
        }
    }
    let got = ptys.get(&info.id).unwrap();
    assert_eq!(got.status, "exited");
    assert_eq!(got.exit_code, Some(0));
    // exited sessions refuse attach
    assert!(matches!(ptys.attach(&info.id, None), Err(oc_core::pty::PtyError::Exited(_))));

    let live = ptys.create(oc_core::pty::CreateInput { command: Some(cat), ..Default::default() }, &cwd, &shell).unwrap();
    let mut att = ptys.attach(&live.id, Some(-1)).unwrap();
    att.activate();
    if cfg!(windows) {
        att.write(CURSOR_REPORT);
    }
    att.write(if cfg!(windows) { "ping\r\n" } else { "ping\n" });
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !seen.contains("ping") && tokio::time::Instant::now() < deadline {
        if let Ok(Some(oc_core::pty::Chunk::Data(d))) = tokio::time::timeout(Duration::from_millis(200), att.rx.recv()).await {
            seen.push_str(&d);
        }
    }
    assert!(seen.contains("ping"), "{seen:?}");
    let again = ptys.attach(&live.id, None).unwrap();
    assert!(again.replay.contains("ping"));
    assert_eq!(again.cursor, again.replay.encode_utf16().count() as u64);
    ptys.remove(&live.id).unwrap();
}
