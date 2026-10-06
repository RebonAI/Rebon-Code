use super::*;
use std::sync::Arc;

use serde_json::json;

fn fact(kind: &str) -> JournalFact {
    JournalFact {
        kind: kind.to_owned(),
        operation_id: Some("operation-1".into()),
        caused_by: None,
        scope_id: Some("session-1".into()),
        plane_epoch: Some("plane-1".into()),
        payload: json!({ "pluginId": "example", "generation": 1 }),
    }
}

fn query(limit: usize) -> JournalQuery {
    JournalQuery {
        after: None,
        through: None,
        limit: NonZeroUsize::new(limit).unwrap(),
        filter: JournalFilter::default(),
    }
}

#[test]
fn empty_journal_has_a_persistent_identity_and_zero_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let watermark = journal.subscribe().watermark;
    assert_eq!(watermark.sequence, 0);
    let page = journal.query(&query(10)).unwrap();
    assert!(page.events.is_empty());
    assert_eq!(page.next, watermark);
    assert_eq!(page.through, watermark);
    drop(journal);
    assert_eq!(
        Journal::open(&path).unwrap().subscribe().watermark,
        watermark
    );
}

#[test]
fn committed_event_round_trips_with_causal_and_plane_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let requested = journal.append(fact("requested")).unwrap();
    let mut completed = fact("completed");
    completed.caused_by = Some(requested.cursor.clone());
    let event = journal.append(completed).unwrap();
    assert_eq!(event.schema_version, SCHEMA_VERSION);
    assert_eq!(event.cursor.sequence, 2);
    assert_eq!(event.fact.caused_by, Some(requested.cursor));
    assert_eq!(event.fact.plane_epoch.as_deref(), Some("plane-1"));
    drop(journal);
    let reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.query(&query(10)).unwrap().events[1], event);
}

#[test]
fn reopening_preserves_sequence_and_rotates_producer_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let first = journal.append(fact("first")).unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    let second = journal.append(fact("second")).unwrap();
    assert_eq!(first.cursor.journal_id, second.cursor.journal_id);
    assert_eq!(second.cursor.sequence, 2);
    assert_ne!(first.producer_epoch, second.producer_epoch);
}

#[test]
fn live_delivery_only_contains_records_that_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let mut subscription = journal.subscribe();
    let committed = journal.append(fact("ready")).unwrap();
    let delivered = subscription.events.try_recv().unwrap();
    assert_eq!(delivered, committed);
    drop(journal);
    let reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.query(&query(10)).unwrap().events, vec![delivered]);
}

#[test]
fn snapshot_query_and_live_subscription_have_no_overlap_or_gap() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    let before = journal.append(fact("before")).unwrap();
    let mut subscription = journal.subscribe();
    let after = journal.append(fact("after")).unwrap();
    let mut history = query(10);
    history.through = Some(subscription.watermark.clone());
    let page = journal.query(&history).unwrap();
    assert_eq!(page.events, vec![before]);
    assert_eq!(page.next, subscription.watermark);
    assert_eq!(subscription.events.try_recv().unwrap(), after);
}

#[test]
fn filtering_advances_cursor_over_nonmatching_records() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    journal.append(fact("ignore")).unwrap();
    let second = journal.append(fact("match")).unwrap();
    let mut other = fact("match");
    other.scope_id = Some("session-2".into());
    journal.append(other).unwrap();
    let mut other = fact("match");
    other.operation_id = Some("operation-2".into());
    journal.append(other).unwrap();
    let fifth = journal.append(fact("match")).unwrap();
    let tail = journal.append(fact("ignore")).unwrap();
    let mut request = query(1);
    request.filter = JournalFilter {
        scope_id: Some("session-1".into()),
        operation_id: Some("operation-1".into()),
        kind: Some("match".into()),
    };
    let page = journal.query(&request).unwrap();
    assert_eq!(page.events, vec![second.clone()]);
    assert_eq!(page.next, second.cursor);
    assert_eq!(page.through, tail.cursor);
    request.after = Some(page.next);
    let page = journal.query(&request).unwrap();
    assert_eq!(page.events, vec![fifth.clone()]);
    assert_eq!(page.next, fifth.cursor);
    request.after = Some(page.next);
    let page = journal.query(&request).unwrap();
    assert!(page.events.is_empty());
    assert_eq!(page.next, tail.cursor);
}

#[test]
fn foreign_future_and_reversed_cursors_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    let first = journal.append(fact("first")).unwrap();
    let second = journal.append(fact("second")).unwrap();
    let mut request = query(10);
    request.after = Some(JournalCursor {
        journal_id: "other".into(),
        sequence: 0,
    });
    assert_eq!(
        journal.query(&request).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    request.after = Some(JournalCursor {
        sequence: 3,
        ..first.cursor.clone()
    });
    assert_eq!(
        journal.query(&request).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    request.after = Some(second.cursor);
    request.through = Some(first.cursor);
    assert_eq!(
        journal.query(&request).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn concurrent_appends_and_live_delivery_share_one_order() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(&dir.path().join("timeline.jsonl")).unwrap());
    let mut subscription = journal.subscribe();
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let journal = Arc::clone(&journal);
            std::thread::spawn(move || {
                for _ in 0..8 {
                    journal.append(fact("ready")).unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let page = journal.query(&query(40)).unwrap();
    for (index, event) in page.events.iter().enumerate() {
        assert_eq!(event.cursor.sequence, index as u64 + 1);
        assert_eq!(subscription.events.try_recv().unwrap(), *event);
    }
    assert_eq!(page.events.len(), 32);
}

#[test]
fn lagged_subscriber_can_recover_every_event_from_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    journal.state.lock().unwrap().events = broadcast::channel(2).0;
    let mut subscription = journal.subscribe();
    for _ in 0..4 {
        journal.append(fact("ready")).unwrap();
    }
    assert!(matches!(
        subscription.events.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(2))
    ));
    let mut request = query(10);
    request.after = Some(subscription.watermark);
    let recovered = journal.query(&request).unwrap();
    assert_eq!(recovered.events.len(), 4);
    assert_eq!(recovered.next.sequence, 4);
}

#[test]
fn second_writer_is_refused_until_the_first_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let first = Journal::open(&path).unwrap();
    assert!(Journal::open(&path).is_err());
    first.append(fact("ready")).unwrap();
    drop(first);
    assert!(Journal::open(&path).is_ok());
}

#[test]
fn partial_tail_is_backed_up_and_recovered_without_reusing_a_committed_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    journal.append(fact("ready")).unwrap();
    drop(journal);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"cursor\":")
        .unwrap();
    let damaged = std::fs::read(&path).unwrap();
    let reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.query(&query(10)).unwrap().events.len(), 1);
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".torn-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(reopened.recovered_tail_backup(), Some(backups[0].as_path()));
    assert_eq!(std::fs::read(&backups[0]).unwrap(), damaged);
    assert_eq!(
        reopened.append(fact("recovered")).unwrap().cursor.sequence,
        2
    );
    drop(reopened);
    let reopened = Journal::open(&path).unwrap();
    assert!(reopened.recovered_tail_backup().is_none());
    assert_eq!(reopened.subscribe().watermark.sequence, 2);
}

#[test]
fn active_writer_allows_an_independent_handle_to_read_committed_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let committed = journal.append(fact("ready")).unwrap();
    let contents = std::fs::read_to_string(&path).unwrap();
    let observed: JournalEvent = serde_json::from_str(contents.lines().nth(1).unwrap()).unwrap();
    assert_eq!(observed, committed);
    assert!(Journal::open(&path).is_err());
}

#[test]
fn malformed_versions_and_duplicate_sequences_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.jsonl");
    let journal = Journal::open(&original).unwrap();
    journal.append(fact("ready")).unwrap();
    drop(journal);
    let lines: Vec<Value> = std::fs::read_to_string(&original)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for case in 0..5 {
        let mut records = lines.clone();
        match case {
            0 => records[0]["schemaVersion"] = json!(2),
            1 => records[1]["schemaVersion"] = json!(2),
            2 => records[1]["cursor"]["sequence"] = json!(2),
            3 => records[1]["cursor"]["journalId"] = json!("other"),
            4 => records.push(records[1].clone()),
            _ => unreachable!(),
        }
        let bytes = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        let path = dir.path().join(format!("bad-{case}.jsonl"));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            Journal::open(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn oversized_append_does_not_consume_cursor_or_poison_writer() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    let mut event = fact("oversized");
    event.payload = json!("x".repeat(MAX_RECORD_BYTES));
    assert_eq!(
        journal.append(event).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(journal.subscribe().watermark.sequence, 0);
    assert_eq!(journal.append(fact("ready")).unwrap().cursor.sequence, 1);
}

#[test]
fn write_failure_is_latched_without_broadcast_or_watermark_advance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let mut subscription = journal.subscribe();
    {
        let mut state = journal.state.lock().unwrap();
        state.file = File::open(&path).unwrap();
    }
    assert!(journal.append(fact("failed")).is_err());
    assert_eq!(
        journal.append(fact("again")).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(journal.subscribe().watermark.sequence, 0);
    assert!(matches!(
        subscription.events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(journal.query(&query(10)).unwrap().events.is_empty());
}

#[test]
fn querying_history_does_not_publish_or_execute_anything() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    journal.append(fact("tool-completed")).unwrap();
    let mut subscription = journal.subscribe();
    for _ in 0..3 {
        assert_eq!(journal.query(&query(10)).unwrap().events.len(), 1);
    }
    assert!(matches!(
        subscription.events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

#[test]
fn reader_accepts_crlf_without_changing_record_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let event = journal.append(fact("ready")).unwrap();
    drop(journal);
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&path, text).unwrap();
    assert_eq!(
        Journal::open(&path)
            .unwrap()
            .query(&query(10))
            .unwrap()
            .events,
        vec![event]
    );
}

#[test]
fn incomplete_header_is_backed_up_before_initialization() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let partial_header = b"{\"schemaVersion\":";
    std::fs::write(&path, partial_header).unwrap();
    let journal = Journal::open(&path).unwrap();
    assert_eq!(
        std::fs::read(journal.recovered_tail_backup().unwrap()).unwrap(),
        partial_header
    );
    assert_eq!(journal.subscribe().watermark.sequence, 0);
    assert_eq!(journal.append(fact("ready")).unwrap().cursor.sequence, 1);
}

#[test]
fn complete_corruption_before_a_torn_tail_is_not_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    journal.append(fact("ready")).unwrap();
    drop(journal);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"not-json\n{\"kind\":")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        Journal::open(&path).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".torn-")
    }));
}

#[test]
fn recovery_does_not_truncate_when_backup_cannot_be_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let bytes = b"complete\npartial";
    std::fs::write(&path, bytes).unwrap();
    let mut file = private_options().open(&path).unwrap();
    let unavailable_backup_path = dir.path().join("missing").join("timeline.jsonl");
    assert!(recover_tail(&mut file, &unavailable_backup_path, 9).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn indexed_pages_preserve_writer_position_and_rebuild_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let first = journal.append(fact("first")).unwrap();
    let second = journal.append(fact("second")).unwrap();
    let third = journal.append(fact("third")).unwrap();
    let offsets = journal.state.lock().unwrap().offsets.clone();
    assert_eq!(offsets.len(), 3);
    let mut request = query(1);
    request.after = Some(first.cursor);
    journal
        .state
        .lock()
        .unwrap()
        .file
        .seek(SeekFrom::Start(0))
        .unwrap();
    let page = journal.query(&request).unwrap();
    assert_eq!(page.events, vec![second]);
    assert_eq!(
        journal
            .state
            .lock()
            .unwrap()
            .file
            .stream_position()
            .unwrap(),
        0
    );
    request.after = Some(page.next);
    assert_eq!(journal.query(&request).unwrap().events, vec![third]);
    drop(journal);
    let reopened = Journal::open(&path).unwrap();
    assert_eq!(reopened.state.lock().unwrap().offsets, offsets);
    assert_eq!(
        reopened.query(&request).unwrap().events[0].cursor.sequence,
        3
    );
}

#[test]
fn committed_prefix_remains_queryable_after_a_partial_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let journal = Journal::open(&path).unwrap();
    let first = journal.append(fact("first")).unwrap();
    {
        let mut state = journal.state.lock().unwrap();
        state.file.write_all(b"{\"partial\":").unwrap();
        state.write_failure = Some("injected partial write".into());
    }
    assert_eq!(journal.query(&query(10)).unwrap().events, vec![first]);
    assert_eq!(
        journal.append(fact("rejected")).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[test]
fn empty_kinds_do_not_consume_cursors_or_reach_live_readers() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(&dir.path().join("timeline.jsonl")).unwrap();
    let mut subscription = journal.subscribe();
    for kind in ["", " \t\n"] {
        assert_eq!(
            journal.append(fact(kind)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(journal.subscribe().watermark.sequence, 0);
    assert!(matches!(
        subscription.events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(journal.append(fact("ready")).unwrap().cursor.sequence, 1);
}

#[cfg(unix)]
#[test]
fn new_journal_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.jsonl");
    let _journal = Journal::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
