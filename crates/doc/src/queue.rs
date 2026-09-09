//! The pending-message queue on a session doc.
//!
//! What you typed while the agent was busy, held where every device can see it
//! (and edit it) until the host has somewhere to put it. Distinct from the
//! `commands` ledger next door: commands are append-only and immutable by
//! design (rule 1), and a queue whose rows can be retyped, reordered and
//! dropped is the opposite of that. So it gets its own container.
//!
//! `queue` is a **LoroMovableList** rather than the plain `LoroList` the rest
//! of the doc uses. A plain list has no move: reordering means delete+insert,
//! which two devices doing it at once resolve into duplicated or lost rows.
//! MovableList carries a real move op, so concurrent reorders converge on one
//! order with every row still present exactly once.
//!
//! Writers: any device (unlike `messages`, which is host-only). The host is
//! the only one that *takes* from the queue — see `DocHost::drain_queue`.

use loro::ToJson;
use serde::{Deserialize, Serialize};

use crate::schema::{DocError, SessionDoc};

/// One unsent message waiting its turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessage {
    /// Stable through promotion: the host uses this as the transcript user
    /// message id when the row is finally dispatched.
    pub id: String,
    /// What the user typed. Never empty — emptying it deletes the row.
    pub text: String,
    /// Committed upload paths, staged at queue time so the row never points at
    /// files that only exist on the device that typed it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Do not automatically steer this row into a live turn. The row remains
    /// visible until turn end or an explicit Steer now / Send now action.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hold_for_turn_end: bool,
    /// Device that queued it.
    pub issued_by: String,
    /// Epoch millis.
    pub issued_at: i64,
    /// Epoch millis of the last text edit, when there has been one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<i64>,
    /// Host-authoritative barrier preventing this row from reaching the
    /// agent while a client has an edit open. Expired edits fail closed into
    /// `ReviewRequired`; they never silently become sendable again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_gate: Option<QueueDeliveryGate>,
}

/// Why a queued row is not currently eligible for automatic or explicit
/// delivery. Kept on the row so moves preserve it and deleting the row cannot
/// leave an orphaned lease behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum QueueDeliveryGate {
    Editing {
        lease_id: String,
        owner_device_id: String,
        owner_instance_id: String,
        acquired_at_ms: i64,
        expires_at_ms: i64,
        base_text_hash: String,
    },
    ReviewRequired {
        previous_lease_id: String,
        owner_device_id: String,
        since_ms: i64,
        base_text_hash: String,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl QueuedMessage {
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        issued_by: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            attachments: Vec::new(),
            hold_for_turn_end: false,
            issued_by: issued_by.into(),
            issued_at: 0,
            edited_at: None,
            delivery_gate: None,
        }
    }
}

impl SessionDoc {
    /// The queue in send order. Malformed rows skip rather than poison the read.
    pub fn read_queue(&self) -> Result<Vec<QueuedMessage>, DocError> {
        let raw = self
            .doc()
            .get_movable_list("queue")
            .get_deep_value()
            .to_json_value();
        let items: Vec<serde_json::Value> = match serde_json::from_value(raw) {
            Ok(items) => items,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(items.into_iter().filter_map(queued_from_json).collect())
    }

    /// Append to the back of the queue.
    pub fn push_queued(&self, item: &QueuedMessage) -> Result<(), DocError> {
        if item.id.trim().is_empty() {
            return Err(DocError::Schema("queued message id required".into()));
        }
        if item.text.trim().is_empty() {
            return Err(DocError::Schema("queued message text required".into()));
        }
        let queue = self.doc().get_movable_list("queue");
        let map = queue.push_container(loro::LoroMap::new())?;
        write_queued_map(&map, item)?;
        self.doc().commit();
        Ok(())
    }

    /// Put a row back at `index` (clamped). The host's undo for a send that
    /// failed after the row was taken: it goes back where it was rather than to
    /// the back of the queue, so a failed "send now" doesn't demote the message
    /// the user just called urgent.
    pub fn insert_queued(&self, index: usize, item: &QueuedMessage) -> Result<(), DocError> {
        if item.id.trim().is_empty() {
            return Err(DocError::Schema("queued message id required".into()));
        }
        if item.text.trim().is_empty() {
            return Err(DocError::Schema("queued message text required".into()));
        }
        let queue = self.doc().get_movable_list("queue");
        let map = queue.insert_container(index.min(queue.len()), loro::LoroMap::new())?;
        write_queued_map(&map, item)?;
        self.doc().commit();
        Ok(())
    }

    /// Retype one row. Empty text means "I don't want to send this after all",
    /// so the row goes — that is the delete gesture, not an error.
    /// `false` when there is no such row, or the text is unchanged.
    pub fn set_queued_text(&self, id: &str, text: &str, now_ms: i64) -> Result<bool, DocError> {
        if text.trim().is_empty() {
            return self.remove_queued(id);
        }
        let queue = self.doc().get_movable_list("queue");
        let Some(index) = index_of(&queue, id) else {
            return Ok(false);
        };
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = queue.get(index)
        else {
            return Ok(false);
        };
        let unchanged = matches!(
            map.get("text"),
            Some(loro::ValueOrContainer::Value(loro::LoroValue::String(s))) if s.as_str() == text
        );
        if unchanged {
            return Ok(false);
        }
        map.insert("text", text)?;
        map.insert("editedAt", now_ms)?;
        self.doc().commit();
        Ok(true)
    }

    /// Install, replace or clear a host-authoritative delivery barrier.
    /// Callers serialize this with the host's queue drain lock.
    pub fn set_queued_delivery_gate(
        &self,
        id: &str,
        gate: Option<&QueueDeliveryGate>,
    ) -> Result<bool, DocError> {
        let queue = self.doc().get_movable_list("queue");
        let Some(index) = index_of(&queue, id) else {
            return Ok(false);
        };
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = queue.get(index)
        else {
            return Ok(false);
        };
        match gate {
            Some(gate) => map.insert(
                "deliveryGate",
                crate::schema::loro_value_from_json(&serde_json::to_value(gate)?),
            )?,
            None => {
                map.delete("deliveryGate")?;
            }
        }
        self.doc().commit();
        Ok(true)
    }

    /// Resolve an edit in one document commit. `None` cancels and preserves
    /// the old text; an empty replacement discards the row; any other value
    /// updates the text and clears the delivery gate atomically.
    ///
    /// Lease ownership is deliberately checked by `DocHost` while holding
    /// its drain lock immediately before this method is called.
    pub fn finish_queued_edit(
        &self,
        id: &str,
        replacement: Option<&str>,
        now_ms: i64,
    ) -> Result<bool, DocError> {
        self.finish_queued_edit_with_attachments(id, replacement, None, now_ms)
    }

    /// Replace text and optional attachment paths in the same CRDT commit
    /// that releases the row. `None` preserves attachments for older clients;
    /// an empty slice explicitly removes all attachments.
    pub fn finish_queued_edit_with_attachments(
        &self,
        id: &str,
        replacement: Option<&str>,
        attachments: Option<&[String]>,
        now_ms: i64,
    ) -> Result<bool, DocError> {
        if replacement.is_some_and(|text| text.trim().is_empty()) {
            return self.remove_queued(id);
        }
        let queue = self.doc().get_movable_list("queue");
        let Some(index) = index_of(&queue, id) else {
            return Ok(false);
        };
        let Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) = queue.get(index)
        else {
            return Ok(false);
        };
        if let Some(text) = replacement {
            let unchanged = matches!(
                map.get("text"),
                Some(loro::ValueOrContainer::Value(loro::LoroValue::String(s)))
                    if s.as_str() == text
            );
            if !unchanged {
                map.insert("text", text)?;
                map.insert("editedAt", now_ms)?;
            }
        }
        if let Some(attachments) = attachments {
            map.insert(
                "attachments",
                crate::schema::loro_value_from_json(&serde_json::to_value(attachments)?),
            )?;
            map.insert("editedAt", now_ms)?;
        }
        map.delete("deliveryGate")?;
        self.doc().commit();
        Ok(true)
    }

    /// Move a row to `to` (clamped to the queue's bounds). `false` when the row
    /// is missing or already sits there.
    pub fn move_queued(&self, id: &str, to: usize) -> Result<bool, DocError> {
        let queue = self.doc().get_movable_list("queue");
        let Some(from) = index_of(&queue, id) else {
            return Ok(false);
        };
        let to = to.min(queue.len().saturating_sub(1));
        if from == to {
            return Ok(false);
        }
        queue.mov(from, to)?;
        self.doc().commit();
        Ok(true)
    }

    /// Drop a row. `false` when it was already gone (another device took it).
    pub fn remove_queued(&self, id: &str) -> Result<bool, DocError> {
        let queue = self.doc().get_movable_list("queue");
        let Some(index) = index_of(&queue, id) else {
            return Ok(false);
        };
        queue.delete(index, 1)?;
        self.doc().commit();
        Ok(true)
    }

    /// Remove a row and hand it back — the host's "send this one" path.
    /// `None` when it is already gone, which is the race we want: two devices
    /// popping the same row means exactly one of them gets it.
    pub fn take_queued(&self, id: &str) -> Result<Option<QueuedMessage>, DocError> {
        let queue = self.doc().get_movable_list("queue");
        let Some(index) = index_of(&queue, id) else {
            return Ok(None);
        };
        let item = self.read_queue()?.into_iter().find(|item| item.id == id);
        // Delivery gates are a model invariant, not merely a UI/engine
        // convention. This keeps future take call sites from accidentally
        // sending a row that is being edited or awaiting review.
        if item
            .as_ref()
            .is_some_and(|item| item.delivery_gate.is_some())
        {
            return Ok(None);
        }
        queue.delete(index, 1)?;
        self.doc().commit();
        Ok(item)
    }

    /// Remove and return the head — the host's turn-end flush.
    pub fn take_queue_head(&self) -> Result<Option<QueuedMessage>, DocError> {
        let Some(head) = self.read_queue()?.into_iter().next() else {
            return Ok(None);
        };
        self.take_queued(&head.id)
    }
}

fn index_of(queue: &loro::LoroMovableList, id: &str) -> Option<usize> {
    (0..queue.len()).find(|&i| {
        matches!(
            queue.get(i),
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map)))
                if matches!(
                    map.get("id"),
                    Some(loro::ValueOrContainer::Value(loro::LoroValue::String(s)))
                        if s.as_str() == id
                )
        )
    })
}

fn write_queued_map(map: &loro::LoroMap, item: &QueuedMessage) -> Result<(), DocError> {
    map.insert("id", item.id.as_str())?;
    map.insert("text", item.text.as_str())?;
    map.insert("issuedBy", item.issued_by.as_str())?;
    map.insert("issuedAt", item.issued_at)?;
    if !item.attachments.is_empty() {
        map.insert(
            "attachments",
            crate::schema::loro_value_from_json(&serde_json::to_value(&item.attachments)?),
        )?;
    }
    if item.hold_for_turn_end {
        map.insert("holdForTurnEnd", true)?;
    }
    if let Some(edited_at) = item.edited_at {
        map.insert("editedAt", edited_at)?;
    }
    if let Some(gate) = &item.delivery_gate {
        map.insert(
            "deliveryGate",
            crate::schema::loro_value_from_json(&serde_json::to_value(gate)?),
        )?;
    }
    Ok(())
}

fn queued_from_json(v: serde_json::Value) -> Option<QueuedMessage> {
    let id = v.get("id")?.as_str()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let text = v.get("text")?.as_str()?.to_string();
    if text.trim().is_empty() {
        return None;
    }
    Some(QueuedMessage {
        id,
        text,
        attachments: v
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok())
            .unwrap_or_default(),
        hold_for_turn_end: v
            .get("holdForTurnEnd")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        issued_by: v
            .get("issuedBy")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
        issued_at: v.get("issuedAt").and_then(|t| t.as_i64()).unwrap_or(0),
        edited_at: v.get("editedAt").and_then(|t| t.as_i64()),
        delivery_gate: match v.get("deliveryGate") {
            None | Some(serde_json::Value::Null) => None,
            Some(gate) => serde_json::from_value(gate.clone()).ok().or_else(|| {
                // A future gate kind must fail closed on an older host. Treat
                // it as review-required instead of silently sending the row.
                Some(QueueDeliveryGate::ReviewRequired {
                    previous_lease_id: String::new(),
                    owner_device_id: String::new(),
                    since_ms: 0,
                    base_text_hash: String::new(),
                })
            }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> SessionDoc {
        SessionDoc::init("chat-1").unwrap()
    }

    fn item(id: &str, text: &str) -> QueuedMessage {
        QueuedMessage {
            id: id.into(),
            text: text.into(),
            attachments: Vec::new(),
            hold_for_turn_end: false,
            issued_by: "device-a".into(),
            issued_at: 1_000,
            edited_at: None,
            delivery_gate: None,
        }
    }

    fn texts(doc: &SessionDoc) -> Vec<String> {
        doc.read_queue()
            .unwrap()
            .into_iter()
            .map(|i| i.text)
            .collect()
    }

    /// A send that fails after the row was taken puts it back where it was,
    /// not at the end: the queue is an order the user chose.
    #[test]
    fn insert_restores_a_taken_row_at_the_head() {
        let doc = doc();
        for (id, text) in [("q1", "first"), ("q2", "second"), ("q3", "third")] {
            doc.push_queued(&item(id, text)).unwrap();
        }
        let taken = doc.take_queued("q2").unwrap().expect("row taken");
        assert_eq!(texts(&doc), vec!["first", "third"]);

        doc.insert_queued(0, &taken).unwrap();
        assert_eq!(texts(&doc), vec!["second", "first", "third"]);
    }

    #[test]
    fn insert_past_the_end_appends_rather_than_failing() {
        let doc = doc();
        doc.push_queued(&item("q1", "first")).unwrap();
        doc.insert_queued(99, &item("q2", "second")).unwrap();
        assert_eq!(texts(&doc), vec!["first", "second"]);
    }

    #[test]
    fn push_and_read_round_trip() {
        let doc = doc();
        assert!(doc.read_queue().unwrap().is_empty());
        let mut first = item("q1", "first");
        first.attachments = vec!["uploads/a.png".into()];
        doc.push_queued(&first).unwrap();
        doc.push_queued(&item("q2", "second")).unwrap();
        assert_eq!(doc.read_queue().unwrap(), vec![first, item("q2", "second")]);
    }

    #[test]
    fn hold_policy_round_trips_and_old_rows_default_to_automatic_steering() {
        let doc = doc();
        let mut held = item("q1", "hold this");
        held.hold_for_turn_end = true;
        doc.push_queued(&held).unwrap();
        doc.push_queued(&item("q2", "legacy behavior")).unwrap();

        let rows = doc.read_queue().unwrap();
        assert!(rows[0].hold_for_turn_end);
        assert!(!rows[1].hold_for_turn_end);
    }

    #[test]
    fn delivery_gate_round_trips_and_finish_clears_it_atomically() {
        let doc = doc();
        let mut queued = item("q1", "original");
        queued.delivery_gate = Some(QueueDeliveryGate::Editing {
            lease_id: "lease-1".into(),
            owner_device_id: "phone".into(),
            owner_instance_id: "view-1".into(),
            acquired_at_ms: 1_000,
            expires_at_ms: 61_000,
            base_text_hash: "hash".into(),
        });
        doc.push_queued(&queued).unwrap();
        assert_eq!(doc.read_queue().unwrap(), vec![queued]);
        let json = serde_json::to_value(doc.read_queue().unwrap()).unwrap();
        assert_eq!(json[0]["deliveryGate"]["leaseId"], "lease-1");
        assert_eq!(json[0]["deliveryGate"]["ownerDeviceId"], "phone");
        assert!(json[0]["deliveryGate"].get("lease_id").is_none());

        assert!(
            doc.finish_queued_edit("q1", Some("revised"), 2_000)
                .unwrap()
        );
        let row = doc.read_queue().unwrap().pop().unwrap();
        assert_eq!(row.text, "revised");
        assert_eq!(row.edited_at, Some(2_000));
        assert_eq!(row.delivery_gate, None);
    }

    #[test]
    fn editing_attachments_preserves_row_identity_order_and_policy() {
        let doc = doc();
        doc.push_queued(&item("before", "first")).unwrap();
        let mut edited = item("edited", "old text");
        edited.attachments = vec!["old.png".into()];
        edited.hold_for_turn_end = true;
        doc.push_queued(&edited).unwrap();
        doc.push_queued(&item("after", "last")).unwrap();
        let paths = vec!["new.png".to_string(), "second.png".to_string()];
        assert!(
            doc.finish_queued_edit_with_attachments(
                "edited",
                Some("new text"),
                Some(&paths),
                2_000,
            )
            .unwrap()
        );
        let rows = doc.read_queue().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["before", "edited", "after"]
        );
        assert_eq!(rows[1].text, "new text");
        assert_eq!(rows[1].attachments, paths);
        assert_eq!(rows[1].issued_at, edited.issued_at);
        assert!(rows[1].hold_for_turn_end);
        assert!(
            doc.finish_queued_edit_with_attachments("edited", Some("text only"), Some(&[]), 3_000,)
                .unwrap()
        );
        assert!(doc.read_queue().unwrap()[1].attachments.is_empty());
    }

    #[test]
    fn cancelling_an_edit_only_clears_the_gate_and_empty_commit_discards() {
        let doc = doc();
        doc.push_queued(&item("q1", "original")).unwrap();
        let review = QueueDeliveryGate::ReviewRequired {
            previous_lease_id: "lease-1".into(),
            owner_device_id: "phone".into(),
            since_ms: 2_000,
            base_text_hash: "hash".into(),
        };
        assert!(doc.set_queued_delivery_gate("q1", Some(&review)).unwrap());
        assert!(doc.finish_queued_edit("q1", None, 3_000).unwrap());
        let row = doc.read_queue().unwrap().pop().unwrap();
        assert_eq!(row.text, "original");
        assert_eq!(row.delivery_gate, None);

        assert!(doc.set_queued_delivery_gate("q1", Some(&review)).unwrap());
        assert!(doc.finish_queued_edit("q1", Some("  "), 4_000).unwrap());
        assert!(doc.read_queue().unwrap().is_empty());
    }

    #[test]
    fn empty_ids_and_text_are_rejected() {
        let doc = doc();
        assert!(doc.push_queued(&item(" ", "hi")).is_err());
        assert!(doc.push_queued(&item("q1", "  ")).is_err());
        assert!(doc.read_queue().unwrap().is_empty());
    }

    #[test]
    fn editing_to_empty_deletes_the_row() {
        let doc = doc();
        doc.push_queued(&item("q1", "first")).unwrap();
        doc.push_queued(&item("q2", "second")).unwrap();

        assert!(doc.set_queued_text("q1", "edited", 2_000).unwrap());
        let rows = doc.read_queue().unwrap();
        assert_eq!(rows[0].text, "edited");
        assert_eq!(rows[0].edited_at, Some(2_000));

        // Same text again is not a write.
        assert!(!doc.set_queued_text("q1", "edited", 3_000).unwrap());

        assert!(doc.set_queued_text("q1", "   ", 3_000).unwrap());
        assert_eq!(texts(&doc), vec!["second"]);
        // Editing a row that is already gone is a no-op, not an error.
        assert!(!doc.set_queued_text("q1", "again", 4_000).unwrap());
    }

    #[test]
    fn move_reorders_and_clamps() {
        let doc = doc();
        for (id, text) in [("q1", "a"), ("q2", "b"), ("q3", "c")] {
            doc.push_queued(&item(id, text)).unwrap();
        }
        assert!(doc.move_queued("q3", 0).unwrap());
        assert_eq!(texts(&doc), vec!["c", "a", "b"]);
        // Past the end clamps to the back rather than failing.
        assert!(doc.move_queued("q3", 99).unwrap());
        assert_eq!(texts(&doc), vec!["a", "b", "c"]);
        // Already there, and unknown ids: no write.
        assert!(!doc.move_queued("q3", 2).unwrap());
        assert!(!doc.move_queued("nope", 0).unwrap());
    }

    #[test]
    fn take_head_pops_in_order_and_take_is_once_only() {
        let doc = doc();
        doc.push_queued(&item("q1", "first")).unwrap();
        doc.push_queued(&item("q2", "second")).unwrap();

        let head = doc.take_queue_head().unwrap().unwrap();
        assert_eq!(head.text, "first");
        assert_eq!(texts(&doc), vec!["second"]);

        assert_eq!(doc.take_queued("q2").unwrap().unwrap().text, "second");
        assert!(doc.take_queued("q2").unwrap().is_none());
        assert!(doc.take_queue_head().unwrap().is_none());
    }

    #[test]
    fn delivery_gate_makes_row_untakeable() {
        let doc = doc();
        let mut row = item("q1", "still editing");
        row.delivery_gate = Some(QueueDeliveryGate::Editing {
            lease_id: "lease-1".into(),
            owner_device_id: "device-a".into(),
            owner_instance_id: "window-a".into(),
            acquired_at_ms: 1_000,
            expires_at_ms: 61_000,
            base_text_hash: "hash".into(),
        });
        doc.push_queued(&row).unwrap();

        assert!(doc.take_queued("q1").unwrap().is_none());
        assert!(doc.take_queue_head().unwrap().is_none());
        assert_eq!(texts(&doc), vec!["still editing"]);
    }

    #[test]
    fn concurrent_reorder_converges_without_losing_rows() {
        let a = doc();
        for (id, text) in [("q1", "a"), ("q2", "b"), ("q3", "c")] {
            a.push_queued(&item(id, text)).unwrap();
        }
        let b = {
            let forked = loro::LoroDoc::new();
            forked.import(&a.export_snapshot().unwrap()).unwrap();
            SessionDoc::from_doc(forked)
        };

        // Two devices reorder at once, and one of them also retypes a row.
        a.move_queued("q3", 0).unwrap();
        b.move_queued("q1", 2).unwrap();
        b.set_queued_text("q2", "b-edited", 5_000).unwrap();

        let from_a = a.doc().export(loro::ExportMode::all_updates()).unwrap();
        let from_b = b.doc().export(loro::ExportMode::all_updates()).unwrap();
        a.doc().import(&from_b).unwrap();
        b.doc().import(&from_a).unwrap();

        let left = a.read_queue().unwrap();
        let right = b.read_queue().unwrap();
        assert_eq!(left, right, "peers converge on one order");
        let mut ids: Vec<&str> = left.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["q1", "q2", "q3"],
            "every row survives exactly once"
        );
        assert!(left.iter().any(|i| i.text == "b-edited"));
    }
}
