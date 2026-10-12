use std::time::Duration;

#[tokio::test]
async fn pty_runs_and_replays() {
    let db = oc_storage::Database::memory().unwrap();
    let bus = oc_core::bus::Bus::new(db);
    let ptys = oc_core::pty::Ptys::new(bus);
    let info = ptys
        .create(
            oc_core::pty::CreateInput { command: Some("/bin/echo".into()), args: Some(vec!["hello pty".into()]), ..Default::default() },
            "/tmp",
            "/bin/sh",
        )
        .unwrap();
    assert!(info.title.starts_with("Terminal "));
    for _ in 0..50 {
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

    let live = ptys
        .create(oc_core::pty::CreateInput { command: Some("/bin/cat".into()), ..Default::default() }, "/tmp", "/bin/sh")
        .unwrap();
    let mut att = ptys.attach(&live.id, Some(-1)).unwrap();
    att.activate();
    att.write("ping\n");
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
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
