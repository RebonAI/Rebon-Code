//! Installing and removing the network filters.
//!
//! Four filters, one sublayer, one provider, all `PERSISTENT` so they survive a
//! reboot and `install` runs once rather than at every start.
//!
//! ## The promise this code has to keep
//!
//! The install prompt tells the user, in words the caller froze:
//!
//! > No sign-out is needed, and your own network is unaffected: the filters are
//! > keyed to the sandbox user's SID.
//!
//! Every filter here therefore carries an `ALE_USER_ID` condition naming the
//! no-network account and nothing else — no image path, no port, no protocol.
//! [`crate::core::wfp::plan_filters`] is where that is enforced as a type (its
//! condition enum has two variants and one of them is the SID), and this module
//! is the transcription. A user who believes "installing a network filter will
//! change my machine's networking" declines, and they would be right to; the
//! sentence is the only reason to press the button, so it cannot be a pleasantry.
//!
//! ## All or nothing
//!
//! Everything goes inside a WFP transaction. Half a filter set is worse than
//! none: a block installed without its loopback permit cuts the sandbox off from
//! the proxy it is supposed to reach, and the machine reboots into that state
//! because the filters are persistent.
//!
//! ## The user condition is a security descriptor
//!
//! `FWPM_CONDITION_ALE_USER_ID` matches against an access check, not a SID
//! comparison — the value is a security descriptor and the filter matches when
//! the connecting token would be granted access by it. So the SID goes into an
//! SDDL string, and without `FWP_ACTRL_MATCH_FILTER` among the filter's access
//! rights the filter silently matches nothing.

use crate::sys::SysResult;

/// What `install` put in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterReport {
    pub provider_added: bool,
    pub sublayer_added: bool,
    pub filters_added: usize,
}

/// What `uninstall` took away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterRemoval {
    pub filters_removed: usize,
    pub sublayer_removed: bool,
    pub provider_removed: bool,
    pub notes: Vec<String>,
}

/// Install the provider, sublayer, and filters for `no_network_sid`.
pub fn install(no_network_sid: &str) -> SysResult<FilterReport> {
    imp::install(no_network_sid)
}

/// Remove everything this module installs.
pub fn uninstall() -> SysResult<FilterRemoval> {
    imp::uninstall()
}

/// The SDDL an `ALE_USER_ID` condition is expressed as.
///
/// `FWP_ACTRL_MATCH_FILTER` is `0x0001`; a descriptor without it grants the
/// token nothing and the filter matches nothing, silently. Built here as a
/// pure function so the string is testable on any platform — it is the one
/// part of this module that can be.
pub fn user_condition_sddl(sid: &str) -> String {
    format!("O:LSD:(A;;CC;;;{sid})")
}

#[cfg(windows)]
mod imp {
    use super::{user_condition_sddl, FilterRemoval, FilterReport};
    use crate::core::wfp::{
        plan_filters, FilterAction, FilterCondition, WfpLayer, PROVIDER_KEY, PROVIDER_NAME,
        SUBLAYER_KEY, SUBLAYER_NAME,
    };
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
        SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;

    const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x8032_0005;
    const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x8032_0007;
    /// `FWP_E_ALREADY_EXISTS`
    const FWP_E_ALREADY_EXISTS: u32 = 0x8032_0009;

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

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

    fn layer_key(layer: WfpLayer) -> GUID {
        match layer {
            WfpLayer::AuthConnectV4 => FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            WfpLayer::AuthConnectV6 => FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        }
    }

    struct Engine(HANDLE);

    impl Engine {
        /// A **non-dynamic** session: dynamic objects are deleted when the
        /// session closes, which for persistent filters means they would
        /// vanish the moment `install` exits.
        fn open() -> SysResult<Self> {
            let mut handle: HANDLE = 0;
            let status =
                unsafe { FwpmEngineOpen0(null(), RPC_C_AUTHN_WINNT, null(), null(), &mut handle) };
            if status != ERROR_SUCCESS {
                return Err(SysError::win32("FwpmEngineOpen0", status));
            }
            Ok(Self(handle))
        }
    }

    impl Drop for Engine {
        fn drop(&mut self) {
            unsafe { FwpmEngineClose0(self.0) };
        }
    }

    /// A security descriptor from SDDL, freed on drop.
    ///
    /// The size is not decoration: `FWP_SECURITY_DESCRIPTOR_TYPE`'s union
    /// member is a `FWP_BYTE_BLOB`, not a bare descriptor pointer, so the
    /// descriptor has to be wrapped with its length before WFP will read it.
    /// Handing it the raw pointer type-checks and produces a filter built
    /// from whatever follows the descriptor in memory.
    struct Descriptor(*mut c_void, u32);

    impl Descriptor {
        /// The blob WFP actually wants. Borrowed, so it lives exactly as
        /// long as the descriptor it points into.
        fn blob(&self) -> FWP_BYTE_BLOB {
            FWP_BYTE_BLOB {
                size: self.1,
                data: self.0 as *mut u8,
            }
        }

        fn from_sddl(sddl: &str) -> SysResult<Self> {
            let mut descriptor: *mut c_void = null_mut();
            let mut size: u32 = 0;
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    1, // SDDL_REVISION_1
                    &mut descriptor,
                    &mut size,
                )
            } == 0
            {
                return Err(SysError::win32(
                    "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                    unsafe { windows_sys::Win32::Foundation::GetLastError() },
                ));
            }
            Ok(Self(descriptor, size))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    /// WFP object rights. `READ` is get-by-key, `ENUM` is enumeration —
    /// `status` needs both and nothing more.
    const FWPM_ACTRL_ENUM: u32 = 0x0000_0020;
    const FWPM_ACTRL_READ: u32 = 0x0000_0080;

    /// Let ordinary users *read* the objects this install created.
    ///
    /// `status` has to run without elevation, and a WFP object that exists cannot be
    /// read by a non-admin without this. Measuring `FwpmEngineOpen0` succeeding
    /// non-elevated suggested no grant was needed — that was wrong, and only visible
    /// once an object actually existed to be denied: on a clean machine the probe
    /// returns "not found" long before it reaches an access check.
    ///
    /// Without it, `status` reports `wfp=missing` on a correctly installed machine,
    /// `is_ready()` is false, and the Windows sandbox refuses to run at all. That is
    /// a worse failure than the one it would be reporting.
    ///
    /// **Added to the existing DACL, never replacing it.** Replacing would mean
    /// guessing at the rights administrators and SYSTEM already hold on an object
    /// that only they can delete — and getting that wrong creates filters nobody can
    /// remove. Read-modify-write is the same shape as the file-ACL layer, for the
    /// same reason.
    ///
    /// Read-only, and only what already appears in the helper's own documentation:
    /// that these filters exist, and which SID they name.
    /// Which of the three object kinds a grant is for.
    ///
    /// The three WFP calls have identical shapes and different names, so this
    /// exists only to keep the read-modify-write below written once — three
    /// copies would be three places for the "merge, never replace" rule to
    /// drift.
    #[derive(Clone, Copy)]
    enum WfpObject {
        Provider,
        SubLayer,
        Filter,
    }

    impl WfpObject {
        fn get(self) -> &'static str {
            match self {
                Self::Provider => "FwpmProviderGetSecurityInfoByKey0",
                Self::SubLayer => "FwpmSubLayerGetSecurityInfoByKey0",
                Self::Filter => "FwpmFilterGetSecurityInfoByKey0",
            }
        }

        fn set(self) -> &'static str {
            match self {
                Self::Provider => "FwpmProviderSetSecurityInfoByKey0",
                Self::SubLayer => "FwpmSubLayerSetSecurityInfoByKey0",
                Self::Filter => "FwpmFilterSetSecurityInfoByKey0",
            }
        }
    }

    fn grant_read(engine: &Engine, key: &GUID, object: WfpObject) -> SysResult<()> {
        let mut existing: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let status = unsafe {
            match object {
                WfpObject::Provider => FwpmProviderGetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut existing,
                    null_mut(),
                    &mut descriptor,
                ),
                WfpObject::SubLayer => FwpmSubLayerGetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut existing,
                    null_mut(),
                    &mut descriptor,
                ),
                WfpObject::Filter => FwpmFilterGetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut existing,
                    null_mut(),
                    &mut descriptor,
                ),
            }
        };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32(object.get(), status));
        }

        let users = OwnedSid::parse(USERS_SID)?;
        let mut entry: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        entry.grfAccessPermissions = FWPM_ACTRL_READ | FWPM_ACTRL_ENUM;
        entry.grfAccessMode = GRANT_ACCESS;
        entry.grfInheritance = 0;
        entry.Trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            ptstrName: users.0 as *mut u16,
        };

        let mut merged: *mut ACL = null_mut();
        // `GRANT_ACCESS` rather than `SET_ACCESS`: it adds to what is there
        // instead of replacing that trustee's entry.
        let result = unsafe { SetEntriesInAclW(1, &entry, existing, &mut merged) };
        if !descriptor.is_null() {
            unsafe { FwpmFreeMemory0(&mut (descriptor as *mut c_void)) };
        }
        if result != ERROR_SUCCESS {
            return Err(SysError::win32("SetEntriesInAclW", result));
        }

        let status = unsafe {
            match object {
                WfpObject::Provider => FwpmProviderSetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null(),
                    null(),
                    merged,
                    null(),
                ),
                WfpObject::SubLayer => FwpmSubLayerSetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null(),
                    null(),
                    merged,
                    null(),
                ),
                WfpObject::Filter => FwpmFilterSetSecurityInfoByKey0(
                    engine.0,
                    key,
                    DACL_SECURITY_INFORMATION,
                    null(),
                    null(),
                    merged,
                    null(),
                ),
            }
        };
        unsafe { LocalFree(merged as *mut c_void) };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32(object.set(), status));
        }
        Ok(())
    }

    /// `BUILTIN\Users`.
    const USERS_SID: &str = "S-1-5-32-545";

    /// An owned SID, freed on drop.
    struct OwnedSid(*mut c_void);

    impl OwnedSid {
        fn parse(text: &str) -> SysResult<Self> {
            let mut sid: *mut c_void = null_mut();
            if unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) } == 0 {
                return Err(SysError::win32("ConvertStringSidToSidW", unsafe {
                    windows_sys::Win32::Foundation::GetLastError()
                }));
            }
            Ok(Self(sid))
        }
    }

    impl Drop for OwnedSid {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    pub fn install(no_network_sid: &str) -> SysResult<FilterReport> {
        let engine = Engine::open()?;
        let mut report = FilterReport::default();

        let status = unsafe { FwpmTransactionBegin0(engine.0, 0) };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("FwpmTransactionBegin0", status));
        }

        // Anything that fails from here rolls the whole set back. Half a
        // filter set survives a reboot and cuts the sandbox off from the
        // proxy it is supposed to reach.
        match install_objects(&engine, no_network_sid, &mut report) {
            Ok(()) => {
                let status = unsafe { FwpmTransactionCommit0(engine.0) };
                if status != ERROR_SUCCESS {
                    return Err(SysError::win32("FwpmTransactionCommit0", status));
                }
                // After the commit: security info is not part of the
                // transaction, and the objects have to exist to be granted
                // on. `status` reads all three kinds, so all three are
                // granted; a provider readable but a sublayer that is not
                // fails one step later and reports the same `wfp=missing`.
                grant_read(&engine, &guid(&PROVIDER_KEY), WfpObject::Provider)?;
                grant_read(&engine, &guid(&SUBLAYER_KEY), WfpObject::SubLayer)?;
                for planned in plan_filters(no_network_sid) {
                    grant_read(&engine, &guid(&planned.key), WfpObject::Filter)?;
                }
                Ok(report)
            }
            Err(error) => {
                unsafe { FwpmTransactionAbort0(engine.0) };
                Err(error)
            }
        }
    }

    fn install_objects(
        engine: &Engine,
        no_network_sid: &str,
        report: &mut FilterReport,
    ) -> SysResult<()> {
        let provider_key = guid(&PROVIDER_KEY);
        let sublayer_key = guid(&SUBLAYER_KEY);

        let mut provider_name = wide(PROVIDER_NAME);
        let mut provider_description =
            wide("Filters outbound connections for the Rebon sandbox's no-network account only.");
        let mut provider: FWPM_PROVIDER0 = unsafe { std::mem::zeroed() };
        provider.providerKey = provider_key;
        provider.displayData.name = provider_name.as_mut_ptr();
        provider.displayData.description = provider_description.as_mut_ptr();
        provider.flags = FWPM_PROVIDER_FLAG_PERSISTENT;
        let status = unsafe { FwpmProviderAdd0(engine.0, &provider, null_mut()) };
        match status {
            ERROR_SUCCESS => report.provider_added = true,
            FWP_E_ALREADY_EXISTS => {}
            other => return Err(SysError::win32("FwpmProviderAdd0", other)),
        }

        let mut sublayer_name = wide(SUBLAYER_NAME);
        let mut sublayer: FWPM_SUBLAYER0 = unsafe { std::mem::zeroed() };
        sublayer.subLayerKey = sublayer_key;
        sublayer.displayData.name = sublayer_name.as_mut_ptr();
        sublayer.providerKey = &provider_key as *const GUID as *mut GUID;
        sublayer.flags = FWPM_SUBLAYER_FLAG_PERSISTENT;
        sublayer.weight = 0x8000;
        let status = unsafe { FwpmSubLayerAdd0(engine.0, &sublayer, null_mut()) };
        match status {
            ERROR_SUCCESS => report.sublayer_added = true,
            FWP_E_ALREADY_EXISTS => {}
            other => return Err(SysError::win32("FwpmSubLayerAdd0", other)),
        }

        // Both have to outlive every filter that points at them.
        let user_descriptor = Descriptor::from_sddl(&user_condition_sddl(no_network_sid))?;
        let mut user_blob = user_descriptor.blob();
        // `::1`, for the v6 loopback permit. Declared out here so it outlives
        // every filter that points at it.
        let mut loopback_v6 = FWP_BYTE_ARRAY16 {
            byteArray16: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        };

        for planned in plan_filters(no_network_sid) {
            let mut conditions: Vec<FWPM_FILTER_CONDITION0> = Vec::new();
            for condition in &planned.conditions {
                let mut entry: FWPM_FILTER_CONDITION0 = unsafe { std::mem::zeroed() };
                match condition {
                    FilterCondition::UserSid(_) => {
                        entry.fieldKey = FWPM_CONDITION_ALE_USER_ID;
                        entry.matchType = FWP_MATCH_EQUAL;
                        entry.conditionValue.r#type = FWP_SECURITY_DESCRIPTOR_TYPE;
                        entry.conditionValue.Anonymous.sd = &mut user_blob;
                    }
                    FilterCondition::RemoteAddressLoopback => {
                        entry.fieldKey = FWPM_CONDITION_IP_REMOTE_ADDRESS;
                        entry.matchType = FWP_MATCH_EQUAL;
                        // "Loopback" is one idea with two encodings, and the
                        // layer decides which. A v4 address is a `UINT32`; a
                        // v6 address is a sixteen-byte array, and giving the
                        // v6 layer a `UINT32` is `FWP_E_TYPE_MISMATCH` —
                        // measured, on the first real `install`.
                        //
                        // The plan says "loopback" and stays layer-agnostic
                        // (`crate::core::wfp`); knowing what that means
                        // at each layer is this module's job.
                        match planned.layer {
                            WfpLayer::AuthConnectV4 => {
                                entry.conditionValue.r#type = FWP_UINT32;
                                // 127.0.0.1 in host byte order, which is what
                                // WFP wants for a v4 address value. Exact
                                // rather than 127.0.0.0/8 on purpose: the
                                // proxy binds 127.0.0.1, and a permit no
                                // wider than it needs to be is the one to
                                // pick.
                                entry.conditionValue.Anonymous.uint32 = 0x7F00_0001;
                            }
                            WfpLayer::AuthConnectV6 => {
                                entry.conditionValue.r#type = FWP_BYTE_ARRAY16_TYPE;
                                entry.conditionValue.Anonymous.byteArray16 = &mut loopback_v6;
                            }
                        }
                    }
                }
                conditions.push(entry);
            }

            let mut name = wide(&planned.name);
            let mut filter: FWPM_FILTER0 = unsafe { std::mem::zeroed() };
            // A fixed key, not one WFP generates. An auto-keyed add never
            // collides, so re-running `install` would add four *more*
            // filters rather than being the no-op it is documented to be —
            // and a filter whose key nobody kept cannot have its security
            // descriptor set afterwards, which is what `status` needs.
            filter.filterKey = guid(&planned.key);
            filter.displayData.name = name.as_mut_ptr();
            filter.layerKey = layer_key(planned.layer);
            filter.subLayerKey = sublayer_key;
            filter.providerKey = &provider_key as *const GUID as *mut GUID;
            filter.flags = FWPM_FILTER_FLAG_PERSISTENT;
            filter.action.r#type = match planned.action {
                FilterAction::Block => FWP_ACTION_BLOCK,
                FilterAction::Permit => FWP_ACTION_PERMIT,
            };
            filter.weight.r#type = FWP_UINT64;
            let weight = planned.weight;
            filter.weight.Anonymous.uint64 = &weight as *const u64 as *mut u64;
            filter.numFilterConditions = conditions.len() as u32;
            filter.filterCondition = conditions.as_mut_ptr();

            let status = unsafe { FwpmFilterAdd0(engine.0, &filter, null_mut(), null_mut()) };
            match status {
                ERROR_SUCCESS => report.filters_added += 1,
                FWP_E_ALREADY_EXISTS => {}
                other => return Err(SysError::win32("FwpmFilterAdd0", other)),
            }
        }

        Ok(())
    }

    pub fn uninstall() -> SysResult<FilterRemoval> {
        let engine = Engine::open()?;
        let mut removal = FilterRemoval::default();
        let provider_key = guid(&PROVIDER_KEY);
        let sublayer_key = guid(&SUBLAYER_KEY);

        // Filters first: the sublayer and provider cannot be deleted while
        // anything still references them.
        removal.filters_removed = delete_filters(&engine, &provider_key, &mut removal.notes)?;

        let status = unsafe { FwpmSubLayerDeleteByKey0(engine.0, &sublayer_key) };
        match status {
            ERROR_SUCCESS => removal.sublayer_removed = true,
            FWP_E_SUBLAYER_NOT_FOUND => {}
            other => removal
                .notes
                .push(SysError::win32("FwpmSubLayerDeleteByKey0", other).to_string()),
        }

        let status = unsafe { FwpmProviderDeleteByKey0(engine.0, &provider_key) };
        match status {
            ERROR_SUCCESS => removal.provider_removed = true,
            FWP_E_PROVIDER_NOT_FOUND => {}
            other => removal
                .notes
                .push(SysError::win32("FwpmProviderDeleteByKey0", other).to_string()),
        }

        Ok(removal)
    }

    /// Delete every filter tagged with our provider.
    ///
    /// By provider key, never by name: the name is for humans, and matching
    /// on it would risk deleting somebody else's filter that happens to be
    /// called something similar. The provider is the claim of ownership.
    fn delete_filters(
        engine: &Engine,
        provider_key: &GUID,
        notes: &mut Vec<String>,
    ) -> SysResult<usize> {
        let mut ids: Vec<u64> = Vec::new();
        for layer in WfpLayer::ALL {
            ids.extend(filter_ids_in(engine, provider_key, layer)?);
        }

        let mut removed = 0usize;
        for id in ids {
            let status = unsafe { FwpmFilterDeleteById0(engine.0, id) };
            if status == ERROR_SUCCESS {
                removed += 1;
            } else {
                notes.push(SysError::win32("FwpmFilterDeleteById0", status).to_string());
            }
        }
        Ok(removed)
    }

    /// The ids of our filters at one layer.
    ///
    /// **One enumeration per layer, each naming its `layerKey`.** The default
    /// `enumType` is `FWP_FILTER_ENUM_FULLY_CONTAINED`, and that mode
    /// *requires* a layer: a zeroed `layerKey` comes back
    /// `FWP_E_LAYER_NOT_FOUND` rather than meaning "every layer". Measured on
    /// the first `uninstall` run, where it turned a clean machine into a
    /// reported failure.
    ///
    /// The provider key still does the ownership filtering — the layer only
    /// says where to look.
    fn filter_ids_in(engine: &Engine, provider_key: &GUID, layer: WfpLayer) -> SysResult<Vec<u64>> {
        let mut template: FWPM_FILTER_ENUM_TEMPLATE0 = unsafe { std::mem::zeroed() };
        template.providerKey = provider_key as *const GUID as *mut GUID;
        template.layerKey = layer_key(layer);
        template.actionMask = u32::MAX;

        let mut enum_handle: HANDLE = 0;
        let status = unsafe { FwpmFilterCreateEnumHandle0(engine.0, &template, &mut enum_handle) };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("FwpmFilterCreateEnumHandle0", status));
        }

        let mut ids: Vec<u64> = Vec::new();
        loop {
            let mut entries: *mut *mut FWPM_FILTER0 = null_mut();
            let mut returned = 0u32;
            let status =
                unsafe { FwpmFilterEnum0(engine.0, enum_handle, 64, &mut entries, &mut returned) };
            if status != ERROR_SUCCESS {
                unsafe { FwpmFilterDestroyEnumHandle0(engine.0, enum_handle) };
                return Err(SysError::win32("FwpmFilterEnum0", status));
            }
            for index in 0..returned as usize {
                let filter = unsafe { *entries.add(index) };
                if !filter.is_null() {
                    ids.push(unsafe { (*filter).filterId });
                }
            }
            if !entries.is_null() {
                unsafe { FwpmFreeMemory0(&mut (entries as *mut c_void)) };
            }
            if returned == 0 {
                break;
            }
        }
        unsafe { FwpmFilterDestroyEnumHandle0(engine.0, enum_handle) };
        Ok(ids)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{FilterRemoval, FilterReport};
    use crate::sys::{SysError, SysResult};

    pub fn install(_sid: &str) -> SysResult<FilterReport> {
        Err(SysError::Unsupported("the Windows Filtering Platform"))
    }

    pub fn uninstall() -> SysResult<FilterRemoval> {
        Err(SysError::Unsupported("the Windows Filtering Platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";

    #[test]
    fn the_user_condition_grants_the_match_right_to_that_sid_alone() {
        // `ALE_USER_ID` matches by access check, not by SID comparison: the
        // filter fires when the connecting token would be granted access by
        // this descriptor. A descriptor without the match right grants
        // nothing, and the filter then matches nothing — silently.
        let sddl = user_condition_sddl(SID);

        assert!(sddl.contains(SID), "{sddl}");
        assert!(sddl.contains("(A;;CC;;;"), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 1, "only that SID: {sddl}");
        assert!(!sddl.contains("(D;"), "no denials belong here: {sddl}");
    }

    #[test]
    fn the_condition_names_no_one_else() {
        // The install prompt promises the user their own traffic is
        // untouched. A second trustee here would make that false.
        let sddl = user_condition_sddl(SID);
        for other in ["WD", "BA", "AU", "S-1-1-0"] {
            assert!(!sddl.contains(other), "{other} leaked in: {sddl}");
        }
    }

    #[test]
    fn reports_start_empty() {
        assert_eq!(FilterReport::default().filters_added, 0);
        assert!(!FilterRemoval::default().provider_removed);
    }
}
