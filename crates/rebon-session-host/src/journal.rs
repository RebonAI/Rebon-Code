//! Durable operation facts, with one writer and non-blocking live delivery.
//!
//! Queries and replay only return records; they never execute the recorded
//! operation. Each file has a persistent cursor domain. A writer holds a
//! sidecar lock so independent readers can still inspect the data on Windows.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

const SCHEMA_VERSION: u32 = 1;
// Facts contain structured metadata or transcript references, not whole transcripts.
const MAX_RECORD_BYTES: usize = 1024 * 1024;
// Slow readers catch up from disk instead of growing a queue for the process lifetime.
const LIVE_CAPACITY: usize = 256;

/// A position in one persistent journal, not a cross-process total order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalCursor {
    pub journal_id: String,
    pub sequence: u64,
}

/// A fact attributed by trusted host code, before the journal stamps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalFact {
    pub kind: String,
    pub operation_id: Option<String>,
    pub caused_by: Option<JournalCursor>,
    pub scope_id: Option<String>,
    /// Supplied by the trusted host; plugins must not choose their own identity.
    pub plane_epoch: Option<String>,
    pub payload: Value,
}

/// A versioned, durable fact; timestamps are display metadata, not ordering keys.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEvent {
    pub schema_version: u32,
    pub cursor: JournalCursor,
    /// Random writer identity, renewed on every open, including in the same process.
    pub producer_epoch: String,
    pub timestamp_ms: u64,
    pub fact: JournalFact,
}

/// Result filtering is not authorization. Endpoints must check scope access first.
#[derive(Clone, Debug, Default)]
pub struct JournalFilter {
    pub scope_id: Option<String>,
    pub operation_id: Option<String>,
    pub kind: Option<String>,
}

impl JournalFilter {
    fn matches(&self, fact: &JournalFact) -> bool {
        self.scope_id
            .as_ref()
            .is_none_or(|scope| fact.scope_id.as_ref() == Some(scope))
            && self
                .operation_id
                .as_ref()
                .is_none_or(|id| fact.operation_id.as_ref() == Some(id))
            && self.kind.as_ref().is_none_or(|kind| fact.kind == *kind)
    }
}

/// A bounded page request. `after` is exclusive and `through` is inclusive.
/// Keep `through` fixed across pages when reading a subscription's history.
pub struct JournalQuery {
    pub after: Option<JournalCursor>,
    pub through: Option<JournalCursor>,
    pub limit: NonZeroUsize,
    pub filter: JournalFilter,
}

/// Matching facts and the last scanned position, including nonmatching records.
pub struct JournalPage {
    pub events: Vec<JournalEvent>,
    pub next: JournalCursor,
    pub through: JournalCursor,
}

/// Live records strictly after `watermark`. On `Lagged`, query from the last
/// applied cursor; filter duplicate live records by their cursor after catching up.
pub struct JournalSubscription {
    pub watermark: JournalCursor,
    pub events: broadcast::Receiver<JournalEvent>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    schema_version: u32,
    journal_id: String,
}

struct State {
    file: File,
    offsets: Vec<u64>,
    committed_len: u64,
    write_failure: Option<String>,
    events: broadcast::Sender<JournalEvent>,
}

struct Scan {
    header: Option<Header>,
    offsets: Vec<u64>,
    committed_len: u64,
    torn_tail: bool,
}

/// One writer for a journal file. Append commits to disk before notifying readers.
/// The sidecar lock lives as long as this object; share it with `Arc` in-process.
pub struct Journal {
    journal_id: String,
    producer_epoch: String,
    path: PathBuf,
    recovery_backup: Option<PathBuf>,
    state: Mutex<State>,
    // Drop the writer state before releasing admission to the next writer.
    _writer_lock: File,
}

impl Journal {
    /// Opens or creates a journal under an existing directory. A torn last line
    /// is backed up before truncation; corrupt complete records are never repaired.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let writer_lock = private_options()
            .create(true)
            .open(sibling(&path, ".lock"))?;
        FileExt::try_lock_exclusive(&writer_lock)?;
        let mut file = private_options().create(true).open(&path)?;
        let mut scanned = scan(&mut file)?;
        let recovery_backup = if scanned.torn_tail {
            Some(recover_tail(&mut file, &path, scanned.committed_len)?)
        } else {
            None
        };
        let header = match scanned.header {
            Some(header) => header,
            None => {
                let header = Header {
                    schema_version: SCHEMA_VERSION,
                    journal_id: new_epoch()?,
                };
                let mut bytes = serde_json::to_vec(&header).expect("journal header is plain data");
                bytes.push(b'\n');
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                sync_parent(&path)?;
                scanned.committed_len = bytes.len() as u64;
                header
            }
        };
        let (events, _) = broadcast::channel(LIVE_CAPACITY);
        Ok(Self {
            journal_id: header.journal_id,
            producer_epoch: new_epoch()?,
            path,
            recovery_backup,
            state: Mutex::new(State {
                file,
                offsets: scanned.offsets,
                committed_len: scanned.committed_len,
                write_failure: None,
                events,
            }),
            _writer_lock: writer_lock,
        })
    }

    /// The byte-for-byte backup made during this open, if a torn tail was recovered.
    pub fn recovered_tail_backup(&self) -> Option<&Path> {
        self.recovery_backup.as_deref()
    }

    /// Commits a fact and then broadcasts it. An I/O error can leave a complete
    /// record on disk: reconcile by operation_id rather than repeating side effects.
    pub fn append(&self, fact: JournalFact) -> io::Result<JournalEvent> {
        if fact.kind.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal kind is empty",
            ));
        }
        let mut state = self.state.lock().expect("journal state poisoned");
        if let Some(failure) = &state.write_failure {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, failure.clone()));
        }
        let event = JournalEvent {
            schema_version: SCHEMA_VERSION,
            cursor: self.cursor(state.offsets.len() as u64 + 1),
            producer_epoch: self.producer_epoch.clone(),
            timestamp_ms: rebon_types::wall_clock_ms(),
            fact,
        };
        let mut bytes = serde_json::to_vec(&event).expect("journal event contains only JSON data");
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal record too large",
            ));
        }
        let committed_len = state.committed_len + bytes.len() as u64;
        let result = (|| {
            state.file.seek(SeekFrom::End(0))?;
            state.file.write_all(&bytes)?;
            state.file.sync_data()
        })();
        if let Err(error) = result {
            // Never hide a partial append behind a later successful write.
            state.write_failure = Some(error.to_string());
            return Err(error);
        }
        let offset = state.committed_len;
        state.offsets.push(offset);
        state.committed_len = committed_len;
        // No live subscribers is not a commit failure; lagged readers catch up on disk.
        let _ = state.events.send(event.clone());
        Ok(event)
    }

    /// Atomically joins live delivery and captures its exclusive starting watermark.
    pub fn subscribe(&self) -> JournalSubscription {
        let state = self.state.lock().expect("journal state poisoned");
        JournalSubscription {
            watermark: self.cursor(state.offsets.len() as u64),
            events: state.events.subscribe(),
        }
    }

    /// Reads a stable committed prefix with an independent file handle, without
    /// holding the writer mutex during disk I/O. No recorded operation is executed.
    pub fn query(&self, query: &JournalQuery) -> io::Result<JournalPage> {
        let (after, through, start, end) = {
            let state = self.state.lock().expect("journal state poisoned");
            let head = state.offsets.len() as u64;
            let after = self.query_sequence(query.after.as_ref(), 0, head)?;
            let through = self.query_sequence(query.through.as_ref(), head, head)?;
            if after > through {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "reversed journal range",
                ));
            }
            let start = state
                .offsets
                .get(after as usize)
                .copied()
                .unwrap_or(state.committed_len);
            let end = state
                .offsets
                .get(through as usize)
                .copied()
                .unwrap_or(state.committed_len);
            (after, through, start, end)
        };
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file.take(end - start));
        let mut page = JournalPage {
            events: Vec::new(),
            next: self.cursor(after),
            through: self.cursor(through),
        };
        while page.next.sequence < through {
            let Some(bytes) = read_record(&mut reader)? else {
                return Err(invalid_data("committed journal records are missing"));
            };
            let event: JournalEvent = serde_json::from_slice(&bytes).map_err(invalid_data)?;
            page.next = event.cursor.clone();
            if query.filter.matches(&event.fact) {
                page.events.push(event);
                if page.events.len() == query.limit.get() {
                    break;
                }
            }
        }
        Ok(page)
    }

    fn cursor(&self, sequence: u64) -> JournalCursor {
        JournalCursor {
            journal_id: self.journal_id.clone(),
            sequence,
        }
    }

    fn query_sequence(
        &self,
        cursor: Option<&JournalCursor>,
        default: u64,
        head: u64,
    ) -> io::Result<u64> {
        match cursor {
            Some(cursor) if cursor.journal_id != self.journal_id || cursor.sequence > head => {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "foreign or future journal cursor",
                ))
            }
            Some(cursor) => Ok(cursor.sequence),
            None => Ok(default),
        }
    }
}

fn scan(file: &mut File) -> io::Result<Scan> {
    let mut reader = BufReader::new(file);
    let mut scanned = Scan {
        header: None,
        offsets: Vec::new(),
        committed_len: 0,
        torn_tail: false,
    };
    let Some(bytes) = read_record(&mut reader)? else {
        return Ok(scanned);
    };
    if bytes.last() != Some(&b'\n') {
        scanned.torn_tail = true;
        return Ok(scanned);
    }
    let header: Header = serde_json::from_slice(&bytes).map_err(invalid_data)?;
    if header.schema_version != SCHEMA_VERSION || header.journal_id.is_empty() {
        return Err(invalid_data("unsupported journal header"));
    }
    scanned.committed_len = bytes.len() as u64;
    while let Some(bytes) = read_record(&mut reader)? {
        if bytes.last() != Some(&b'\n') {
            scanned.torn_tail = true;
            break;
        }
        let event: JournalEvent = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        if event.schema_version != SCHEMA_VERSION
            || event.cursor.journal_id != header.journal_id
            || event.cursor.sequence != scanned.offsets.len() as u64 + 1
            || event.producer_epoch.is_empty()
            || event.fact.kind.trim().is_empty()
        {
            return Err(invalid_data(
                "invalid journal event identity, kind or version",
            ));
        }
        scanned.offsets.push(scanned.committed_len);
        scanned.committed_len += bytes.len() as u64;
    }
    scanned.header = Some(header);
    Ok(scanned)
}

fn recover_tail(file: &mut File, path: &Path, committed_len: u64) -> io::Result<PathBuf> {
    let backup_path = sibling(path, &format!(".torn-{}", new_epoch()?));
    let mut backup = private_options().create_new(true).open(&backup_path)?;
    file.seek(SeekFrom::Start(0))?;
    io::copy(file, &mut backup)?;
    backup.sync_all()?;
    sync_parent(&backup_path)?;
    // The original bytes and backup directory entry must survive before truncation.
    file.set_len(committed_len)?;
    file.sync_all()?;
    Ok(backup_path)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().expect("absolute journal path has a parent"))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn read_record(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid_data("oversized journal record"));
    }
    Ok(Some(bytes))
}

fn new_epoch() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "journal/tests.rs"]
mod tests;
