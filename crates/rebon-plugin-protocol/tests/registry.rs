//! The load → ready → route → drain → unloaded progression, and every way it
//! is allowed to be refused.

use rebon_plugin_protocol::{
    CallClosed, CallIdentity, EventDelivery, EventSubscribeRequest, EventUnsubscribeRequest,
    Payload, PluginLoadRequest, PluginPhase, PluginReadyReport, PluginRegistry,
    PluginToolDefinition, PluginUnloadRequest, RegistryError, ServiceCallRequest,
    ToolInvokeRequest, PLATFORM_PLUGIN_ID,
};

fn load(plugin: &str) -> PluginLoadRequest {
    PluginLoadRequest {
        plugin_id: plugin.into(),
        root: "/packages/demo".into(),
        entry: "index.mjs".into(),
        services: vec!["compose".into(), "render".into()],
        event_topics: vec!["session".into()],
        published_topics: vec!["compose:session/append".into()],
        llm_providers: vec!["demo".into()],
        tools: vec!["grep".into()],
        commands: vec!["demo".into()],
        invokable_tools: vec!["read_file".into()],
        seats: vec!["credentials".into()],
        config: Payload::null(),
    }
}

fn ready(plugin: &str) -> PluginReadyReport {
    PluginReadyReport {
        plugin_id: plugin.into(),
        services: vec!["compose".into()],
        event_topics: vec!["session".into()],
        llm_providers: vec!["demo".into()],
        llm_adapters: Default::default(),
        tools: vec![tool_definition("grep")],
        commands: Vec::new(),
    }
}

fn tool_definition(name: &str) -> PluginToolDefinition {
    PluginToolDefinition {
        name: name.into(),
        description: "search files".into(),
        input_schema: Payload::from(serde_json::json!({"type": "object"})),
    }
}

fn identity(plugin: &str, call: &str) -> CallIdentity {
    CallIdentity {
        host_epoch: 7,
        plugin_id: plugin.into(),
        scope_id: "session-1".into(),
        scope_generation: 3,
        call_id: call.into(),
    }
}

fn call(service: &str) -> ServiceCallRequest {
    ServiceCallRequest {
        service: service.into(),
        request: Payload::null(),
    }
}

fn subscribe(id: &str, topic: &str) -> EventSubscribeRequest {
    EventSubscribeRequest {
        subscription: id.into(),
        topic: topic.into(),
    }
}

fn delivery(id: &str, topic: &str) -> EventDelivery {
    EventDelivery {
        subscription: id.into(),
        topic: topic.into(),
        event: Payload::null(),
    }
}

fn loaded(plugin: &str) -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry.admit_load(&load(plugin)).unwrap();
    registry.accept_ready(&ready(plugin)).unwrap();
    registry
}

#[test]
fn the_seven_stages_run_end_to_end() {
    let mut registry = PluginRegistry::new();

    registry.admit_load(&load("plugin.a")).unwrap();
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Admitted));

    registry.accept_ready(&ready("plugin.a")).unwrap();
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Ready));

    registry
        .admit_service_call(&identity("plugin.a", "call-1"), &call("compose"))
        .unwrap();
    registry
        .subscribe(
            &identity("plugin.a", "sub-call"),
            &subscribe("s1", "session"),
        )
        .unwrap();
    registry
        .admit_event_delivery(&identity("plugin.a", "ev-1"), &delivery("s1", "session"))
        .unwrap();

    let report = registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    assert_eq!(report.outstanding_calls, vec!["call-1".to_string()]);
    assert_eq!(report.revoked_subscriptions, vec!["s1".to_string()]);
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Draining));

    // Closing the last call is what finishes this drain — there is nothing left
    // for a separate finish_unload to do, and asking anyway says so.
    assert_eq!(
        registry.complete_call("plugin.a", "call-1").unwrap(),
        CallClosed::DrainFinished
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Unloaded));
    assert_eq!(
        registry.finish_unload("plugin.a").unwrap_err().code(),
        "[PLUGIN_NOT_READY]"
    );
}

#[test]
fn the_reserved_platform_identity_cannot_be_loaded() {
    let mut registry = PluginRegistry::new();
    let error = registry.admit_load(&load(PLATFORM_PLUGIN_ID)).unwrap_err();
    assert_eq!(error.code(), "[RESERVED_PLUGIN_ID]");
    assert_eq!(registry.phase(PLATFORM_PLUGIN_ID), None);
}

#[test]
fn loading_twice_is_refused_and_reloading_after_unload_is_not() {
    let mut registry = loaded("plugin.a");
    let error = registry.admit_load(&load("plugin.a")).unwrap_err();
    assert_eq!(error.code(), "[PLUGIN_ALREADY_LOADED]");

    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    registry.finish_unload("plugin.a").unwrap();

    // A reload starts from the new manifest, not the old declarations.
    let mut narrower = load("plugin.a");
    narrower.services = vec!["render".into()];
    narrower.event_topics = Vec::new();
    registry.admit_load(&narrower).unwrap();
    let error = registry
        .accept_ready(&PluginReadyReport {
            plugin_id: "plugin.a".into(),
            services: vec!["compose".into()],
            event_topics: Vec::new(),
            llm_providers: Vec::new(),
            llm_adapters: Default::default(),
            tools: Vec::new(),
            commands: Vec::new(),
        })
        .unwrap_err();
    assert_eq!(error.code(), "[UNAUTHORIZED_REGISTER]");
}

/// The manifest is the reviewable artifact, so it is the ceiling: no amount of
/// plugin code may widen what the plugin offers.
#[test]
fn a_ready_report_cannot_register_beyond_the_manifest() {
    for (services, topics, providers, name) in [
        (vec!["smuggled".to_string()], vec![], vec![], "smuggled"),
        (vec![], vec!["smuggled".to_string()], vec![], "smuggled"),
        (vec![], vec![], vec!["smuggled".to_string()], "smuggled"),
    ] {
        let mut registry = PluginRegistry::new();
        registry.admit_load(&load("plugin.a")).unwrap();
        let error = registry
            .accept_ready(&PluginReadyReport {
                plugin_id: "plugin.a".into(),
                services,
                event_topics: topics,
                llm_providers: providers,
                llm_adapters: Default::default(),
                tools: Vec::new(),
                commands: Vec::new(),
            })
            .unwrap_err();
        assert_eq!(error.code(), "[UNAUTHORIZED_REGISTER]");
        assert!(error.to_string().contains(name), "{error}");
        // The refusal leaves the plugin where it was, not half-ready.
        assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Admitted));
    }
}

/// Declaring more than it registers is fine — a manifest is permission, not an
/// obligation.
#[test]
fn a_ready_report_may_register_less_than_declared() {
    let registry = loaded("plugin.a");
    let services = registry.services("plugin.a").unwrap();
    assert!(services.contains("compose"));
    assert!(
        !services.contains("render"),
        "declared but never registered"
    );
}

#[test]
fn a_failed_load_leaves_nothing_behind() {
    let mut registry = PluginRegistry::new();
    registry.admit_load(&load("plugin.a")).unwrap();
    registry.reject_load("plugin.a").unwrap();
    assert_eq!(registry.phase("plugin.a"), None);

    // …and rejecting something that is not mid-load says so.
    assert_eq!(
        registry.reject_load("plugin.a").unwrap_err().code(),
        "[UNKNOWN_PLUGIN]"
    );
    let mut ready_registry = loaded("plugin.b");
    assert_eq!(
        ready_registry.reject_load("plugin.b").unwrap_err().code(),
        "[PLUGIN_NOT_READY]"
    );
}

/// "Not there yet" and "no longer there" are different diagnoses, and a caller
/// retries only one of them.
#[test]
fn routing_refusals_distinguish_not_yet_ready_from_stale() {
    let mut registry = PluginRegistry::new();
    registry.admit_load(&load("plugin.a")).unwrap();
    assert_eq!(
        registry
            .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
            .unwrap_err()
            .code(),
        "[PLUGIN_NOT_READY]"
    );

    registry.accept_ready(&ready("plugin.a")).unwrap();
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    assert_eq!(
        registry
            .admit_service_call(&identity("plugin.a", "c2"), &call("compose"))
            .unwrap_err()
            .code(),
        "[STALE_PROVIDER]"
    );

    registry.finish_unload("plugin.a").unwrap();
    assert_eq!(
        registry
            .admit_service_call(&identity("plugin.a", "c3"), &call("compose"))
            .unwrap_err()
            .code(),
        "[STALE_PROVIDER]",
        "an unloaded plugin is stale, not unknown"
    );
    assert_eq!(
        registry
            .admit_service_call(&identity("plugin.z", "c4"), &call("compose"))
            .unwrap_err()
            .code(),
        "[UNKNOWN_PLUGIN]"
    );
}

#[test]
fn a_service_the_plugin_did_not_register_is_refused() {
    let mut registry = loaded("plugin.a");
    // Declared in the manifest but never registered — still not callable.
    let error = registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("render"))
        .unwrap_err();
    assert_eq!(error.code(), "[UNKNOWN_SERVICE]");
}

#[test]
fn call_ids_are_accounted_for_exactly_once() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
        .unwrap();
    assert_eq!(
        registry
            .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
            .unwrap_err()
            .code(),
        "[DUPLICATE_CALL]"
    );
    assert_eq!(registry.in_flight("plugin.a"), vec!["c1".to_string()]);

    registry.complete_call("plugin.a", "c1").unwrap();
    assert!(registry.in_flight("plugin.a").is_empty());
    assert_eq!(
        registry.complete_call("plugin.a", "c1").unwrap_err().code(),
        "[UNKNOWN_CALL]"
    );
}

#[test]
fn subscriptions_need_a_registered_topic() {
    let mut registry = loaded("plugin.a");
    assert_eq!(
        registry
            .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "other"))
            .unwrap_err()
            .code(),
        "[UNKNOWN_TOPIC]"
    );
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();
}

#[test]
fn subscription_ids_are_unique_per_plugin_not_globally() {
    let mut registry = loaded("plugin.a");
    registry.admit_load(&load("plugin.b")).unwrap();
    registry.accept_ready(&ready("plugin.b")).unwrap();

    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();
    assert_eq!(
        registry
            .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
            .unwrap_err()
            .code(),
        "[DUPLICATE_SUBSCRIPTION]"
    );
    // The same id under another plugin is a different subscription.
    registry
        .subscribe(&identity("plugin.b", "s"), &subscribe("s1", "session"))
        .unwrap();
}

#[test]
fn unsubscribe_removes_exactly_the_named_subscription() {
    let mut registry = loaded("plugin.a");
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s2", "session"))
        .unwrap();

    registry
        .unsubscribe(
            "plugin.a",
            &EventUnsubscribeRequest {
                subscription: "s1".into(),
            },
        )
        .unwrap();
    assert!(registry.subscription("plugin.a", "s1").is_none());
    assert!(registry.subscription("plugin.a", "s2").is_some());

    assert_eq!(
        registry
            .unsubscribe(
                "plugin.a",
                &EventUnsubscribeRequest {
                    subscription: "s1".into(),
                },
            )
            .unwrap_err()
            .code(),
        "[UNKNOWN_SUBSCRIPTION]"
    );
}

#[test]
fn delivery_must_match_the_subscription_it_names() {
    let mut registry = loaded("plugin.a");
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();

    assert_eq!(
        registry
            .admit_event_delivery(&identity("plugin.a", "e"), &delivery("s2", "session"))
            .unwrap_err()
            .code(),
        "[UNKNOWN_SUBSCRIPTION]"
    );
    assert_eq!(
        registry
            .admit_event_delivery(&identity("plugin.a", "e"), &delivery("s1", "other"))
            .unwrap_err()
            .code(),
        "[TOPIC_MISMATCH]"
    );
}

/// A delivery carrying an older generation is aimed at a scope incarnation that
/// no longer exists; letting it through would backfill a closed scope.
#[test]
fn delivery_is_pinned_to_the_scope_incarnation_that_subscribed() {
    let mut registry = loaded("plugin.a");
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();

    let mut older = identity("plugin.a", "e");
    older.scope_generation = 2;
    assert_eq!(
        registry
            .admit_event_delivery(&older, &delivery("s1", "session"))
            .unwrap_err()
            .code(),
        "[STALE_SUBSCRIPTION]"
    );

    let mut other_scope = identity("plugin.a", "e");
    other_scope.scope_id = "session-2".into();
    assert_eq!(
        registry
            .admit_event_delivery(&other_scope, &delivery("s1", "session"))
            .unwrap_err()
            .code(),
        "[STALE_SUBSCRIPTION]"
    );
}

#[test]
fn advancing_a_generation_revokes_the_subscriptions_of_the_old_one() {
    let mut registry = loaded("plugin.a");
    registry.admit_load(&load("plugin.b")).unwrap();
    registry.accept_ready(&ready("plugin.b")).unwrap();
    registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("s1", "session"))
        .unwrap();
    registry
        .subscribe(&identity("plugin.b", "s"), &subscribe("s1", "session"))
        .unwrap();

    // A subscription in a different scope is untouched.
    let mut other = identity("plugin.a", "s");
    other.scope_id = "session-2".into();
    registry
        .subscribe(&other, &subscribe("s-other", "session"))
        .unwrap();

    let revoked = registry.revoke_stale_subscriptions("plugin.a", "session-1", 4);
    assert_eq!(revoked, vec![("plugin.a".to_string(), "s1".to_string())]);
    assert!(registry.subscription("plugin.a", "s1").is_none());

    // A subscription in another scope of the same plugin is untouched.
    assert!(registry.subscription("plugin.a", "s-other").is_some());

    // And so is another plugin's subscription on the same session. A generation
    // counts incarnations of one plugin's scope; plugin.b holds its own counter
    // and its scope is still open, so revoking here would cancel a live
    // subscription by comparing two unrelated clocks.
    assert!(registry.subscription("plugin.b", "s1").is_some());

    // Re-running at the same generation revokes nothing more.
    assert!(registry
        .revoke_stale_subscriptions("plugin.a", "session-1", 4)
        .is_empty());
}

#[test]
fn a_drain_cannot_finish_while_work_is_in_flight() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
        .unwrap();
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();

    let error = registry.finish_unload("plugin.a").unwrap_err();
    assert_eq!(error.code(), "[DRAIN_INCOMPLETE]");
    assert!(error.to_string().contains('1'), "{error}");
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Draining));

    assert_eq!(
        registry.complete_call("plugin.a", "c1").unwrap(),
        CallClosed::DrainFinished
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Unloaded));
}

/// The whole point of finishing a drain: the id comes back.
///
/// A plugin whose last call closed during the drain has to end up loadable
/// again, or an unload is a one-way door and a reload is impossible.
#[test]
fn a_plugin_drained_by_its_last_call_can_be_loaded_again() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
        .unwrap();
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    assert_eq!(
        registry.admit_load(&load("plugin.a")).unwrap_err().code(),
        "[PLUGIN_ALREADY_LOADED]",
        "a draining plugin still holds its id"
    );

    registry.complete_call("plugin.a", "c1").unwrap();

    registry.admit_load(&load("plugin.a")).unwrap();
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Admitted));
}

/// Closing a call outside a drain changes nothing about the plugin.
#[test]
fn closing_a_call_on_a_live_plugin_leaves_it_alone() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
        .unwrap();
    assert_eq!(
        registry.complete_call("plugin.a", "c1").unwrap(),
        CallClosed::PluginUnaffected
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Ready));
}

/// A call that is not the last one leaves the drain running.
#[test]
fn a_drain_with_two_calls_finishes_on_the_second() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_service_call(&identity("plugin.a", "c1"), &call("compose"))
        .unwrap();
    registry
        .admit_service_call(&identity("plugin.a", "c2"), &call("compose"))
        .unwrap();
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();

    assert_eq!(
        registry.complete_call("plugin.a", "c1").unwrap(),
        CallClosed::PluginUnaffected
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Draining));
    assert_eq!(
        registry.complete_call("plugin.a", "c2").unwrap(),
        CallClosed::DrainFinished
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Unloaded));
}

/// The host has no phase for a plugin whose module has not run yet, so an
/// unload naming one is refused there. This side refuses it too, and says which
/// of the two "not routable" situations it is: not ready yet, not stale.
#[test]
fn a_plugin_still_loading_cannot_be_unloaded() {
    let mut registry = PluginRegistry::new();
    registry.admit_load(&load("plugin.a")).unwrap();
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Admitted));

    let error = registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap_err();
    assert_eq!(error.code(), "[PLUGIN_NOT_READY]");
    assert_eq!(
        registry.phase("plugin.a"),
        Some(PluginPhase::Admitted),
        "a refused unload leaves the load to finish on its own"
    );

    // And it does: the load that was in flight still completes.
    registry.accept_ready(&ready("plugin.a")).unwrap();
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Ready));
}

#[test]
fn unloading_twice_reads_as_stale_rather_than_unknown() {
    let mut registry = loaded("plugin.a");
    let request = PluginUnloadRequest {
        plugin_id: "plugin.a".into(),
    };
    registry.begin_unload(&request).unwrap();
    assert_eq!(
        registry.begin_unload(&request).unwrap_err().code(),
        "[STALE_PROVIDER]"
    );
    registry.finish_unload("plugin.a").unwrap();
    assert_eq!(
        registry.begin_unload(&request).unwrap_err().code(),
        "[STALE_PROVIDER]"
    );
    assert_eq!(
        registry.finish_unload("plugin.a").unwrap_err().code(),
        "[PLUGIN_NOT_READY]"
    );
}

/// Every refusal a caller can act on needs a token it can switch on, and no two
/// distinct refusals may share one.
#[test]
fn every_error_carries_a_distinct_bracketed_code() {
    let errors = [
        RegistryError::AlreadyLoaded {
            plugin_id: "p".into(),
        },
        RegistryError::UnknownPlugin {
            plugin_id: "p".into(),
        },
        RegistryError::NotReady {
            plugin_id: "p".into(),
            phase: PluginPhase::Admitted,
        },
        RegistryError::StaleProvider {
            plugin_id: "p".into(),
            phase: PluginPhase::Draining,
        },
        RegistryError::Undeclared {
            plugin_id: "p".into(),
            kind: "service",
            name: "s".into(),
        },
        RegistryError::UnknownService {
            plugin_id: "p".into(),
            service: "s".into(),
        },
        RegistryError::UnknownTopic {
            plugin_id: "p".into(),
            topic: "t".into(),
        },
        RegistryError::DuplicateCall {
            call_id: "c".into(),
        },
        RegistryError::UnknownCall {
            plugin_id: "p".into(),
            call_id: "c".into(),
        },
        RegistryError::DuplicateSubscription {
            plugin_id: "p".into(),
            subscription: "s".into(),
        },
        RegistryError::UnknownSubscription {
            plugin_id: "p".into(),
            subscription: "s".into(),
        },
        RegistryError::StaleSubscription {
            subscription: "s".into(),
            scope_id: "sc".into(),
            recorded: 1,
            actual: 2,
        },
        RegistryError::TopicMismatch {
            subscription: "s".into(),
            recorded: "a".into(),
            actual: "b".into(),
        },
        RegistryError::DrainIncomplete {
            plugin_id: "p".into(),
            count: 1,
        },
        RegistryError::IdentityMismatch {
            expected: "a".into(),
            actual: "b".into(),
        },
    ];
    let mut codes = std::collections::BTreeSet::new();
    for error in &errors {
        let code = error.code();
        assert!(code.starts_with('[') && code.ends_with(']'), "{code}");
        assert!(codes.insert(code), "{code} is used twice");
        assert!(!error.to_string().is_empty());
    }
    assert_eq!(codes.len(), errors.len());
}

/// Payload validation runs before any state changes, so a malformed request
/// cannot half-apply.
#[test]
fn malformed_payloads_are_refused_without_touching_state() {
    let mut registry = loaded("plugin.a");
    let error = registry
        .admit_service_call(
            &identity("plugin.a", "c1"),
            &ServiceCallRequest {
                service: String::new(),
                request: Payload::null(),
            },
        )
        .unwrap_err();
    assert_eq!(error.code(), "[EMPTY_NAME]");
    assert!(registry.in_flight("plugin.a").is_empty());

    let error = registry
        .subscribe(&identity("plugin.a", "s"), &subscribe("", "session"))
        .unwrap_err();
    assert_eq!(error.code(), "[EMPTY_NAME]");
    assert!(registry.subscription("plugin.a", "").is_none());
}

fn invoke(tool: &str) -> ToolInvokeRequest {
    ToolInvokeRequest {
        tool: tool.into(),
        input: Payload::null(),
    }
}

/// The manifest is the ceiling for what a plugin may *call*, exactly as it is
/// for what a plugin may provide. A tool has no registered counterpart, so this
/// declaration is the only thing standing between a plugin and the tool.
#[test]
fn a_plugin_may_invoke_only_the_tools_its_manifest_declared() {
    let registry = loaded("plugin.a");
    registry
        .admit_tool_invoke(&identity("plugin.a", "c1"), &invoke("read_file"))
        .expect("a declared tool is allowed");

    let error = registry
        .admit_tool_invoke(&identity("plugin.a", "c2"), &invoke("write_file"))
        .unwrap_err();
    assert_eq!(error.code(), "[UNAUTHORIZED_TOOL]");
}

#[test]
fn a_draining_plugin_may_not_start_new_tool_work() {
    let mut registry = loaded("plugin.a");
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    let error = registry
        .admit_tool_invoke(&identity("plugin.a", "c1"), &invoke("read_file"))
        .unwrap_err();
    assert_eq!(error.code(), "[STALE_PROVIDER]");
}

/// Reloading takes the new manifest's word, not the old one's — otherwise a
/// plugin could keep a permission its current manifest no longer asks for.
#[test]
fn reloading_replaces_the_declared_tools() {
    let mut registry = loaded("plugin.a");
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    registry.finish_unload("plugin.a").unwrap();

    let mut narrower = load("plugin.a");
    narrower.invokable_tools = Vec::new();
    registry.admit_load(&narrower).unwrap();
    registry.accept_ready(&ready("plugin.a")).unwrap();

    let error = registry
        .admit_tool_invoke(&identity("plugin.a", "c1"), &invoke("read_file"))
        .unwrap_err();
    assert_eq!(error.code(), "[UNAUTHORIZED_TOOL]");
    assert!(registry
        .declared_invokable_tools("plugin.a")
        .unwrap()
        .is_empty());
}

fn call_tool(tool: &str) -> ToolInvokeRequest {
    ToolInvokeRequest {
        tool: tool.into(),
        input: Payload::null(),
    }
}

/// Providing a tool and being allowed to call one are different rights, and the
/// manifest keeps them in different lists. Reading one for the other would let
/// a plugin call whatever it happened to provide.
#[test]
fn providing_a_tool_and_invoking_one_are_separate_permissions() {
    let mut registry = loaded("plugin.a");

    // `grep` is provided; `read_file` may be invoked. Neither implies the other.
    registry
        .admit_tool_call(&identity("plugin.a", "c1"), &call_tool("grep"))
        .expect("a registered tool can be called");
    assert_eq!(
        registry
            .admit_tool_call(&identity("plugin.a", "c2"), &call_tool("read_file"))
            .unwrap_err()
            .code(),
        "[UNKNOWN_TOOL]",
        "being allowed to invoke a tool does not mean providing one"
    );
    assert_eq!(
        registry
            .admit_tool_invoke(&identity("plugin.a", "c3"), &invoke("grep"))
            .unwrap_err()
            .code(),
        "[UNAUTHORIZED_TOOL]",
        "providing a tool does not mean being allowed to invoke it"
    );
}

/// A tool has to describe itself, because unlike a service it is offered to a
/// model rather than called by something that already knows it exists.
#[test]
fn a_registered_tool_keeps_the_definition_a_model_reads() {
    let registry = loaded("plugin.a");
    let tools = registry.tools("plugin.a");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "grep");
    assert_eq!(tools[0].description, "search files");
    assert_eq!(
        tools[0].input_schema.to_value().unwrap(),
        serde_json::json!({"type": "object"})
    );
}

/// A running tool is work in flight exactly like a service call, so a drain
/// waits for it and cannot be finished behind its back.
#[test]
fn a_running_tool_holds_a_drain_open() {
    let mut registry = loaded("plugin.a");
    registry
        .admit_tool_call(&identity("plugin.a", "c1"), &call_tool("grep"))
        .unwrap();
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    assert_eq!(
        registry.finish_unload("plugin.a").unwrap_err().code(),
        "[DRAIN_INCOMPLETE]"
    );
    assert_eq!(
        registry.complete_call("plugin.a", "c1").unwrap(),
        CallClosed::DrainFinished
    );
    assert_eq!(registry.phase("plugin.a"), Some(PluginPhase::Unloaded));
}

#[test]
fn a_draining_plugin_routes_no_new_tool_calls() {
    let mut registry = loaded("plugin.a");
    registry
        .begin_unload(&PluginUnloadRequest {
            plugin_id: "plugin.a".into(),
        })
        .unwrap();
    assert_eq!(
        registry
            .admit_tool_call(&identity("plugin.a", "c1"), &call_tool("grep"))
            .unwrap_err()
            .code(),
        "[STALE_PROVIDER]"
    );
}
