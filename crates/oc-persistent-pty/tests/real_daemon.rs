//! End-to-end test against the real `opencode-pty` daemon (ignored by default: needs the
//! binary, downloaded from npm unless `OPENCODE_PTY_BIN` is set).
//!
//! Run: `cargo test -p oc-persistent-pty --test real_daemon -- --ignored --nocapture`

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oc_persistent_pty::{
    AttachInput, BinaryFn, BinaryResolver, BusEvent, CreateInput, Options, PersistentPty, Role, StreamEvent,
};

fn global(root: &std::path::Path) -> oc_util::Global {
    let mut g = oc_util::Global::resolve();
    g.state = root.join("state");
    g.bin = root.join("bin");
    g.cache = root.join("cache");
    g
}

async fn binary(root: &std::path::Path) -> String {
    if let Ok(path) = std::env::var("OPENCODE_PTY_BIN") {
        return path;
    }
    // Share one download across runs.
    let bin = std::env::temp_dir().join("oc-persistent-pty-test-bin");
    let resolver = BinaryResolver::new(bin, reqwest::Client::new());
    let package = oc_persistent_pty::binary::current_package().expect("supported platform");
    let path = resolver.install(package).await.expect("install opencode-pty");
    let _ = root;
    path.to_string_lossy().into_owned()
}

fn fixed(path: String) -> BinaryFn {
    Arc::new(move || {
        let path = path.clone();
        Box::pin(async move { Ok(path) })
    })
}

async fn wait_output(stream: &mut oc_persistent_pty::EventStream, needle: &str, seen: &mut Vec<StreamEvent>) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !text.contains(needle) {
        let event = tokio::time::timeout_at(deadline, stream.recv()).await.expect("timed out waiting for output");
        let Some(event) = event else { panic!("stream ended; output so far: {text:?}") };
        if let StreamEvent::Output { data, .. } = &event {
            text.push_str(&String::from_utf8_lossy(data));
        }
        seen.push(event);
    }
    text
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_daemon_create_attach_handoff_shutdown() {
    let root = std::env::temp_dir().join(format!("ocpty-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    std::fs::create_dir_all(&root).unwrap();
    let exe = binary(&root).await;
    let g = global(&root);
    let options = Options { binary: Some(fixed(exe.clone())), ..Default::default() };
    let a = PersistentPty::new(&g, options.clone()).await;
    let mut bus = a.subscribe_events();

    // No daemon yet: list is empty and nothing is spawned.
    assert!(a.list(None).await.unwrap().is_empty());
    assert!(!a.daemon().is_owner());

    let session = "ses_test";
    let input = CreateInput {
        command: Some("/bin/sh".into()),
        args: vec![],
        cwd: Some("/tmp".into()),
        title: "Terminal".into(),
        env: [("PS1".to_string(), "$ ".to_string())].into(),
        size: None,
    };
    let info = a.create(session, input).await.expect("create");
    assert!(info.id.starts_with("pty_persistent_"));
    assert_eq!(info.session_id, session);
    assert_eq!((info.size.cols, info.size.rows), (80, 24));
    assert_eq!(info.command, "/bin/sh");
    assert!(a.daemon().is_owner());
    match bus.recv().await.unwrap() {
        BusEvent::Added { session_id, terminal } => {
            assert_eq!(session_id, session);
            assert_eq!(terminal.id, info.id);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(a.list(Some(session)).await.unwrap().len(), 1);
    assert!(a.list(Some("ses_other")).await.unwrap().is_empty());
    assert_eq!(a.get(&info.id).await.unwrap().id, info.id);
    assert!(matches!(a.get("pty_persistent_999").await, Err(oc_persistent_pty::Error::NotFound(_))));

    // Attach as controller with replay from 0, then drive input.
    let mut attachment = a
        .attach(&info.id, AttachInput { cursor: 0, attachment_id: "att-1".into(), role: Role::Controller, takeover: true })
        .await
        .expect("attach");
    assert_eq!(attachment.role, Role::Controller);
    let attached = oc_persistent_pty::ws::attached_message("att-1", true, &attachment);
    assert_eq!(attached["type"], "attached");
    assert_eq!(attached["info"]["id"], info.id.as_str());
    let mut stream = attachment.activate().expect("activate");
    assert!(attachment.activate().is_none());
    a.input(&info.id, "att-1", 80, 24, b"echo hello-$((40+2))\n").await.expect("input");
    let mut seen = Vec::new();
    let text = wait_output(&mut stream, "hello-42", &mut seen).await;
    eprintln!("output: {text:?}");

    // read() uses the controller-selected terminal.
    // Omitted `lines` = live height; an explicit count returns the last N physical rows.
    let read = a.read(session, None).await.unwrap().expect("current terminal");
    assert_eq!(a.read(session, Some(3)).await.unwrap().unwrap().screen.text.split('\n').count(), 3);
    assert_eq!(read.pty_id, info.id);
    assert!(read.screen.text.contains("hello-42"), "{:?}", read.screen.text);
    assert!(a.read("ses_other", None).await.unwrap().is_none());
    assert!(matches!(a.read(session, Some(0)).await, Err(oc_persistent_pty::Error::Unavailable(m)) if m == "lines must be an integer between 1 and 65535"));

    let snapshot = a.snapshot(&info.id).await.unwrap();
    assert!(snapshot.text.contains("hello-42"));
    assert!(!snapshot.checkpoint.is_empty());

    // Resize → resized event on the stream.
    a.resize(&info.id, 100, 30, Some("att-1")).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout_at(deadline, stream.recv()).await.expect("resized").expect("stream open");
        if let StreamEvent::Resized { cols, rows, .. } = event {
            assert_eq!((cols, rows), (100, 30));
            break;
        }
    }
    assert_eq!(a.get(&info.id).await.unwrap().size.cols, 100);

    // Legacy write path.
    a.write(&info.id, b"echo via-write\n", Some("att-1")).await.unwrap();
    wait_output(&mut stream, "via-write", &mut seen).await;
    attachment.detach();

    // Restart handoff: B adopts A's daemon with the ticket.
    let handoff = a.handoff().await.unwrap().expect("handoff");
    assert_eq!(PathBuf::from(&handoff.directory), a.daemon().directory());
    let json = handoff.to_json();
    let inherited = oc_persistent_pty::handoff::parse_inherited(Some(&json)).unwrap();
    let b = PersistentPty::new(&g, Options { handoff: Some(inherited), ..options.clone() }).await;
    assert!(b.daemon().is_owner(), "B adopted the daemon");
    assert_eq!(b.daemon().id(), a.daemon().id());
    a.close();
    drop(a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let listed = b.list(None).await.unwrap();
    assert_eq!(listed.len(), 1, "terminal survived the handoff");

    // Replay after handoff from offset 0 still includes earlier output.
    let mut observer = b
        .attach(&info.id, AttachInput { cursor: 0, attachment_id: "att-2".into(), role: Role::Observer, takeover: false })
        .await
        .unwrap();
    assert_eq!(observer.role, Role::Observer);
    assert!(String::from_utf8_lossy(&observer.replay.data).contains("hello-42"));
    observer.detach();
    assert!(observer.activate().is_none());

    // remove → persistent-pty.removed.
    let mut bus_b = b.subscribe_events();
    b.remove(&info.id).await.unwrap();
    assert_eq!(
        bus_b.recv().await.unwrap(),
        BusEvent::Removed { session_id: session.into(), pty_id: info.id.clone() }
    );
    assert!(b.list(None).await.unwrap().is_empty());

    // An observed exit removes the terminal automatically.
    let exiting = b
        .create(
            session,
            CreateInput {
                command: Some("/bin/sh".into()),
                args: vec!["-c".into(), "sleep 0.5; exit 3".into()],
                cwd: None,
                title: "Exit".into(),
                env: Default::default(),
                size: Some(oc_persistent_pty::Size { cols: 40, rows: 10 }),
            },
        )
        .await
        .unwrap();
    let mut watch = b
        .attach(&exiting.id, AttachInput { cursor: 0, attachment_id: "att-3".into(), role: Role::Observer, takeover: false })
        .await
        .unwrap();
    let mut events = watch.activate().unwrap();
    let mut exit = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("exit") {
        if let StreamEvent::Exited { exit_code, .. } = event {
            exit = Some(exit_code);
        }
    }
    assert_eq!(exit, Some(Some(3)));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !b.list(None).await.unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "exited terminal was not removed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Explicit shutdown stops the daemon.
    b.shutdown().await.unwrap();
    assert!(b.list(None).await.unwrap().is_empty());
    assert!(b.daemon().discover(None).await.is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_daemon_recovers_after_crash_and_stops_with_owner() {
    let root = std::env::temp_dir().join(format!("ocpty-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    std::fs::create_dir_all(&root).unwrap();
    let exe = binary(&root).await;
    let g = global(&root);
    let options = Options { binary: Some(fixed(exe)), ..Default::default() };
    let pty = PersistentPty::new(&g, options.clone()).await;
    let input = || CreateInput {
        command: Some("/bin/sh".into()),
        args: vec![],
        cwd: None,
        title: "Terminal".into(),
        env: Default::default(),
        size: None,
    };
    pty.create("ses", input()).await.unwrap();
    let registration: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pty.daemon().directory().join("service.json")).unwrap()).unwrap();
    let pid = registration["pid"].as_i64().unwrap();
    // SAFETY: killing the daemon we spawned.
    unsafe { libc_kill(pid as i32, 9) };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pty.daemon().is_owner() {
        assert!(tokio::time::Instant::now() < deadline, "owner loss not noticed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pty.list(None).await.unwrap().is_empty(), "no daemon → empty list");
    // create respawns a daemon.
    pty.create("ses", input()).await.unwrap();
    assert_eq!(pty.list(None).await.unwrap().len(), 1);

    // Dropping the service without a handoff disconnects the owner, which stops the daemon.
    let directory = pty.daemon().directory();
    drop(pty);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while directory.join("service.json").exists() {
        assert!(tokio::time::Instant::now() < deadline, "daemon did not stop with its owner");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_dir_all(&root);
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}
