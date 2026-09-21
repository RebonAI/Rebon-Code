//! One lock for the tests that rewrite `HOME` / `USERPROFILE` /
//! `REBON_CONFIG_DIR`. Those variables are process-global, so two tests that
//! set them concurrently read each other's home directory.

pub(crate) fn env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}
