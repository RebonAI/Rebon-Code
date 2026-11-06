//! When a job last told the person something.
//!
//! The router speaks to the person through `TellPerson`; the process hosting
//! its session notes, at the end of each turn that told them anything, when
//! that was, in `<job dir>/person-report.json`. The supervisor reads it to
//! decide whether the router is due a progress check.

use serde::{Deserialize, Serialize};

use crate::store::BackgroundStore;

const FILE_NAME: &str = "person-report.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonReport {
    at_ms: u64,
}

/// Note that `job_id` told the person something at `at_ms`.
pub fn note_person_report(
    store: &BackgroundStore,
    job_id: &str,
    at_ms: u64,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&PersonReport { at_ms }).map_err(std::io::Error::other)?;
    rebon_session::write_file_atomically(&store.job_dir(job_id).join(FILE_NAME), &bytes)
}

/// When `job_id` last told the person something, if it ever did.
pub fn last_person_report_ms(store: &BackgroundStore, job_id: &str) -> Option<u64> {
    let bytes = std::fs::read(store.job_dir(job_id).join(FILE_NAME)).ok()?;
    serde_json::from_slice::<PersonReport>(&bytes)
        .ok()
        .map(|report| report.at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_remembered_per_job() {
        let root = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(root.path());
        std::fs::create_dir_all(store.job_dir("router")).unwrap();
        assert_eq!(last_person_report_ms(&store, "router"), None);
        note_person_report(&store, "router", 42).unwrap();
        assert_eq!(last_person_report_ms(&store, "router"), Some(42));
        assert_eq!(last_person_report_ms(&store, "worker"), None);
    }
}
