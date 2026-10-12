use oc_core::instructions::{environment_source, Read, Source};
use oc_core::{bus::Bus, instruction_state, session::{CreateInput, Sessions}};
use oc_schema::types::LocationRef;
use oc_schema::Message;
use serde_json::Value;

fn date(v: &str) -> Source {
    Source::new(
        "core/date",
        Read::Value(Value::String(v.into())),
        |v| Some(format!("Today's date: {}", v.as_str()?)),
        |_, v| Some(format!("Today's date is now: {}", v.as_str()?)),
        None,
    )
}

#[tokio::test]
async fn initial_then_update() {
    let db = oc_storage::Database::memory().unwrap();
    let bus = Bus::new(db.clone());
    let sessions = Sessions::new(bus.clone(), "test");
    let dir = std::env::temp_dir().join(format!("oc-instr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let s = sessions
        .create(CreateInput {
            location: Some(LocationRef { directory: dir.display().to_string(), workspace_id: None }),
            ..Default::default()
        })
        .await
        .unwrap();
    let env = || environment_source("/w", "/w", false, "/tmp/opencode");
    instruction_state::prepare(&bus, &[date("Mon Oct 12 2026"), env()], &s.id).unwrap();
    let base = instruction_state::initial(&bus, &s.id, &[date("x"), env()]).unwrap();
    assert!(base.starts_with("Today's date: Mon Oct 12 2026\n\nHere is some useful information"));
    // unchanged → no event
    instruction_state::prepare(&bus, &[date("Mon Oct 12 2026"), env()], &s.id).unwrap();
    instruction_state::prepare(&bus, &[date("Tue Oct 13 2026"), env()], &s.id).unwrap();
    let msgs = db.session_messages(&s.id).unwrap();
    let systems: Vec<_> = msgs
        .iter()
        .filter_map(|(_, m)| match m {
            Message::System { text, description, .. } => Some((text.clone(), description.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(systems, vec![("Today's date is now: Tue Oct 13 2026".to_string(), Some("Instructions updated: core/date".to_string()))]);
    // the epoch baseline is unchanged within the epoch
    let base2 = instruction_state::initial(&bus, &s.id, &[date("x"), env()]).unwrap();
    assert_eq!(base, base2);
    std::fs::remove_dir_all(&dir).ok();
}
