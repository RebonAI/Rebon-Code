//! The Windows Filtering Platform filters.
//!
//! Four filters, installed once under administrator with
//! `FWPM_FILTER_FLAG_PERSISTENT` so they survive a reboot: block and
//! permit-loopback, each at the IPv4 and IPv6 connect-authorization layers. All
//! four are keyed on the SID of
//! [`crate::core::account::SANDBOX_USER_NO_NETWORK`] — the account
//! `--block-network` selects.
//!
//! ## The condition list is the user-facing promise
//!
//! The caller's remediation text tells the user, in words it froze:
//!
//! > No sign-out is needed, and your own network is unaffected: the filters are
//! > keyed to the sandbox user's SID.
//!
//! The natural reading of "install a network filter" is "this changes my machine's
//! networking", and a user who believes that declines. That sentence is the only
//! reason to press the button, so it cannot be a pleasantry — which is why
//! [`FilterCondition`] has exactly two variants and [`plan_filters`] is tested to
//! put the user SID on every single filter. There is deliberately no way to
//! express a condition on an image path, a port, or a protocol: the type system is
//! doing the promising.
//!
//! ## What this cannot do
//!
//! Only `connect` is authorized. Established connections, raw sockets, and
//! anything the kernel originates are outside it. DNS resolution goes through the
//! `dnscache` service under its own identity, so it does not match our SID
//! condition and names still resolve inside the sandbox; the domain decision
//! belongs to the proxy, exactly as on Linux.

/// The name of the helper's own sublayer.
pub const SUBLAYER_NAME: &str = "sandbox-win";

/// The name of the WFP provider every object the helper creates is tagged
/// with.
pub const PROVIDER_NAME: &str = "sandbox-win (Rebon sandbox helper)";

/// A stable GUID for that sublayer, as a byte array so the sys layer can build a
/// `GUID` without this crate knowing what one is.
///
/// Fixed rather than generated: `uninstall` and a re-`install` have to find the
/// same sublayer, and a filter left behind under a forgotten key is exactly the
/// residue the ledger exists to prevent.
pub const SUBLAYER_KEY: [u8; 16] = [
    0x9d, 0x4a, 0x1c, 0x6b, 0x2f, 0x83, 0x47, 0xe1, 0xa5, 0x7c, 0x0b, 0x11, 0x64, 0xd2, 0x38, 0x5e,
];

/// A stable GUID for the provider.
///
/// A provider is how WFP lets a piece of software claim its own objects. Every
/// filter the helper installs carries this key, which makes two otherwise awkward
/// operations trivial and exact: `uninstall` deletes what belongs to us and
/// nothing else, and `status` can *count* our filters rather than inferring from
/// the sublayer's existence that any were ever added. Without it, "the sublayer is
/// there" would have to stand in for "the filters are working" — the substitution
/// a future `selftest` subcommand would exist to catch.
pub const PROVIDER_KEY: [u8; 16] = [
    0x9d, 0x4a, 0x1c, 0x6b, 0x2f, 0x83, 0x47, 0xe1, 0xa5, 0x7c, 0x0b, 0x11, 0x64, 0xd2, 0x38, 0x5f,
];

/// The layers a filter is installed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WfpLayer {
    /// `FWPM_LAYER_ALE_AUTH_CONNECT_V4`
    AuthConnectV4,
    /// `FWPM_LAYER_ALE_AUTH_CONNECT_V6`
    AuthConnectV6,
}

impl WfpLayer {
    pub const ALL: [WfpLayer; 2] = [WfpLayer::AuthConnectV4, WfpLayer::AuthConnectV6];

    pub const fn suffix(self) -> &'static str {
        match self {
            WfpLayer::AuthConnectV4 => "v4",
            WfpLayer::AuthConnectV6 => "v6",
        }
    }
}

/// What a filter does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    Block,
    Permit,
}

/// The only two things a filter here may be conditioned on.
///
/// Adding a variant would be a change to the promise made to the user, not a
/// refactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterCondition {
    /// `FWPM_CONDITION_ALE_USER_ID` is set to this SID.
    UserSid(String),
    /// `FWPM_CONDITION_IP_REMOTE_ADDRESS` is the loopback range.
    RemoteAddressLoopback,
}

/// Weight of the blocking filters. Lower than [`PERMIT_WEIGHT`] so the
/// loopback permit is evaluated first and wins.
pub const BLOCK_WEIGHT: u64 = 100;

/// Weight of the loopback permits.
pub const PERMIT_WEIGHT: u64 = 200;

/// If the block ever outweighs the permit, a `--block-network` command loses
/// its route to Rebon's loopback proxy and the network sandbox becomes a
/// total blackout instead of a filtered one. Pinned at compile time because
/// the two constants are edited independently.
const _: () = assert!(PERMIT_WEIGHT > BLOCK_WEIGHT);

/// One filter to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFilter {
    /// A stable GUID for this filter, as bytes. See [`filter_key`].
    pub key: [u8; 16],
    pub name: String,
    pub layer: WfpLayer,
    pub action: FilterAction,
    pub weight: u64,
    pub conditions: Vec<FilterCondition>,
}

/// A stable GUID for the filter at `layer` doing `action`.
///
/// Letting WFP generate the key would mean a second `install` on a machine
/// that already has these filters adds four *more* of them: an auto-keyed add
/// never collides, so `FWP_E_ALREADY_EXISTS` never fires and the "install is
/// idempotent" promise is false the first time somebody re-runs it. With a
/// fixed key the second run is a no-op, which is what the word means.
///
/// It is also what makes the read grant possible at all: WFP addresses a
/// filter's security descriptor by key, so a filter whose key nobody kept
/// cannot be granted on afterwards.
///
/// Derived from [`SUBLAYER_KEY`] by its last two bytes so the family is
/// visibly one family in a `netsh wfp show filters` dump, where these sit
/// among a few hundred of Windows' own.
pub const fn filter_key(layer: WfpLayer, action: FilterAction) -> [u8; 16] {
    let mut key = SUBLAYER_KEY;
    key[14] = match layer {
        WfpLayer::AuthConnectV4 => 0x40,
        WfpLayer::AuthConnectV6 => 0x60,
    };
    key[15] = match action {
        FilterAction::Block => 0xb0,
        FilterAction::Permit => 0xe0,
    };
    key
}

/// The filter name for a blocking filter.
pub fn block_filter_name(sid: &str, layer: WfpLayer) -> String {
    format!("{SUBLAYER_NAME}-{sid}-block-user-{}", layer.suffix())
}

/// The filter name for a loopback permit.
pub fn permit_loopback_filter_name(sid: &str, layer: WfpLayer) -> String {
    format!(
        "{SUBLAYER_NAME}-{sid}-permit-loopback-user-{}",
        layer.suffix()
    )
}

/// Whether a filter name belongs to this helper.
///
/// `uninstall` and `reap` walk the engine's filter list and delete what they
/// recognise. Recognising too much would delete somebody else's filter;
/// recognising too little leaves the user's machine filtered by software they
/// have removed.
pub fn is_ours(name: &str) -> bool {
    name.starts_with(&format!("{SUBLAYER_NAME}-"))
        && (name.contains("-block-user-") || name.contains("-permit-loopback-user-"))
}

/// How many filters a finished `install` leaves behind.
///
/// Two layers times two actions. Named so `status` can require the whole set
/// rather than "at least one": three of four is not a lesser version of
/// working. A missing block leaves that address family unfiltered — the
/// sandbox reaches the internet over IPv6 while reporting itself confined —
/// and a missing permit cuts it off from the loopback proxy entirely.
pub const EXPECTED_FILTERS: usize = 4;

/// Every filter `install` writes, for the no-network account's SID.
pub fn plan_filters(no_network_account_sid: &str) -> Vec<PlannedFilter> {
    let mut filters = Vec::with_capacity(4);
    for layer in WfpLayer::ALL {
        filters.push(PlannedFilter {
            key: filter_key(layer, FilterAction::Block),
            name: block_filter_name(no_network_account_sid, layer),
            layer,
            action: FilterAction::Block,
            weight: BLOCK_WEIGHT,
            conditions: vec![FilterCondition::UserSid(no_network_account_sid.to_string())],
        });
        // Loopback stays open so a confined command can still reach Rebon's
        // local proxy — the same shape as the Linux backend, where the
        // domain decision is made in the proxy rather than the kernel.
        filters.push(PlannedFilter {
            key: filter_key(layer, FilterAction::Permit),
            name: permit_loopback_filter_name(no_network_account_sid, layer),
            layer,
            action: FilterAction::Permit,
            weight: PERMIT_WEIGHT,
            conditions: vec![
                FilterCondition::UserSid(no_network_account_sid.to_string()),
                FilterCondition::RemoteAddressLoopback,
            ],
        });
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";
    const OTHER_SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    #[test]
    fn the_plan_is_four_filters_two_layers_two_actions() {
        let filters = plan_filters(SID);
        assert_eq!(filters.len(), 4);
        assert_eq!(
            filters
                .iter()
                .filter(|f| f.layer == WfpLayer::AuthConnectV4)
                .count(),
            2
        );
        assert_eq!(
            filters
                .iter()
                .filter(|f| f.action == FilterAction::Permit)
                .count(),
            2
        );
    }

    #[test]
    fn every_filter_is_keyed_on_the_sandbox_accounts_sid() {
        // This is the test behind the install prompt's promise. If a filter
        // could ever be added without a user-SID condition, it would match
        // the user's own traffic and the sentence they agreed to would be
        // false.
        for filter in plan_filters(SID) {
            assert!(
                filter
                    .conditions
                    .iter()
                    .any(|c| matches!(c, FilterCondition::UserSid(sid) if sid == SID)),
                "{} has no user SID condition",
                filter.name
            );
        }
    }

    #[test]
    fn no_filter_is_unconditional() {
        // A WFP filter with an empty condition list matches *everything* at
        // its layer — every process on the machine, the user's own included.
        // That is the one shape that would make the install prompt a lie, and
        // it is a single deleted line away at all times.
        for filter in plan_filters(SID) {
            assert!(
                !filter.conditions.is_empty(),
                "{} would match the whole machine",
                filter.name
            );
            for condition in &filter.conditions {
                // Exhaustive on purpose: adding a variant to
                // `FilterCondition` breaks this build, which is the point.
                match condition {
                    FilterCondition::UserSid(_) | FilterCondition::RemoteAddressLoopback => {}
                }
            }
        }
    }

    #[test]
    fn the_loopback_permit_outweighs_the_block() {
        // Same layer, same SID, opposite actions: whichever weighs more is
        // the one that decides, and the sandbox has to keep reaching the
        // local proxy.
        let filters = plan_filters(SID);
        let block = filters
            .iter()
            .find(|f| f.action == FilterAction::Block)
            .unwrap();
        let permit = filters
            .iter()
            .find(|f| f.action == FilterAction::Permit)
            .unwrap();
        assert!(permit.weight > block.weight);
    }

    #[test]
    fn only_the_permit_carries_the_loopback_condition() {
        for filter in plan_filters(SID) {
            let loopback = filter
                .conditions
                .contains(&FilterCondition::RemoteAddressLoopback);
            assert_eq!(loopback, filter.action == FilterAction::Permit);
        }
    }

    #[test]
    fn names_carry_the_sid_so_two_installs_cannot_collide() {
        let mine = plan_filters(SID);
        let theirs = plan_filters(OTHER_SID);
        for filter in &mine {
            assert!(filter.name.contains(SID));
            assert!(!theirs.iter().any(|other| other.name == filter.name));
        }
    }

    #[test]
    fn names_are_unique_within_one_plan() {
        let mut names: Vec<String> = plan_filters(SID).into_iter().map(|f| f.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn ownership_recognition_covers_the_plan_and_nothing_else() {
        for filter in plan_filters(SID) {
            assert!(is_ours(&filter.name), "{}", filter.name);
        }
        assert!(!is_ours("Windows Defender Firewall block"));
        assert!(!is_ours("sandbox-win"));
        assert!(!is_ours("sandbox-win-something-else"));
        assert!(!is_ours(""));
    }

    #[test]
    fn every_planned_filter_has_its_own_key() {
        // Two filters sharing a key means the second add fails as a
        // duplicate and one of the four is silently never installed — which
        // for the loopback permit is a total network blackout rather than a
        // filtered one.
        let filters = plan_filters("S-1-5-21-1-2-3-1001");
        let mut keys: Vec<[u8; 16]> = filters.iter().map(|filter| filter.key).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }

    #[test]
    fn no_filter_key_collides_with_the_provider_or_sublayer() {
        for filter in plan_filters("S-1-5-21-1-2-3-1001") {
            assert_ne!(filter.key, PROVIDER_KEY);
            assert_ne!(filter.key, SUBLAYER_KEY);
            assert_ne!(filter.key, [0u8; 16]);
        }
    }

    #[test]
    fn the_filter_keys_do_not_depend_on_the_sid() {
        // `uninstall` on a machine whose sandbox account was recreated has to
        // find the filters the previous account's install left behind. Keys
        // that varied with the SID would strand them.
        let first = plan_filters("S-1-5-21-1-2-3-1001");
        let second = plan_filters("S-1-5-21-9-9-9-4242");
        let first_keys: Vec<_> = first.iter().map(|filter| filter.key).collect();
        let second_keys: Vec<_> = second.iter().map(|filter| filter.key).collect();
        assert_eq!(first_keys, second_keys);
    }

    #[test]
    fn the_sublayer_key_is_fixed_so_uninstall_can_find_it() {
        assert_eq!(SUBLAYER_KEY.len(), 16);
        assert_ne!(SUBLAYER_KEY, [0u8; 16]);
    }

    #[test]
    fn the_provider_and_sublayer_are_different_objects() {
        // Reusing one GUID for both would make `FwpmProviderGetByKey0` and
        // `FwpmSubLayerGetByKey0` answer about each other's absence, so
        // `status` would report the network layer installed on the strength
        // of whichever happened to exist.
        assert_ne!(PROVIDER_KEY, SUBLAYER_KEY);
        assert_ne!(PROVIDER_KEY, [0u8; 16]);
    }

    #[test]
    fn the_provider_name_says_who_it_belongs_to() {
        // It shows up in `netsh wfp show filters` on a user's machine. A bare
        // "sandbox-win" there is something they would reasonably investigate
        // as malware.
        assert!(PROVIDER_NAME.contains("Rebon"), "{PROVIDER_NAME}");
        assert!(PROVIDER_NAME.contains(SUBLAYER_NAME), "{PROVIDER_NAME}");
    }
}
