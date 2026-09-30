//! Process-global environment lock shared by tests in this binary.
//!
//! `std::env::set_var` is process-wide, not test-local: a test that swaps
//! `PATH` / `PATHEXT` to exercise command discovery makes *every* test
//! running concurrently in this binary unable to resolve `git`. Both sides
//! take this lock — the tests that mutate the environment and the tests
//! that shell out while it is intact.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::lock_env;
    use std::sync::TryLockError;

    #[test]
    fn runtime_guard_excludes_config_home_lock() {
        let _guard = lock_env();
        assert!(matches!(
            rebon_tool::env_test_lock().try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }

    #[test]
    fn runtime_guard_excludes_config_home_lock_on_another_thread() {
        let _guard = lock_env();
        assert!(std::thread::spawn(|| matches!(
            rebon_tool::env_test_lock().try_lock(),
            Err(TryLockError::WouldBlock)
        ))
        .join()
        .unwrap());
    }

    #[test]
    fn released_runtime_guard_can_be_reacquired() {
        let guard = lock_env();
        drop(guard);
        let _guard = lock_env();
        assert!(matches!(
            rebon_tool::env_test_lock().try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }
}
