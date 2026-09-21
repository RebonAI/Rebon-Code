//! Probing the Windows Filtering Platform.
//!
//! `status` has to answer `wfp=ok` or not, and it has to do so **without
//! elevating**. Whether those two are compatible was an open question — opening
//! the filter engine takes `FWPM_ACTRL_OPEN`, and the obvious guess is that only
//! administrators have it.
//!
//! Measured on Windows 10 19044 from an ordinary, non-elevated account:
//! `FwpmEngineOpen0` **succeeds**, and on a clean machine `FwpmProviderGetByKey0`
//! returns `FWP_E_PROVIDER_NOT_FOUND`. That reads as "no grant on the `install`
//! side is needed", and it is wrong twice over, both visible only once an object
//! existed to be denied — a probe that finds nothing never reaches an access
//! check:
//!
//! 1. Reading an object that *does* exist is `ERROR_ACCESS_DENIED` unless
//!    `install` granted it. `install` now does (see `wfp_install`).
//! 2. *Enumerating* filters is a right on the filters container, which is
//!    machine-wide and not ours to widen. So this module does not enumerate: it
//!    asks for each of the four filters it installed **by key**, which needs only
//!    the per-object grant. That is also a stricter check than a count — it
//!    verifies the exact set is present, not that some number of filters carry
//!    our provider key.
//!
//! [`WfpProbe::Unreadable`] stays because a locked-down machine may still refuse,
//! and because the distinction below matters either way.
//!
//! **`Unreadable` renders as missing, never as ok.** A probe that could not look
//! is not evidence that the filters are there, and "we assumed it was fine" is
//! how a sandbox reports success while enforcing nothing.
//!
//! ## Why existence of the sublayer is not enough
//!
//! A sublayer with no filters in it blocks nothing. That is the classic
//! "installed but not biting" failure — `bwrap` exists and is executable on a
//! machine with user namespaces disabled, and every `unshare` fails. So
//! [`WfpProbe::Installed`] requires the provider, the sublayer, **and** at least
//! one filter tagged with our provider.

use crate::sys::SysResult;

/// What the filter engine says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WfpProbe {
    /// Provider, sublayer, and all
    /// [`EXPECTED_FILTERS`](crate::core::wfp::EXPECTED_FILTERS) filters
    /// are present.
    Installed { filters: usize },
    /// Some of the filters are there and some are not.
    ///
    /// Not `Installed`: a missing block leaves one address family unfiltered
    /// while `status` says the sandbox is confined, and a missing loopback
    /// permit cuts it off from the proxy. Not plain `Missing` either — the
    /// remedy is the same (`install` is idempotent and re-adds what is
    /// absent) but the sentence a person needs to read is different.
    Partial { filters: usize },
    /// The engine was readable and our objects are not there.
    Missing,
    /// The engine could not be read at all. Reported as missing by `status`,
    /// but kept distinct because the remedy is not "run install" — it is a
    /// machine policy or a stopped Base Filtering Engine service, and telling
    /// someone to reinstall would send them to fix the wrong thing.
    Unreadable { detail: String },
}

impl WfpProbe {
    /// The `wfp=ok` probe. Only [`WfpProbe::Installed`] qualifies.
    pub fn is_installed(&self) -> bool {
        matches!(self, WfpProbe::Installed { .. })
    }
}

/// Ask the filter engine whether the helper's filters are installed.
pub fn probe() -> WfpProbe {
    match imp::probe() {
        Ok(probe) => probe,
        Err(error) => WfpProbe::Unreadable {
            detail: error.to_string(),
        },
    }
}

/// The un-collapsed form of [`probe`]: the engine's answer as a `SysResult`,
/// with the reason kept instead of folded into [`WfpProbe::Unreadable`].
pub fn probe_detailed() -> SysResult<WfpProbe> {
    imp::probe()
}

#[cfg(windows)]
mod imp {
    use super::WfpProbe;
    use crate::core::wfp::{
        filter_key, FilterAction, WfpLayer, EXPECTED_FILTERS, PROVIDER_KEY, SUBLAYER_KEY,
    };
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
        FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterGetByKey0, FwpmFreeMemory0,
        FwpmProviderGetByKey0, FwpmSubLayerGetByKey0, FWPM_FILTER0, FWPM_PROVIDER0, FWPM_SUBLAYER0,
    };
    use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;

    /// `FWP_E_*` for "the object is not in the engine". Distinct from a
    /// failure to read: one means "not installed", the other means "we could
    /// not look", and conflating them is how a probe reports a state it never
    /// observed.
    ///
    /// The numbers are one apart and easy to transpose, which is exactly what
    /// happened here first: `PROVIDER_NOT_FOUND` written as `…06` turned an
    /// ordinary "nothing installed" into a spurious "the engine could not be
    /// read" note on every `status`.
    const FWP_E_FILTER_NOT_FOUND: u32 = 0x8032_0003;
    const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x8032_0005;
    const FWP_E_PROVIDER_CONTEXT_NOT_FOUND: u32 = 0x8032_0006;
    const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x8032_0007;

    fn guid(bytes: &[u8; 16]) -> GUID {
        GUID {
            data1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_be_bytes([bytes[4], bytes[5]]),
            data3: u16::from_be_bytes([bytes[6], bytes[7]]),
            data4: [
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ],
        }
    }

    struct Engine(HANDLE);

    impl Drop for Engine {
        fn drop(&mut self) {
            unsafe { FwpmEngineClose0(self.0) };
        }
    }

    fn open() -> SysResult<Engine> {
        let mut handle: HANDLE = 0;
        // Dynamic session, no explicit credentials: this is a read, and a
        // dynamic session guarantees nothing outlives the probe even if it
        // panics partway.
        let status =
            unsafe { FwpmEngineOpen0(null(), RPC_C_AUTHN_WINNT, null(), null(), &mut handle) };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("FwpmEngineOpen0", status));
        }
        Ok(Engine(handle))
    }

    pub fn probe() -> SysResult<WfpProbe> {
        let engine = open()?;

        let provider_key = guid(&PROVIDER_KEY);
        let mut provider: *mut FWPM_PROVIDER0 = null_mut();
        let status = unsafe { FwpmProviderGetByKey0(engine.0, &provider_key, &mut provider) };
        if status == FWP_E_PROVIDER_NOT_FOUND || status == FWP_E_PROVIDER_CONTEXT_NOT_FOUND {
            return Ok(WfpProbe::Missing);
        }
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("FwpmProviderGetByKey0", status));
        }
        unsafe { FwpmFreeMemory0(&mut (provider as *mut c_void)) };

        let sublayer_key = guid(&SUBLAYER_KEY);
        let mut sublayer: *mut FWPM_SUBLAYER0 = null_mut();
        let status = unsafe { FwpmSubLayerGetByKey0(engine.0, &sublayer_key, &mut sublayer) };
        if status == FWP_E_SUBLAYER_NOT_FOUND {
            return Ok(WfpProbe::Missing);
        }
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("FwpmSubLayerGetByKey0", status));
        }
        unsafe { FwpmFreeMemory0(&mut (sublayer as *mut c_void)) };

        // The objects exist. Whether they *do* anything is the filter count —
        // a sublayer with nothing in it is the "installed but not biting"
        // shape, and reporting it as ok would be exactly the failure the
        // status contract exists to make visible.
        let filters = count_expected_filters(&engine)?;
        if filters == 0 {
            return Ok(WfpProbe::Missing);
        }
        if filters < EXPECTED_FILTERS {
            return Ok(WfpProbe::Partial { filters });
        }
        Ok(WfpProbe::Installed { filters })
    }

    /// How many of the four filters `install` writes are present.
    ///
    /// Asked one key at a time rather than enumerated. Enumeration reads
    /// nicer and cannot be done here: `FwpmFilterCreateEnumHandle0` takes
    /// `FWPM_ACTRL_ENUM` on the filters *container*, which is one ACL for
    /// every filter on the machine. Granting a helper's users enumerate over
    /// all of WFP to let `status` count four objects is not a trade this
    /// crate gets to make, and the install prompt's promise that the user's
    /// own networking is untouched would stop being true.
    ///
    /// Get-by-key needs only read on the object itself, which `install`
    /// grants. This is why the filter keys are fixed in
    /// [`crate::core::wfp::filter_key`] instead of generated.
    ///
    /// A partial count is reported as it is found. Four means installed;
    /// fewer than four is the "installed but not biting" shape and the
    /// caller decides what to do with it — silently rounding three up to
    /// "ok" is what the status contract exists to prevent.
    fn count_expected_filters(engine: &Engine) -> SysResult<usize> {
        let mut present = 0usize;
        for layer in WfpLayer::ALL {
            for action in [FilterAction::Block, FilterAction::Permit] {
                let key = guid(&filter_key(layer, action));
                let mut filter: *mut FWPM_FILTER0 = null_mut();
                let status = unsafe { FwpmFilterGetByKey0(engine.0, &key, &mut filter) };
                match status {
                    ERROR_SUCCESS => {
                        present += 1;
                        unsafe { FwpmFreeMemory0(&mut (filter as *mut c_void)) };
                    }
                    FWP_E_FILTER_NOT_FOUND => {}
                    other => return Err(SysError::win32("FwpmFilterGetByKey0", other)),
                }
            }
        }
        Ok(present)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::WfpProbe;
    use crate::sys::{SysError, SysResult};

    pub fn probe() -> SysResult<WfpProbe> {
        Err(SysError::Unsupported("the Windows Filtering Platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_installed_counts_as_ok() {
        // The rule the whole module exists to enforce: a probe that could not
        // read the engine is not evidence that the filters are there.
        assert!(WfpProbe::Installed { filters: 4 }.is_installed());
        assert!(!WfpProbe::Missing.is_installed());
        assert!(!WfpProbe::Unreadable {
            detail: "access denied".into()
        }
        .is_installed());
    }

    #[test]
    fn a_probe_never_panics_and_never_claims_more_than_it_saw() {
        // On a non-elevated Windows box this comes back `Unreadable`; on a
        // non-Windows one it also comes back `Unreadable`. Either way it must
        // return rather than fail, because `status` has to exit zero.
        let probe = probe();
        if let WfpProbe::Unreadable { detail } = &probe {
            assert!(!detail.is_empty());
        }
        assert!(
            !probe.is_installed()
                || matches!(probe, WfpProbe::Installed { filters } if filters > 0)
        );
    }
}
