//! Instruction epochs (`core/session/instruction-state.ts`): prepare / initial / preview.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};

use oc_schema::event::SessionEvent;

use crate::bus::{Bus, Durable1};
use crate::instructions::{self, Source, REMOVED};

struct Stored {
    initial: Map<String, Value>,
    current: Map<String, Value>,
}

fn find(bus: &Bus, session: &str) -> anyhow::Result<Option<Stored>> {
    let row: Option<(String, String)> = bus.db().with(|c| {
        c.query_row(
            "SELECT initial_values, current_values FROM instruction_state WHERE session_id = ?",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })?;
    Ok(row.map(|(i, c)| Stored {
        initial: serde_json::from_str(&i).unwrap_or_default(),
        current: serde_json::from_str(&c).unwrap_or_default(),
    }))
}

fn load_blobs(bus: &Bus, hashes: &[String]) -> anyhow::Result<HashMap<String, Value>> {
    let mut out = HashMap::new();
    for h in hashes {
        if out.contains_key(h) {
            continue;
        }
        let v: Option<Option<String>> =
            bus.db().with(|c| c.query_row("SELECT value FROM instruction_blob WHERE hash = ?", [h], |r| r.get(0)).optional())?;
        let Some(v) = v else { anyhow::bail!("Instruction blob not found: {h}") };
        out.insert(h.clone(), v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null));
    }
    Ok(out)
}

fn deref(values: &Map<String, Value>, blobs: &HashMap<String, Value>) -> anyhow::Result<Map<String, Value>> {
    values
        .iter()
        .map(|(k, h)| {
            let h = h.as_str().unwrap_or_default();
            blobs.get(h).cloned().map(|v| (k.clone(), v)).ok_or_else(|| anyhow::anyhow!("Instruction blob not found: {h}"))
        })
        .collect()
}

fn apply_delta(previous: &Map<String, Value>, delta: &Map<String, Value>) -> Map<String, Value> {
    let mut out = previous.clone();
    for (k, v) in delta {
        if v.as_str() == Some(REMOVED) {
            out.shift_remove(k);
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn deref_delta(delta: &Map<String, Value>, blobs: &HashMap<String, Value>) -> Vec<(String, Option<Value>)> {
    delta
        .iter()
        .map(|(k, h)| {
            let h = h.as_str().unwrap_or_default();
            (k.clone(), if h == REMOVED { None } else { blobs.get(h).cloned() })
        })
        .collect()
}

/// Observe sources and commit `session.instructions.updated` when initial or changed.
pub fn prepare(bus: &Bus, sources: &[Source], session: &str) -> anyhow::Result<()> {
    let stored = find(bus, session)?;
    let admission = instructions::diff(sources, stored.as_ref().map(|s| &s.current)).map_err(|m| anyhow::anyhow!(m))?;
    let initial = stored.is_none();
    if !initial && admission.delta.is_empty() {
        return Ok(());
    }
    let text = if initial {
        String::new()
    } else {
        let previous = &stored.as_ref().unwrap().current;
        let replaced: Vec<String> = previous
            .iter()
            .filter(|(k, _)| admission.delta.contains_key(*k))
            .filter_map(|(_, h)| h.as_str().map(str::to_string))
            .collect();
        let old = load_blobs(bus, &replaced)?;
        let prev_values: Map<String, Value> = previous
            .iter()
            .filter(|(k, _)| admission.delta.contains_key(*k))
            .filter_map(|(k, h)| old.get(h.as_str()?).map(|v| (k.clone(), v.clone())))
            .collect();
        let admitted: HashMap<String, Value> = admission.blobs.iter().cloned().collect();
        instructions::render_update(sources, &prev_values, &deref_delta(&admission.delta, &admitted))
    };
    let blobs = admission.blobs.clone();
    let metadata = initial.then(|| json!({"instructions": {"initial": true}}).as_object().cloned().unwrap());
    bus.commit_with(
        vec![Durable1 {
            event: SessionEvent::InstructionsUpdated {
                session_id: session.into(),
                delta: admission.delta,
                text: (!text.is_empty()).then_some(text),
            },
            metadata,
            id: None,
        }],
        move |c, _, _| {
            for (hash, value) in &blobs {
                c.execute(
                    "INSERT INTO instruction_blob (hash, value) VALUES (?, ?) ON CONFLICT DO NOTHING",
                    params![hash, value.to_string()],
                )?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

/// The epoch baseline rendered at the start of every request.
pub fn initial(bus: &Bus, session: &str, sources: &[Source]) -> anyhow::Result<String> {
    let Some(state) = find(bus, session)? else {
        anyhow::bail!("Instruction state not found during assembly: {session}");
    };
    let hashes: Vec<String> = state.initial.values().filter_map(|h| h.as_str().map(str::to_string)).collect();
    let blobs = load_blobs(bus, &hashes)?;
    Ok(instructions::render_initial(sources, &deref(&state.initial, &blobs)?))
}

pub fn current(bus: &Bus, session: &str) -> anyhow::Result<Option<Map<String, Value>>> {
    Ok(find(bus, session)?.map(|s| s.current))
}

/// `preview`: baseline + pending update text without publishing.
pub fn preview(bus: &Bus, session: &str, sources: &[Source]) -> anyhow::Result<(String, String)> {
    let state = find(bus, session)?;
    let admission = instructions::diff(sources, state.as_ref().map(|s| &s.current)).map_err(|m| anyhow::anyhow!(m))?;
    let observed: HashMap<String, Value> = admission.blobs.iter().cloned().collect();
    let Some(state) = state else {
        let current = apply_delta(&Map::new(), &admission.delta);
        return Ok((instructions::render_initial(sources, &deref(&current, &observed)?), String::new()));
    };
    let mut hashes: Vec<String> = state.initial.values().chain(state.current.values()).filter_map(|h| h.as_str().map(str::to_string)).collect();
    hashes.dedup();
    let stored = load_blobs(bus, &hashes)?;
    Ok((
        instructions::render_initial(sources, &deref(&state.initial, &stored)?),
        instructions::render_update(sources, &deref(&state.current, &stored)?, &deref_delta(&admission.delta, &observed)),
    ))
}
