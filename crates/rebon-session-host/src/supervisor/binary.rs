use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

pub fn executable_modified_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_ms)
}

pub fn supervisor_binary_changed(path: &Path, started_modified_at: Option<u64>) -> bool {
    let Some(started_modified_at) = started_modified_at else {
        return false;
    };
    executable_modified_ms(path).is_some_and(|current| current > started_modified_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_binary_changed_detects_newer_executable_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rebon-test-exe");
        fs::write(&path, "one").unwrap();
        let modified = executable_modified_ms(&path).expect("mtime");

        assert!(supervisor_binary_changed(
            &path,
            Some(modified.saturating_sub(1))
        ));
        assert!(!supervisor_binary_changed(
            &path,
            Some(modified.saturating_add(1))
        ));
        assert!(!supervisor_binary_changed(&path, None));
    }
}
