//! RegistryDoc unit tests. The merge cases mirror
//! `edge/src/registry-core.test.ts` — shared vectors; change both together.
//! The typed-API cases mirror `workspace.rs`'s tests so the drop-in claim is
//! tested, not asserted.

use super::*;
use zeron_proto::{HarnessId, SandboxLevel, SessionStatus};

fn ts(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

fn hlc(ms: i64) -> String {
    encode_hlc(ms, 0, "dev-a")
}

fn hlc_by(ms: i64, device: &str) -> String {
    encode_hlc(ms, 0, device)
}

fn upsert(set: &[(&str, Value)], at: i64) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Upsert,
        set: Some(
            set.iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
        hlc: hlc(at),
        clocks: None,
    }
}

fn update(set: &[(&str, Value)], hlc: String) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Update,
        set: Some(
            set.iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ),
        hlc,
        clocks: None,
    }
}

fn delete(at: i64) -> RowOp {
    RowOp {
        kind: "chats".into(),
        id: "chat-1".into(),
        op: OpKind::Delete,
        set: None,
        hlc: hlc(at),
        clocks: None,
    }
}

fn applied(row: Option<&RegistryRow>, op: &RowOp) -> RegistryRow {
    let (next, changed) = apply_op(row, op);
    assert!(changed, "expected op to change the row");
    next.expect("row after change")
}

// ── merge semantics (mirror of registry-core.test.ts) ───────────────────────

#[test]
fn hlc_orders_lexicographically() {
    assert!(hlc(2) > hlc(1));
    assert!(encode_hlc(1, 2, "a") > encode_hlc(1, 1, "a"));
    assert!(encode_hlc(1, 1, "b") > encode_hlc(1, 1, "a"));
    assert!(hlc(10_000) > hlc(999));
}

#[test]
fn hlc_clock_is_monotonic_across_regressions() {
    let mut clock = HlcClock::default();
    let a = clock.next(1_000, "d");
    let b = clock.next(500, "d"); // wall clock went backwards
    let c = clock.next(1_000, "d"); // and stalled
    assert!(b > a);
    assert!(c > b);
}

#[test]
fn upsert_creates_update_never_does() {
    let row = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    assert_eq!(row.fields["title"], json!("hello"));
    let (missing, changed) = apply_op(None, &update(&[("title", json!("x"))], hlc(2000)));
    assert!(!changed);
    assert!(missing.is_none());
}

#[test]
fn field_lww_newer_wins_older_and_ties_lose() {
    let row = applied(
        None,
        &upsert(
            &[("title", json!("hello")), ("archived", json!(false))],
            1000,
        ),
    );
    let (_, changed) = apply_op(Some(&row), &update(&[("title", json!("stale"))], hlc(500)));
    assert!(!changed);
    // Exact replay: strict-> compare makes re-pushes idempotent.
    let (_, changed) = apply_op(
        Some(&row),
        &upsert(
            &[("title", json!("hello")), ("archived", json!(false))],
            1000,
        ),
    );
    assert!(!changed);
    let renamed = applied(
        Some(&row),
        &update(&[("title", json!("renamed"))], hlc_by(2000, "dev-b")),
    );
    assert_eq!(renamed.fields["title"], json!("renamed"));
    assert_eq!(renamed.clocks["archived"], hlc(1000));
}

#[test]
fn same_ms_conflicts_settle_by_device_deterministically() {
    let base = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    let from_a = update(&[("title", json!("A"))], hlc_by(5000, "dev-a"));
    let from_b = update(&[("title", json!("B"))], hlc_by(5000, "dev-b"));
    let ab = apply_op(apply_op(Some(&base), &from_a).0.as_ref(), &from_b)
        .0
        .unwrap();
    let ba = apply_op(apply_op(Some(&base), &from_b).0.as_ref(), &from_a)
        .0
        .unwrap();
    assert_eq!(ab.fields["title"], json!("B"));
    assert_eq!(ba.fields["title"], json!("B"));
}

#[test]
fn null_deletes_fields_with_a_clock() {
    let row = applied(
        None,
        &upsert(&[("title", json!("x")), ("name", json!("y"))], 1000),
    );
    let row = applied(Some(&row), &update(&[("name", Value::Null)], hlc(2000)));
    assert!(!row.fields.contains_key("name"));
    assert_eq!(row.clocks["name"], hlc(2000));
    let (_, changed) = apply_op(Some(&row), &update(&[("name", json!("zombie"))], hlc(1500)));
    assert!(!changed);
}

#[test]
fn delete_only_wins_when_causally_newer() {
    let row = applied(None, &upsert(&[("title", json!("hello"))], 1000));
    let (_, changed) = apply_op(Some(&row), &delete(500));
    assert!(!changed);
    let gone = applied(Some(&row), &delete(2000));
    assert!(gone.deleted);
    assert!(gone.fields.is_empty());
    // Updates never touch tombstones.
    let (_, changed) = apply_op(
        Some(&gone),
        &update(&[("title", json!("ghost"))], hlc(3000)),
    );
    assert!(!changed);
    // Older upsert can't revive; newer revives from ONLY its own fields.
    let (_, changed) = apply_op(Some(&gone), &upsert(&[("title", json!("old"))], 1500));
    assert!(!changed);
    let revived = applied(Some(&gone), &upsert(&[("title", json!("back"))], 4000));
    assert!(!revived.deleted);
    assert_eq!(revived.fields.len(), 1);
    assert_eq!(revived.fields["title"], json!("back"));
}

#[test]
fn delete_on_missing_plants_guard_tombstone() {
    let gone = applied(None, &delete(1000));
    assert!(gone.deleted);
    let (_, changed) = apply_op(Some(&gone), &upsert(&[("title", json!("late"))], 500));
    assert!(!changed);
}

#[test]
fn seed_ops_preserve_original_causality() {
    let row = applied(
        None,
        &upsert(&[("title", json!("old")), ("status", json!("idle"))], 1000),
    );
    let row = applied(
        Some(&row),
        &update(&[("status", json!("working"))], hlc(9000)),
    );
    let seeded = applied(None, &row_to_seed_op(&row));
    assert_eq!(seeded.fields, row.fields);
    assert_eq!(seeded.clocks, row.clocks);
    let (_, changed) = apply_op(
        Some(&seeded),
        &update(&[("status", json!("errored"))], hlc(5000)),
    );
    assert!(!changed);
    // Tombstones round-trip too.
    let gone = applied(None, &delete(7000));
    let reseeded = applied(None, &row_to_seed_op(&gone));
    assert!(reseeded.deleted);
    assert_eq!(reseeded.del_hlc.as_deref(), Some(hlc(7000).as_str()));
}

#[test]
fn wire_shapes_match_the_edge() {
    // The exact JSON the TS side produces/consumes (registry-core.ts).
    let op: RowOp = serde_json::from_value(json!({
        "kind": "chats", "id": "chat-1", "op": "upsert",
        "set": {"title": "hi", "gone": null},
        "hlc": "0000000001000-000000-dev-a",
        "clocks": {"title": "0000000002000-000000-dev-b"}
    }))
    .unwrap();
    assert_eq!(op.op, OpKind::Upsert);
    assert_eq!(op.clock_for("title"), "0000000002000-000000-dev-b");
    assert_eq!(op.clock_for("gone"), "0000000001000-000000-dev-a");
    let row: RegistryRow = serde_json::from_value(json!({
        "kind": "chats", "id": "chat-1", "seq": 7, "deleted": false,
        "fields": {"title": "hi"}, "clocks": {"title": "0000000001000-000000-dev-a"}
    }))
    .unwrap();
    assert_eq!(row.seq, 7);
    let back = serde_json::to_value(&row).unwrap();
    assert_eq!(back["delHlc"], Value::Null); // skipped, not "del_hlc"
    assert!(back.get("del_hlc").is_none());
    let del: RowOp = serde_json::from_value(json!({
        "kind": "chats", "id": "c", "op": "delete", "hlc": "0000000001000-000000-d"
    }))
    .unwrap();
    assert_eq!(del.op, OpKind::Delete);
}

// ── doc lifecycle ───────────────────────────────────────────────────────────

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        platform: "linux".into(),
        last_seen_at: Some(ts(1_000)),
        created_at: Some(ts(500)),
        version: Some("0.1.0".into()),
    }
}

fn chat(id: &str, device_id: &str) -> Chat {
    Chat {
        id: id.into(),
        device_id: device_id.into(),
        title: Some("First chat".into()),
        archived: false,
        cwd: Some("/tmp/repo".into()),
        branch: Some("main".into()),
        checkout_id: None,
        source_context: None,
        config: Some(ChatConfig {
            harness: HarnessId::Mock,
            model: Some("mock-1".into()),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        }),
        last_message_preview: None,
        last_message_at: None,
        created_at: ts(2_000),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: None,
        last_seen_at: None,
        room_gen: None,
    }
}

fn space(id: &str, device_id: &str, path: &str) -> Space {
    Space {
        id: id.into(),
        device_id: device_id.into(),
        path: path.into(),
        name: None,
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        created_at: ts(1_500),
    }
}

fn session(chat_id: &str, device_id: &str, status: SessionStatus) -> Session {
    Session {
        chat_id: chat_id.into(),
        device_id: device_id.into(),
        status,
        started_at: Some(ts(3_000)),
        updated_at: ts(3_500),
    }
}

/// Stand-in for the server: applies every pushable batch from `docs` to a row
/// table with the SAME merge fn, acks, and broadcasts merged rows to all.
fn server_round(
    server: &mut HashMap<(String, String), RegistryRow>,
    seq: &mut u64,
    docs: &mut [&mut RegistryDoc],
) {
    // Collect each doc's pushable batches first (server applies in order).
    let mut acks: Vec<(usize, String)> = Vec::new();
    let mut touched_all: Vec<RegistryRow> = Vec::new();
    let mut batches: Vec<(usize, PendingBatch)> = Vec::new();
    for (i, doc) in docs.iter_mut().enumerate() {
        for batch in doc.take_pushable() {
            batches.push((i, batch));
        }
    }
    for (i, batch) in batches {
        *seq += 1;
        let mut touched: Vec<RegistryRow> = Vec::new();
        for op in &batch.ops {
            let key = (op.kind.clone(), op.id.clone());
            let (next, changed) = apply_op(server.get(&key), op);
            if let Some(mut next) = next
                && changed
            {
                next.seq = *seq;
                server.insert(key, next.clone());
                touched.retain(|r| !(r.kind == next.kind && r.id == next.id));
                touched.push(next);
            }
        }
        acks.push((i, batch.batch));
        touched_all.extend(touched);
    }
    for doc in docs.iter_mut() {
        let _ = doc.apply_rows(*seq, touched_all.clone());
    }
    for (i, batch) in acks {
        docs[i].ack_batch(&batch, *seq);
    }
}

#[test]
fn rows_round_trip_and_upsert_refreshes() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_device(&device("dev-a", "laptop")).unwrap();
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    doc.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
        .unwrap();

    let state = doc.read_all().unwrap();
    assert_eq!(state.devices, vec![device("dev-a", "laptop")]);
    assert_eq!(state.chats, vec![chat("chat-1", "dev-a")]);
    assert_eq!(
        state.sessions,
        vec![session("chat-1", "dev-a", SessionStatus::Working)]
    );

    let mut updated = chat("chat-1", "dev-a");
    updated.title = None;
    updated.last_message_preview = Some("hello".into());
    updated.last_message_at = Some(ts(9_000));
    doc.upsert_chat(&updated).unwrap();
    let chats = doc.read_chats().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].title, None);
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("hello"));
}

#[test]
fn own_push_ack_never_advances_the_cursor() {
    // Field incident: on the HTTPS transport, push ran before pull in one
    // cycle — the ack jumped the cursor to OUR batch's seq, and the pull
    // then fetched "since" that jumped cursor, permanently skipping every
    // batch other devices wrote in between (new sessions never appeared in
    // a laptop's sidebar while updates to known rows kept flowing).
    let mut ws = RegistryDoc::new("dev-a");
    ws.apply_state(10, true, 0, Vec::new());
    assert_eq!(ws.cursor(), 10);
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let batch = ws.take_pushable().pop().expect("pending batch");
    // The server assigns our batch seq 15 — batches 11..=14 belong to peers
    // and have not reached us yet.
    assert!(ws.ack_batch(&batch.batch, 15));
    assert_eq!(
        ws.cursor(),
        10,
        "the next pull must still fetch the peers' batches 11..=14"
    );
    assert_eq!(ws.pending_len(), 0, "the ack still retires the batch");
}

#[test]
fn broadcast_seq_gap_applies_rows_but_holds_the_cursor() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.apply_state(10, true, 0, Vec::new());
    let row = |id: &str, seq: u64| RegistryRow {
        kind: "chats".into(),
        id: id.into(),
        seq,
        deleted: false,
        del_hlc: None,
        fields: [
            ("id".to_owned(), json!(id)),
            ("deviceId".to_owned(), json!("dev-b")),
            ("createdAt".to_owned(), json!(1_000)),
        ]
        .into_iter()
        .collect(),
        clocks: Default::default(),
    };
    // Contiguous broadcast advances.
    assert!(ws.apply_rows(11, vec![row("chat-a", 11)]));
    assert_eq!(ws.cursor(), 11);
    // A GAPPED broadcast (12..=14 were missed) applies its rows but must
    // not advance — the caller resyncs and the reconnect hello backfills.
    assert!(!ws.apply_rows(15, vec![row("chat-b", 15)]));
    assert_eq!(ws.cursor(), 11, "cursor holds across the gap");
    assert_eq!(ws.read_chats().unwrap().len(), 2, "fresh rows still apply");
    // Idempotent replay of an already-seen batch is not a gap.
    assert!(ws.apply_rows(11, vec![row("chat-a", 11)]));
}

#[test]
fn pre_epoch_snapshots_resync_in_full_once() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.apply_state(42, true, 0, Vec::new());
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let bytes = ws.to_bytes().unwrap();

    // A current-epoch snapshot keeps its cursor.
    let restored = RegistryDoc::from_bytes(&bytes, "dev-a").unwrap();
    assert_eq!(restored.cursor(), 42);
    assert_eq!(restored.read_chats().unwrap().len(), 1);

    // A snapshot from BEFORE the cursor-integrity fixes (no epoch field)
    // may hide a jumped cursor: it zeroes on load so the next hello/pull is
    // a full state, healing any invisible-row window. Rows are kept.
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v.as_object_mut().unwrap().remove("resyncEpoch");
    let old = serde_json::to_vec(&v).unwrap();
    let healed = RegistryDoc::from_bytes(&old, "dev-a").unwrap();
    assert_eq!(
        healed.cursor(),
        0,
        "pre-epoch snapshot forces a full resync"
    );
    assert_eq!(
        healed.read_chats().unwrap().len(),
        1,
        "rows survive the reset"
    );
}

#[test]
fn future_harness_chat_rows_stay_visible_without_their_config() {
    // Field incident: a pre-v0.2.10 client received rows whose
    // config.harness said "opencode" — a variant it didn't have — and
    // dropped the WHOLE row ("skipping malformed registry row"), so new
    // sessions silently never appeared in that device's sidebar. Unknown
    // config values must cost the config, not the row.
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let fields: std::collections::BTreeMap<String, serde_json::Value> = [
        ("id".to_owned(), json!("chat-2")),
        ("deviceId".to_owned(), json!("dev-b")),
        ("createdAt".to_owned(), json!(1_000)),
        (
            "config".to_owned(),
            json!({
                "harness": "harness-from-the-future",
                "model": "novel/model",
                "sandbox": "workspace-write",
            }),
        ),
    ]
    .into_iter()
    .collect();
    let _ = ws.apply_rows(
        7,
        vec![RegistryRow {
            kind: "chats".into(),
            id: "chat-2".into(),
            seq: 7,
            deleted: false,
            del_hlc: None,
            fields,
            clocks: Default::default(),
        }],
    );

    let chats = ws.read_chats().unwrap();
    assert_eq!(chats.len(), 2, "the future-harness row must not vanish");
    let newcomer = chats.iter().find(|c| c.id == "chat-2").expect("visible");
    assert_eq!(
        newcomer.config, None,
        "unknown config degrades, row survives"
    );
    // The well-formed sibling keeps its config untouched.
    assert!(chats.iter().any(|c| c.id == "chat-1" && c.config.is_some()));
}

#[test]
fn field_mutators_round_trip() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_device(&device("dev-a", "laptop")).unwrap();
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();

    assert!(ws.rename_chat("chat-1", "Renamed").unwrap());
    assert!(ws.set_chat_archived("chat-1", true).unwrap());
    assert!(
        ws.set_chat_last_message("chat-1", "preview text", ts(5_000))
            .unwrap()
    );
    assert!(ws.rename_device("dev-a", "workstation").unwrap());
    assert!(ws.set_device_last_seen("dev-a", ts(6_000)).unwrap());
    assert!(!ws.rename_chat("nope", "x").unwrap());
    assert!(!ws.set_chat_archived("nope", true).unwrap());
    assert!(!ws.rename_device("nope", "x").unwrap());

    let chat = ws.chat("chat-1").unwrap().unwrap();
    assert_eq!(chat.title.as_deref(), Some("Renamed"));
    assert!(chat.archived);
    assert_eq!(chat.last_message_preview.as_deref(), Some("preview text"));
    assert_eq!(chat.last_message_at, Some(ts(5_000)));
    let dev = &ws.read_devices().unwrap()[0];
    assert_eq!(dev.name, "workstation");
    assert_eq!(dev.last_seen_at, Some(ts(6_000)));
}

#[test]
fn delete_chat_tombstones_row_and_session() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    ws.upsert_session(&session("chat-1", "dev-a", SessionStatus::Idle))
        .unwrap();
    assert!(ws.delete_chat("chat-1").unwrap());
    assert!(ws.read_chats().unwrap().is_empty());
    assert!(ws.read_sessions().unwrap().is_empty());
    assert!(!ws.delete_chat("chat-1").unwrap());
}

#[test]
fn spaces_round_trip_and_mutate() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_space(&space("sp-1", "dev-a", "/home/u/project"))
        .unwrap();
    let row = ws.space("sp-1").unwrap().expect("row exists");
    assert_eq!(row.display_name(), "project");
    assert!(!row.git_detected);

    assert!(ws.rename_space("sp-1", Some("My Project")).unwrap());
    assert_eq!(
        ws.space("sp-1").unwrap().unwrap().display_name(),
        "My Project"
    );
    assert!(ws.rename_space("sp-1", None).unwrap());
    assert_eq!(ws.space("sp-1").unwrap().unwrap().display_name(), "project");

    assert!(
        ws.set_space_git("sp-1", true, Some("checkout-abc"), ts(4_000))
            .unwrap()
    );
    let row = ws.space("sp-1").unwrap().unwrap();
    assert!(row.git_detected);
    assert_eq!(row.checkout_id.as_deref(), Some("checkout-abc"));
    assert_eq!(row.git_checked_at, Some(ts(4_000)));

    assert!(!ws.rename_space("nope", Some("x")).unwrap());
    assert!(!ws.set_space_git("nope", true, None, ts(1)).unwrap());
}

#[test]
fn chat_seen_is_monotonic() {
    let mut ws = RegistryDoc::new("dev-a");
    ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    assert!(ws.set_chat_seen("chat-1", ts(5_000)).unwrap());
    assert_eq!(
        ws.chat("chat-1").unwrap().unwrap().last_seen_at,
        Some(ts(5_000))
    );
    // Older stamps are ignored without a write.
    let before = ws.pending_len();
    assert!(ws.set_chat_seen("chat-1", ts(4_000)).unwrap());
    assert_eq!(ws.pending_len(), before);
    assert_eq!(
        ws.chat("chat-1").unwrap().unwrap().last_seen_at,
        Some(ts(5_000))
    );
    assert!(!ws.set_chat_seen("nope", ts(1)).unwrap());
}

#[test]
fn two_docs_converge_through_a_server() {
    let mut a = RegistryDoc::new("dev-a");
    let mut b = RegistryDoc::new("dev-b");
    a.upsert_device(&device("dev-a", "laptop")).unwrap();
    a.upsert_chat(&chat("chat-a", "dev-a")).unwrap();
    a.upsert_session(&session("chat-a", "dev-a", SessionStatus::Working))
        .unwrap();
    b.upsert_device(&device("dev-b", "vps")).unwrap();
    b.upsert_chat(&chat("chat-b", "dev-b")).unwrap();

    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut a, &mut b]);

    let sa = a.read_all().unwrap();
    let sb = b.read_all().unwrap();
    assert_eq!(sa, sb);
    assert_eq!(
        sa.devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        vec!["dev-a", "dev-b"]
    );
    assert_eq!(
        sa.chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["chat-a", "chat-b"]
    );
    assert_eq!(sa.sessions.len(), 1);
    assert_eq!(a.pending_len(), 0);
    assert_eq!(b.pending_len(), 0);

    // Concurrent same-field rename settles identically on both.
    a.rename_chat("chat-a", "from a").unwrap();
    b.rename_chat("chat-a", "from b").unwrap();
    server_round(&mut server, &mut seq, &mut [&mut a, &mut b]);
    let title_a = a.chat("chat-a").unwrap().unwrap().title;
    let title_b = b.chat("chat-a").unwrap().unwrap().title;
    assert_eq!(title_a, title_b);
    assert!(matches!(
        title_a.as_deref(),
        Some("from a") | Some("from b")
    ));
}

/// The command plane outruns the registry channel: the host's
/// claim-on-first-command lands with NEWER clocks than the viewer's earlier
/// `createChat`. The claim writes only the fields it knows, so the viewer's
/// `config`/`title` must still land on merge (a full-row claim's `Null`
/// writes deleted them — the missing-harness-icon clobber).
#[test]
fn late_create_chat_config_survives_a_prior_claim() {
    let mut viewer = RegistryDoc::new("dev-viewer");
    let mut host = RegistryDoc::new("dev-a");

    // Viewer writes the real row first (older clocks)…
    viewer.upsert_chat(&chat("chat-race", "dev-a")).unwrap();
    // …then the host claims the same chat before that row reaches it. (Real
    // claims trail by whole seconds; the sleep keeps the HLCs out of the
    // same-millisecond device-id tiebreak.)
    std::thread::sleep(std::time::Duration::from_millis(2));
    host.claim_chat("chat-race", Some("/tmp/repo"), Some("space-1"), ts(9_000));

    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut viewer, &mut host]);

    for doc in [&viewer, &host] {
        let merged = doc.chat("chat-race").unwrap().expect("merged row");
        // Claimed fields (newer clocks) stand…
        assert_eq!(merged.device_id, "dev-a");
        assert_eq!(merged.space_id.as_deref(), Some("space-1"));
        // …and the fields the claim never wrote come from the createChat.
        assert_eq!(merged.config, chat("chat-race", "dev-a").config);
        assert_eq!(merged.title.as_deref(), Some("First chat"));
    }
}

#[test]
fn delete_space_cascades_and_converges() {
    let mut a = RegistryDoc::new("dev-a");
    a.upsert_space(&space("sp-1", "dev-a", "/tmp/one")).unwrap();
    a.upsert_space(&space("sp-2", "dev-a", "/tmp/two")).unwrap();
    let mut in_space = chat("chat-1", "dev-a");
    in_space.space_id = Some("sp-1".into());
    let mut other = chat("chat-2", "dev-a");
    other.space_id = Some("sp-2".into());
    a.upsert_chat(&in_space).unwrap();
    a.upsert_chat(&other).unwrap();
    a.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
        .unwrap();
    let mut b = RegistryDoc::new("dev-b");
    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut a, &mut b]);
    assert_eq!(a.read_all().unwrap(), b.read_all().unwrap());

    let deleted = a.delete_space("sp-1").unwrap();
    assert!(deleted.existed);
    assert_eq!(deleted.chat_ids, vec!["chat-1".to_string()]);
    // Overlay hides the cascade locally before the server even sees it.
    assert_eq!(a.read_spaces().unwrap().len(), 1);
    server_round(&mut server, &mut seq, &mut [&mut a, &mut b]);

    for ws in [&a, &b] {
        let state = ws.read_all().unwrap();
        assert_eq!(
            state
                .spaces
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sp-2"]
        );
        assert_eq!(
            state
                .chats
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["chat-2"]
        );
        assert!(state.sessions.is_empty());
    }
    let again = b.delete_space("sp-1").unwrap();
    assert!(!again.existed);
    assert!(again.chat_ids.is_empty());
}

#[test]
fn persistence_round_trips_rows_pending_and_cursor() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    // Half-sync: authoritative row + one unacked rename.
    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut doc]);
    doc.rename_chat("chat-1", "unacked").unwrap();
    assert_eq!(doc.pending_len(), 1);

    let bytes = doc.to_bytes().unwrap();
    let restored = RegistryDoc::from_bytes(&bytes, "dev-a").unwrap();
    assert_eq!(restored.cursor(), doc.cursor());
    assert_eq!(restored.pending_len(), 1);
    assert_eq!(restored.read_all().unwrap(), doc.read_all().unwrap());
    assert_eq!(
        restored.chat("chat-1").unwrap().unwrap().title.as_deref(),
        Some("unacked")
    );
}

#[test]
fn state_frames_delta_replace_and_reseed() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut doc]);
    assert_eq!(doc.cursor(), seq);

    // Delta: a row someone else pushed.
    let remote = applied(
        None,
        &RowOp {
            kind: "chats".into(),
            id: "chat-9".into(),
            op: OpKind::Upsert,
            set: Some(
                [
                    ("id".to_string(), json!("chat-9")),
                    ("deviceId".to_string(), json!("dev-b")),
                    ("createdAt".to_string(), json!(1)),
                ]
                .into(),
            ),
            hlc: hlc_by(50, "dev-b"),
            clocks: None,
        },
    );
    let outcome = doc.apply_state(seq + 1, false, 0, vec![remote.clone()]);
    assert_eq!(outcome, StateOutcome::Delta);
    assert_eq!(doc.read_chats().unwrap().len(), 2);
    assert_eq!(doc.cursor(), seq + 1);

    // Full replace at a newer seq drops rows the server no longer has…
    // …except it re-seeds rows the server never saw. Here the server returns
    // only chat-9, so chat-1 (authoritative locally) re-seeds.
    let outcome = doc.apply_state(seq + 2, true, 0, vec![remote]);
    assert_eq!(outcome, StateOutcome::Reseeded);
    assert!(doc.pending_len() > 0);
    let seeded = doc
        .chat("chat-1")
        .unwrap()
        .expect("kept via reseed overlay");
    assert_eq!(seeded.device_id, "dev-a");

    // Server behind us (wiped): local rows kept, reseed enqueued.
    let mut fresh = RegistryDoc::new("dev-a");
    fresh.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let mut server2 = HashMap::new();
    let mut seq2 = 0u64;
    server_round(&mut server2, &mut seq2, &mut [&mut fresh]);
    assert_eq!(fresh.pending_len(), 0);
    let outcome = fresh.apply_state(0, true, 0, vec![]);
    assert_eq!(outcome, StateOutcome::Reseeded);
    assert!(fresh.pending_len() > 0);
    assert_eq!(fresh.read_chats().unwrap().len(), 1);
}

#[test]
fn reconnect_replay_is_idempotent() {
    let mut doc = RegistryDoc::new("dev-a");
    doc.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
    let mut server = HashMap::new();
    let mut seq = 0u64;
    // Push once, but the ack is "lost": simulate by applying to the server
    // without acking the doc.
    let batches = doc.take_pushable();
    for batch in &batches {
        seq += 1;
        for op in &batch.ops {
            let key = (op.kind.clone(), op.id.clone());
            let (next, changed) = apply_op(server.get(&key), op);
            if let Some(mut next) = next
                && changed
            {
                next.seq = seq;
                server.insert(key, next);
            }
        }
    }
    // Reconnect: batches become pushable again and re-apply as no-ops.
    doc.mark_disconnected();
    let replay = doc.take_pushable();
    assert_eq!(replay.len(), batches.len());
    for batch in &replay {
        for op in &batch.ops {
            let key = (op.kind.clone(), op.id.clone());
            let (_, changed) = apply_op(server.get(&key), op);
            assert!(!changed, "replayed op must be a no-op");
        }
    }
}

#[test]
fn migration_seeds_pending_upserts_that_lose_to_live_writes() {
    // Build a legacy loro workspace doc, materialize, seed.
    let legacy = crate::workspace::WorkspaceDoc::new();
    legacy.upsert_device(&device("dev-a", "laptop")).unwrap();
    legacy
        .upsert_space(&space("sp-1", "dev-a", "/tmp/one"))
        .unwrap();
    let mut legacy_chat = chat("chat-1", "dev-a");
    legacy_chat.space_id = Some("sp-1".into());
    legacy_chat.title = Some("migrated title".into());
    legacy_chat.last_message_at = Some(ts(400_000));
    legacy.upsert_chat(&legacy_chat).unwrap();
    legacy
        .upsert_session(&session("chat-1", "dev-a", SessionStatus::Idle))
        .unwrap();

    let mut doc = RegistryDoc::new("dev-a");
    let seeded = doc
        .seed_from_workspace(&legacy.read_all().unwrap())
        .unwrap();
    assert_eq!(seeded, 4);
    // Instant: the overlay serves the full state before any server contact.
    let state = doc.read_all().unwrap();
    assert_eq!(state, legacy.read_all().unwrap());

    // Two devices seeding the same converged doc = identical result.
    let mut other = RegistryDoc::new("dev-b");
    other
        .seed_from_workspace(&legacy.read_all().unwrap())
        .unwrap();
    let mut server = HashMap::new();
    let mut seq = 0u64;
    server_round(&mut server, &mut seq, &mut [&mut doc, &mut other]);
    assert_eq!(doc.read_all().unwrap(), other.read_all().unwrap());
    assert_eq!(doc.read_all().unwrap(), legacy.read_all().unwrap());

    // A live rename (now-clock) beats the migrated title everywhere.
    other.rename_chat("chat-1", "live rename").unwrap();
    server_round(&mut server, &mut seq, &mut [&mut doc, &mut other]);
    assert_eq!(
        doc.chat("chat-1").unwrap().unwrap().title.as_deref(),
        Some("live rename")
    );
}
