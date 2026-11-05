//! The person's words as a job's host heard them, and the grants its workers
//! read.
//!
//! Two halves:
//!
//! - **The evidence (router side, in memory).** [`note_user_prompt`] records
//!   what a person typed into a job as it arrives through the live door —
//!   `queue_live_background_prompt`, the one sink behind a client's `Reply`,
//!   `Steer` and ACP `session/prompt`, and only for a prompt with no origin.
//!   A prompt read back from `state.json` (recovered, queued by a launch, or
//!   written into the file by anyone) is never recorded. [`note_agent_reply`]
//!   keeps the job's last reply, so a "yes" can be read against what it
//!   answers. This is only what a grant is checked against, at the moment the
//!   router asks for one, in the router's own process; it need not outlive it.
//! - **The grants (worker side, on disk).** Issued grants live in
//!   `rebon_work`'s `grants.json`, beside the ledger. The record persists, but
//!   a worker rejects it when the issuing router's process identity changes.
//!   [`TaskGrantSource`] reads the ones covering a worker's task for each
//!   auto-mode decision.
//!
//! What this does not do: tell the person's client apart from another
//! process that holds the IPC token, or keep a same-user shell from writing
//! `grants.json`. File tools are refused both (see `rebon_core::permission`);
//! a shell command is left to the classifier's own rules until workers run
//! sandboxed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use rebon_tool::{LauncherUserStatement, UserGrant, UserGrantSnapshot};

/// How many statements a grant is checked against: the most recent ones.
pub(crate) const MAX_STATEMENTS: usize = 60;
/// How much of one statement is kept.
const MAX_TEXT_CHARS: usize = 2_000;
/// How much of the job's reply a statement carries as its referent.
const MAX_REPLY_CHARS: usize = 1_500;

#[derive(Default)]
struct JobBook {
    statements: Vec<LauncherUserStatement>,
    next_seq: u64,
    last_reply: Option<String>,
}

fn books() -> &'static Mutex<HashMap<String, JobBook>> {
    static BOOKS: OnceLock<Mutex<HashMap<String, JobBook>>> = OnceLock::new();
    BOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record only a live user prompt. A new message invalidates older grants
/// before the prompt is committed, even when the message is not a denial.
pub(crate) fn note_user_prompt(
    home: &std::path::Path,
    job_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    let now = rebon_session_host::now_ms();
    let ledger = rebon_work::Ledger::load(&rebon_work::ledger_path(home))?;
    rebon_work::update_grants(home, &ledger, now, |book| {
        book.revoke_on_user_message(job_id, &ledger, now);
    })?;
    let mut books = books().lock().expect("user statement book poisoned");
    let book = books.entry(job_id.to_string()).or_default();
    book.next_seq += 1;
    let statement = LauncherUserStatement {
        seq: book.next_seq,
        text: clip(text, MAX_TEXT_CHARS),
        replying_to: book.last_reply.clone(),
    };
    book.statements.push(statement);
    if book.statements.len() > MAX_STATEMENTS {
        let excess = book.statements.len() - MAX_STATEMENTS;
        book.statements.drain(..excess);
    }
    Ok(())
}

/// Remember `job_id`'s last reply, so the next statement can be read against
/// the proposal it answers. Called by the host when a turn ends.
pub(crate) fn note_agent_reply(job_id: &str, reply: &str) {
    let reply = reply.trim();
    let mut books = books().lock().expect("user statement book poisoned");
    let book = books.entry(job_id.to_string()).or_default();
    book.last_reply = (!reply.is_empty()).then(|| clip_tail(reply, MAX_REPLY_CHARS));
}

/// Everything recorded for `job_id`, oldest first.
pub(crate) fn statements(job_id: &str) -> Vec<LauncherUserStatement> {
    books()
        .lock()
        .expect("user statement book poisoned")
        .get(job_id)
        .map(|book| book.statements.clone())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn forget(job_id: &str) {
    books()
        .lock()
        .expect("user statement book poisoned")
        .remove(job_id);
}

/// A worker's grants: the live ones in `grants.json` that cover the task it
/// is the worker of, read afresh on every call.
pub(crate) struct TaskGrantSource {
    home: PathBuf,
    worker_job_id: String,
}

impl TaskGrantSource {
    pub(crate) fn new(home: PathBuf, worker_job_id: String) -> Self {
        Self {
            home,
            worker_job_id,
        }
    }

    fn read(&self) -> UserGrantSnapshot {
        let Ok(ledger) = rebon_work::Ledger::load(&rebon_work::ledger_path(&self.home)) else {
            return UserGrantSnapshot {
                authorization_withdrawn: true,
                ..Default::default()
            };
        };
        let Ok(book) = rebon_work::GrantBook::load(&rebon_work::grants_path(&self.home)) else {
            return UserGrantSnapshot {
                authorization_withdrawn: true,
                ..Default::default()
            };
        };
        let now = rebon_session_host::now_ms();
        let store = rebon_session_host::BackgroundStore::new(self.home.clone());
        let grants = book
            .for_worker(&ledger, &self.worker_job_id, now)
            .into_iter()
            .filter(|grant| {
                store.read_state(&grant.router_job_id).is_ok_and(|router| {
                    router.process.pid == Some(grant.router_pid)
                        && router.process.pid_identity.as_deref()
                            == Some(grant.router_pid_identity.as_str())
                })
            })
            .map(|grant| UserGrant {
                id: grant.id.clone(),
                action: grant.action.clone(),
                target: grant.target.clone(),
                user_quote: grant.user_quote.clone(),
                proposal_quote: grant.proposal_quote.clone(),
            })
            .collect();
        UserGrantSnapshot {
            grants,
            authorization_withdrawn: book.router_revoked_for_worker(&ledger, &self.worker_job_id),
            revision: serde_json::to_string(&book).expect("grant book is serializable"),
        }
    }
}

#[async_trait::async_trait]
impl rebon_tool::UserGrantSource for TaskGrantSource {
    async fn snapshot(&self) -> UserGrantSnapshot {
        let source = Self {
            home: self.home.clone(),
            worker_job_id: self.worker_job_id.clone(),
        };
        tokio::task::spawn_blocking(move || source.read())
            .await
            .unwrap_or(UserGrantSnapshot {
                authorization_withdrawn: true,
                ..Default::default()
            })
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars).collect();
    clipped.push_str(" …[truncated]");
    clipped
}

/// The end of a reply is where its question to the person usually is.
fn clip_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - max_chars).collect();
    format!("[…] {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_test_user_prompt(job_id: &str, text: &str) {
        let home = tempfile::tempdir().unwrap();
        note_user_prompt(home.path(), job_id, text).unwrap();
    }

    fn job(name: &str) -> String {
        let id = format!("user-grants-test-{name}-{:?}", std::thread::current().id());
        forget(&id);
        id
    }

    #[test]
    fn statements_keep_order_and_carry_the_reply_they_answer() {
        let router = job("order");
        note_test_user_prompt(&router, "fix CI");
        note_agent_reply(&router, "CI is green. Shall I push the fix to main?");
        note_test_user_prompt(&router, "yes");
        note_test_user_prompt(&router, "  ");

        let seen = statements(&router);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].seq, 1);
        assert_eq!(seen[0].text, "fix CI");
        assert_eq!(seen[0].replying_to, None);
        assert_eq!(seen[1].seq, 2);
        assert_eq!(seen[1].text, "yes");
        assert_eq!(
            seen[1].replying_to.as_deref(),
            Some("CI is green. Shall I push the fix to main?")
        );
    }

    #[test]
    fn a_job_nobody_typed_into_has_nothing() {
        let router = job("empty");
        assert!(statements(&router).is_empty());
        note_agent_reply(&router, "a reply alone records no statement");
        assert!(statements(&router).is_empty());
    }

    #[test]
    fn the_book_keeps_the_most_recent_statements() {
        let router = job("cap");
        for n in 0..(MAX_STATEMENTS + 5) {
            note_test_user_prompt(&router, &format!("message {n}"));
        }
        let seen = statements(&router);
        assert_eq!(seen.len(), MAX_STATEMENTS);
        assert_eq!(
            seen.last().unwrap().text,
            format!("message {}", MAX_STATEMENTS + 4)
        );
        assert_eq!(seen.first().unwrap().seq, 6);
    }

    #[test]
    fn long_text_is_clipped_and_says_so() {
        let router = job("clip");
        note_test_user_prompt(&router, &"x".repeat(MAX_TEXT_CHARS + 10));
        let text = &statements(&router)[0].text;
        assert!(text.ends_with("…[truncated]"));
    }
}
