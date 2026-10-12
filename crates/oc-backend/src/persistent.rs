//! `persistentPty.connect`: oc-persistent-pty attachments as oc-server `PersistentPtyAttachment`s
//! (oc-server does the WebSocket framing: `attached`, replay, `replay_complete`, live events).

use std::sync::Arc;

use futures::StreamExt;
use oc_persistent_pty::{AttachInput, PersistentPty, Role, StreamEvent};
use oc_server::{ApiError, PersistentPtyAttachRequest, PersistentPtyAttachment, PersistentPtyEvent, PtyRole};

use crate::util::conv;

struct Control {
    pty: PersistentPty,
    id: String,
    attachment_id: String,
    detach: oc_persistent_pty::DetachHandle,
}

#[async_trait::async_trait]
impl oc_server::PersistentPtyControl for Control {
    async fn input(&self, cols: u16, rows: u16, data: bytes::Bytes) {
        let _ = self.pty.input(&self.id, &self.attachment_id, cols as u64, rows as u64, &data).await;
    }
    async fn control(&self, cols: u16, rows: u16) {
        let _ = self.pty.control(&self.id, &self.attachment_id, cols as u64, rows as u64).await;
    }
    fn detach(&self) {
        self.detach.detach();
    }
}

/// `StreamEvent` → oc-server event (`Output` binary, `Resized` with checkpoint, others JSON).
pub fn event(e: StreamEvent) -> PersistentPtyEvent {
    match e {
        StreamEvent::Output { data, .. } => PersistentPtyEvent::Output(data.into()),
        StreamEvent::Resized { cols, rows, generation, checkpoint } => PersistentPtyEvent::Resized {
            cols: cols.min(u16::MAX as u64) as u16,
            rows: rows.min(u16::MAX as u64) as u16,
            generation: generation as i64,
            checkpoint: checkpoint.into(),
        },
        other => PersistentPtyEvent::Json(other.to_ws_json().unwrap_or_default()),
    }
}

pub async fn connect(pty: &PersistentPty, req: PersistentPtyAttachRequest) -> Result<PersistentPtyAttachment, ApiError> {
    let role = match req.role {
        PtyRole::Controller => Role::Controller,
        PtyRole::Observer => Role::Observer,
    };
    let input = AttachInput { cursor: req.cursor, attachment_id: req.attachment_id.clone(), role, takeover: req.takeover };
    // Any error closes the socket with 4404 "terminal unavailable".
    let mut attachment = pty.attach(&req.pty_id, input).await.map_err(|e| ApiError::defect(e.to_string()))?;
    let events = attachment.activate().ok_or_else(|| ApiError::defect("attachment already active"))?;
    let control = Control {
        pty: pty.clone(),
        id: req.pty_id.clone(),
        attachment_id: req.attachment_id,
        detach: attachment.detach_handle(),
    };
    Ok(PersistentPtyAttachment {
        info: conv(&attachment.info)?,
        role: match attachment.role {
            Role::Controller => PtyRole::Controller,
            Role::Observer => PtyRole::Observer,
        },
        generation: attachment.generation as i64,
        replay: oc_server::PersistentPtyReplay {
            requested_offset: attachment.replay.requested_offset,
            available_offset: attachment.replay.available_offset,
            end_offset: attachment.replay.end_offset,
            truncated: attachment.replay.truncated,
            data: std::mem::take(&mut attachment.replay.data).into(),
        },
        events: events.map(event).boxed(),
        control: Arc::new(control),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stream_events() {
        assert!(matches!(event(StreamEvent::Output { start: 0, end: 2, data: b"hi".to_vec() }), PersistentPtyEvent::Output(b) if &b[..] == b"hi"));
        assert!(matches!(
            event(StreamEvent::Resized { cols: 80, rows: 24, generation: 3, checkpoint: vec![1] }),
            PersistentPtyEvent::Resized { cols: 80, rows: 24, generation: 3, .. }
        ));
        match event(StreamEvent::TitleChanged { title: "t".into() }) {
            PersistentPtyEvent::Json(v) => assert_eq!(v["type"], "title_changed"),
            _ => panic!("json expected"),
        }
    }
}
