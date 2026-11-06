//! Undo, revert and queued undo give the prompt back (2.2.5, the on-by-default part of #60116):
//! `prompt/restore.ts` (`usePromptRestore`), `component/prompt/draft-stash.ts` (`appendToSessionPrompt`),
//! `prompt/history.tsx` (`appendPrompt`), and the `session.undo`, Revert (`dialog-message.tsx`) and
//! `queued_prompt.undo` handlers of `routes/session/index.tsx`.
//!
//! The whole projected prompt comes back (text, files, agents, skills; pasted ranges are dropped), after
//! anything already in the session's composer, or in its stashed draft when another session is routed. Stored
//! media references (`experimental.media_blob`) are not ported, so restoring never reads anything.

use super::{App, Toast};
use crate::api::types::Message;
use crate::state::PromptInfo;
use serde_json::{Value, json};

/// `measureEditText`: the composer offset just after `text` (graphemes by display width, a newline counts 1).
pub fn measure_edit_text(text: &str) -> usize {
    crate::util::locale::graphemes(text).into_iter().map(|g| if g == "\n" { 1 } else { crate::util::locale::width(g) }).sum()
}

fn shifted(items: &[Value], by: f64) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut item = item.clone();
            if by != 0.0
                && let Some(m) = item.get_mut("mention").filter(|m| m.is_object())
            {
                for k in ["start", "end"] {
                    if let Some(n) = m.get(k).and_then(|n| n.as_f64()) {
                        m[k] = json!(n + by);
                    }
                }
            }
            item
        })
        .collect()
}

/// `appendPrompt`: `following` after `prompt` and a blank line (`"\n\n"`, only when `prompt` has text). The new
/// prompt's mentions and pasted ranges move by the width of what precedes them; the existing ones keep theirs.
pub fn append_prompt(prompt: &PromptInfo, following: &PromptInfo) -> PromptInfo {
    let text = if prompt.text.is_empty() { String::new() } else { format!("{}\n\n", prompt.text) };
    let offset = measure_edit_text(&text) as f64;
    let join = |a: &[Value], b: &[Value]| -> Vec<Value> {
        let mut out = shifted(a, 0.0);
        out.extend(shifted(b, offset));
        out
    };
    let mut pasted = prompt.pasted.clone();
    pasted.extend(following.pasted.iter().map(|p| {
        let mut p = p.clone();
        p.source.start += offset;
        p.source.end += offset;
        p
    }));
    PromptInfo {
        text: text + &following.text,
        files: join(&prompt.files, &following.files),
        agents: join(&prompt.agents, &following.agents),
        skills: Some(join(prompt.skills.as_deref().unwrap_or_default(), following.skills.as_deref().unwrap_or_default())),
        pasted,
        mode: None,
    }
}

/// `usePromptRestore`: a sent prompt as the composer edits it (`projectedPromptInput`, pasted ranges dropped).
pub fn restored_prompt(m: &Message) -> PromptInfo {
    let mut p = super::go_upsell::projected_prompt(m);
    p.pasted.clear();
    p
}

impl App {
    /// `appendToSessionPrompt`: append to the session's composer when its view is the routed one (normal mode),
    /// else to its stashed draft (the cursor at the end). Returns whether it wrote to the composer.
    pub fn append_to_session_prompt(&mut self, sid: &str, prompt: &PromptInfo) -> bool {
        if self.session_id() == Some(sid) {
            let next = append_prompt(&self.editor.info(), prompt);
            self.editor.set(&next);
            self.editor.shell = false;
            self.dirty = true;
            return true;
        }
        let next = match self.drafts.remove(sid) {
            Some(draft) => append_prompt(&draft, prompt),
            None => prompt.clone(),
        };
        self.drafts.insert(sid.to_string(), next);
        false
    }

    /// `undoPending`: cancel a pending inbox prompt and give it back. Single-flight per inbox item
    /// (`runPendingAction`); `done` runs only once the prompt is back, with whether the composer took it.
    fn undo_pending(&mut self, sid: &str, inbox: &str, prompt: PromptInfo, failure: &'static str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
        if !self.pending_actions.insert(inbox.to_string()) {
            return;
        }
        let c = self.client.clone();
        let (sid, inbox) = (sid.to_string(), inbox.to_string());
        let (s2, i2) = (sid.clone(), inbox.clone());
        self.spawn(async move { c.inbox_cancel(&s2, &i2).await }, move |app, r| {
            app.pending_actions.remove(&inbox);
            match r {
                Ok(()) => {
                    let composer = app.append_to_session_prompt(&sid, &prompt);
                    done(app, composer);
                }
                Err(e) => {
                    app.toast(Toast::new("error", crate::api::error_message(&e)).title(format!("Failed to {failure} pending prompt")));
                }
            }
        });
    }

    /// `session.undo`: the last user message before the revert boundary with text. A pending delivery is
    /// cancelled; otherwise the session is interrupted, waited for and reverted to it. Then its prompt comes back.
    pub fn undo(&mut self) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let boundary = self.session().and_then(|s| s.revert.as_ref().map(|r| r.message_id.clone()));
        let list = self.store.messages(&sid);
        let end = boundary.and_then(|b| list.iter().position(|m| m.id == b)).unwrap_or(list.len());
        let target = list[..end].iter().rev().find(|m| m.kind == "user" && !m.text_str().trim().is_empty()).cloned();
        let Some(target) = target else {
            self.toast(Toast::new("error", "Nothing to undo").duration(3000));
            self.dialogs.clear();
            return;
        };
        let restored = restored_prompt(&target);
        let pending = self.store.pending.get(&sid).is_some_and(|p| p.iter().any(|i| i.kind == "user" && i.id == target.id));
        if pending {
            self.undo_pending(&sid, &target.id, restored, "delete", |_, _| {});
        } else {
            let c = self.client.clone();
            let (s2, mid) = (sid.clone(), target.id.clone());
            self.spawn(
                async move {
                    c.interrupt(&s2, false).await?;
                    c.wait(&s2).await?;
                    c.revert_stage(&s2, &mid).await
                },
                move |app, r| match r {
                    Ok(_) => {
                        app.append_to_session_prompt(&sid, &restored);
                    }
                    Err(e) => app.toast(Toast::new("error", crate::api::error_message(&e)).duration(5000)),
                },
            );
        }
        self.dialogs.clear();
    }

    /// Revert (`DialogMessage`): stage the revert first; the composer changes only once it is staged, so a failed
    /// revert leaves the draft alone.
    pub fn revert_message(&mut self, sid: &str, mid: &str) {
        let restored = self.store.messages(sid).iter().find(|m| m.id == mid && m.kind == "user").map(restored_prompt);
        let c = self.client.clone();
        let (sid, mid) = (sid.to_string(), mid.to_string());
        let s2 = sid.clone();
        self.spawn(async move { c.revert_stage(&s2, &mid).await }, move |app, r| match r {
            Ok(_) => {
                if let Some(p) = restored {
                    app.append_to_session_prompt(&sid, &p);
                }
            }
            Err(e) => app.toast(Toast::new("error", crate::api::error_message(&e)).duration(5000)),
        });
        self.dialogs.clear();
    }

    /// `queued_prompt.undo`: take a queued prompt back into the composer (not while a shell command is typed).
    pub fn undo_queued(&mut self, inbox: &str) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else { return };
        let Some(queued) = self.queued_prompts().into_iter().find(|i| i.id == inbox) else { return };
        if self.editor.shell && !self.editor.text.is_empty() {
            self.toast(Toast::new("error", "Leave shell mode before undoing a queued prompt"));
            return;
        }
        let mut payload = queued.payload.as_object().cloned().unwrap_or_default();
        payload.insert("id".into(), json!(queued.id));
        payload.insert("type".into(), json!("user"));
        payload.insert("time".into(), json!({ "created": queued.time.created }));
        let restored = match Message::from_value(Value::Object(payload)) {
            Some(m) => restored_prompt(&m),
            None => {
                let mut p = PromptInfo::empty();
                p.text = queued.payload.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                p
            }
        };
        self.undo_pending(&sid, inbox, restored, "undo", |app, _composer| {
            app.dialogs.clear();
            app.right_focus = false;
            app.dirty = true;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PastedSource, PastedText};

    fn info(text: &str) -> PromptInfo {
        let mut p = PromptInfo::empty();
        p.text = text.into();
        p
    }

    // history.test (appendPrompt): the second prompt's mentions and pasted ranges move past the first and the
    // blank line; the first keeps its own.
    #[test]
    fn appends_after_a_blank_line_and_shifts_the_new_ranges() {
        let mut a = info("fix @a.ts");
        a.files.push(json!({ "uri": "file:///w/a.ts", "name": "a.ts", "mention": { "start": 4, "end": 9, "text": "@a.ts" } }));
        let mut b = info("see @b.ts 漢");
        b.files.push(json!({ "uri": "file:///w/b.ts", "name": "b.ts", "mention": { "start": 4, "end": 9, "text": "@b.ts" } }));
        b.agents.push(json!({ "name": "explore", "mention": { "start": 0, "end": 3, "text": "see" } }));
        b.pasted.push(PastedText { text: "x".into(), source: PastedSource { start: 10.0, end: 12.0, text: "漢".into() } });
        let out = append_prompt(&a, &b);
        assert_eq!(out.text, "fix @a.ts\n\nsee @b.ts 漢");
        assert_eq!(out.files[0]["mention"], json!({ "start": 4, "end": 9, "text": "@a.ts" }));
        assert_eq!(out.files[1]["mention"], json!({ "start": 15.0, "end": 20.0, "text": "@b.ts" }));
        assert_eq!(out.agents[0]["mention"]["start"], json!(11.0));
        assert_eq!((out.pasted[0].source.start, out.pasted[0].source.end), (21.0, 23.0));
        assert_eq!(out.mode, None);
        // An empty composer takes the prompt as it is.
        let out = append_prompt(&PromptInfo::empty(), &b);
        assert_eq!(out.text, b.text);
        assert_eq!(out.files[0]["mention"], json!({ "start": 4, "end": 9, "text": "@b.ts" }));
        // Wide characters count by display width.
        assert_eq!(measure_edit_text("漢字\n\n"), 6);
    }
}
