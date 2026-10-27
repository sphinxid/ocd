//! The update notice on the home screen and the `/update` dialog (`update.rs`).

use super::tests::{app, render};
use super::update::{UpdateCheck, UpdateNotice, UpdateState, UpdaterHooks, dialog_text};
use super::*;
use crate::keymap::Stroke;
use std::sync::{Arc, Mutex};

struct Fake {
    tx: tokio::sync::watch::Sender<Option<UpdateNotice>>,
    applied: Arc<Mutex<Vec<String>>>,
}

fn hooks(check: anyhow::Result<Option<UpdateCheck>>, apply_ok: bool) -> (UpdaterHooks, Fake) {
    let (tx, notices) = tokio::sync::watch::channel(None);
    let applied = Arc::new(Mutex::new(vec![]));
    let check = Arc::new(Mutex::new(Some(check)));
    let seen = applied.clone();
    let hooks = UpdaterHooks {
        notices,
        check: Arc::new(move || {
            let r = check.lock().unwrap().take().unwrap_or(Ok(None));
            Box::pin(async move { r })
        }),
        apply: Arc::new(move |v| {
            seen.lock().unwrap().push(v);
            Box::pin(async move { if apply_ok { Ok(()) } else { anyhow::bail!("Cannot replace /usr/bin/ocd") } })
        }),
    };
    (hooks, Fake { tx, applied })
}

fn app_with(h: Option<UpdaterHooks>) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.opts.updater = h;
    (a, rx)
}

/// Applies queued app messages until `done` holds (or 5 s pass).
async fn settle(a: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, done: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(a) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(AppMsg::Apply(f))) => f(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
}

fn top_text(a: &App) -> String {
    // Unwrapped: the dialog wraps at its width.
    a.dialogs.last().map(dialog_text).unwrap_or_default().replace('\n', " ")
}

fn top_title(a: &App) -> String {
    match a.dialogs.last().map(|d| &d.body) {
        Some(crate::ui::dialog::Body::Text(t)) => t.title.clone(),
        _ => String::new(),
    }
}

#[test]
fn without_an_updater_there_is_no_command_or_notice() {
    let (mut a, _rx) = app_with(None);
    a.update_notify(UpdateNotice::Available("0.2.0".into()));
    assert!(a.update.notification.is_none());
    assert!(!a.commands().iter().any(|c| c.id == "opencode.update"));
    assert!(!render(&mut a, 100, 30).iter().any(|l| l.contains("/update")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_background_notice_shows_under_the_logo_and_skip_remembers_it() {
    let (h, fake) = hooks(Ok(None), true);
    let (mut a, mut rx) = app_with(Some(h));
    assert!(a.commands().iter().any(|c| c.id == "opencode.update" && c.slash == Some("update")));
    let before = render(&mut a, 100, 30);
    a.start_update_watch();
    fake.tx.send(Some(UpdateNotice::Available("0.2.0".into()))).unwrap();
    settle(&mut a, &mut rx, |a| a.update.notification.is_some()).await;
    assert_eq!(a.update.notification, Some(UpdateNotice::Available("0.2.0".into())));
    let lines = render(&mut a, 100, 30);
    let row = lines.iter().position(|l| l.contains("/update to install v0.2.0")).expect("notice row");
    let logo_bottom = lines.iter().rposition(|l| l.contains('█')).unwrap();
    assert_eq!(row, logo_bottom + 2, "one blank row under the logo:\n{}", lines.join("\n"));
    let centered = lines[row].trim_start().len();
    assert!((100 - centered) / 2 - (lines[row].len() - lines[row].trim_start().len()) <= 1, "centered: {:?}", lines[row]);
    // The prompt stays below it; the centered block grows by one row.
    let prompt = |ls: &[String]| ls.iter().position(|l| l.contains('┃')).unwrap();
    assert!(prompt(&lines) > row + 1);
    assert!(prompt(&lines) - prompt(&before) <= 1);

    // Clicking it opens the dialog with the known update; Enter on "skip" is not the last button.
    a.on_click(crate::ui::text::Click::Custom(super::update::NOTIFICATION_CLICK.into()));
    assert_eq!(top_title(&a), "Update available");
    assert_eq!(top_text(&a), "An update is available. After installing, you'll be prompted to restart ocd.");
    assert_eq!(a.update_buttons(), vec!["skip", "update"]);
    a.on_key(Stroke::key("escape"));
    assert!(a.dialogs.is_empty());
    assert!(a.update.notification.is_some(), "escape is not skip");

    // Skip from a dialog without an update dismisses the notice for good.
    a.update.state = None;
    a.update.check = Some(UpdateState::Unavailable("x".into()));
    a.open_dialog(a.update_dialog());
    a.on_key(Stroke::key("return"));
    assert!(a.dialogs.is_empty());
    assert!(a.update.notification.is_none());
    let stored = a.keyed.get("update-notifications").unwrap();
    assert_eq!(stored["versions"], serde_json::json!(["client:0.2.0"]));
    a.update_notify(UpdateNotice::Available("0.2.0".into()));
    assert!(a.update.notification.is_none(), "a dismissed version stays hidden");
    a.update_notify(UpdateNotice::Available("0.3.0".into()));
    assert!(a.update.notification.is_some(), "a newer one shows");
    assert!(fake.applied.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_checks_installs_and_offers_a_restart() {
    let (h, fake) = hooks(Ok(Some(UpdateCheck::Available("0.2.0".into()))), true);
    let (mut a, mut rx) = app_with(Some(h));
    a.run_command("opencode.update", None);
    assert_eq!(a.update_display(), UpdateState::Checking);
    assert_eq!(top_text(&a), "Checking for updates…");
    settle(&mut a, &mut rx, |a| a.update.check.is_none()).await;
    assert_eq!(top_title(&a), "Update available");
    let screen = render(&mut a, 100, 30);
    assert!(screen.iter().any(|l| l.contains("Update available") && l.contains("esc")), "{}", screen.join("\n"));
    assert!(screen.iter().any(|l| l.contains("skip  update enter")), "{}", screen.join("\n"));
    a.on_key(Stroke::key("return"));
    assert_eq!(a.update_display(), UpdateState::Installing("0.2.0".into()));
    assert_eq!(top_title(&a), "Updating ocd");
    assert_eq!(top_text(&a), "Installing ocd 0.2.0…");
    assert!(a.update_buttons().is_empty());
    settle(&mut a, &mut rx, |a| matches!(a.update.state, Some(UpdateState::Installed(_)))).await;
    assert_eq!(*fake.applied.lock().unwrap(), vec!["0.2.0".to_string()]);
    assert_eq!(top_text(&a), "Update successful! Restart closes ocd, then run ocd to use the new version.");
    assert_eq!(a.update_buttons(), vec!["skip", "restart"]);
    // The exit epilogue names the new version.
    let epilogue = a.exit_epilogue().unwrap();
    assert!(epilogue.contains("Run \x1b[0m\x1b[1mocd\x1b[0m\x1b[90m to use \x1b[0m\x1b[1m0.2.0"), "{epilogue:?}");
    assert!(!epilogue.contains("Session"));
    a.on_key(Stroke::key("return"));
    assert!(a.quit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_reports_current_unavailable_and_failures() {
    let (h, _) = hooks(Ok(None), true);
    let (mut a, mut rx) = app_with(Some(h));
    a.open_update(false);
    settle(&mut a, &mut rx, |a| a.update.check.is_none()).await;
    assert_eq!((top_title(&a), top_text(&a)), ("Update".to_string(), "ocd is already up to date.".to_string()));
    assert_eq!(a.update_buttons(), vec!["skip"]);

    let (h, _) = hooks(Ok(Some(UpdateCheck::Unavailable("This ocd is a local build.".into()))), true);
    let (mut a, mut rx) = app_with(Some(h));
    a.open_update(false);
    settle(&mut a, &mut rx, |a| !matches!(a.update.check, Some(UpdateState::Checking))).await;
    assert_eq!(top_text(&a), "This ocd is a local build.");

    let (h, _) = hooks(Err(anyhow::anyhow!("GitHub API rate limit reached")), true);
    let (mut a, mut rx) = app_with(Some(h));
    a.open_update(false);
    settle(&mut a, &mut rx, |a| !matches!(a.update.check, Some(UpdateState::Checking))).await;
    assert_eq!(a.update_display(), UpdateState::CheckFailed("GitHub API rate limit reached".into()));

    // A failed install keeps the dialog open with the error; opening it again checks again.
    let (h, fake) = hooks(Ok(Some(UpdateCheck::Available("0.2.0".into()))), false);
    let (mut a, mut rx) = app_with(Some(h));
    a.update_notify(UpdateNotice::Available("0.2.0".into()));
    a.open_update(true);
    assert!(a.update.check.is_none(), "a known update needs no lookup");
    a.on_key(Stroke::key("return"));
    settle(&mut a, &mut rx, |a| matches!(a.update.state, Some(UpdateState::Failed(_)))).await;
    assert_eq!(top_title(&a), "Update available");
    assert_eq!(top_text(&a), "Cannot replace /usr/bin/ocd");
    assert_eq!(fake.applied.lock().unwrap().len(), 1);
    assert!(a.update.updated_version.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_automatic_install_says_restart() {
    let (h, fake) = hooks(Ok(None), true);
    let (mut a, mut rx) = app_with(Some(h));
    a.start_update_watch();
    fake.tx.send(Some(UpdateNotice::Installed("0.2.0".into()))).unwrap();
    settle(&mut a, &mut rx, |a| a.update.notification.is_some()).await;
    assert!(render(&mut a, 100, 30).iter().any(|l| l.contains("/exit restart to use v0.2.0")));
    assert_eq!(a.update.updated_version.as_deref(), Some("0.2.0"));
    a.on_click(crate::ui::text::Click::Custom(super::update::NOTIFICATION_CLICK.into()));
    assert!(a.quit);
}
