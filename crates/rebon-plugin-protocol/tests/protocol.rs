use rebon_plugin_protocol::{
    next_scope_generation, CallIdentity, CallLedger, CancelDisposition, ChunkDisposition,
    CodecError, LifecycleError, NdjsonCodec, StaleReason, TerminalDisposition, TerminalStatus,
    WireContractError, WireEnvelope, WireMessage, CALL_CANCEL_METHOD, DEFAULT_MAX_FRAME_BYTES,
    MAX_SAFE_WIRE_INTEGER, PLATFORM_CONTROL_GENERATION, PLATFORM_CONTROL_SCOPE_ID,
    PLATFORM_PLUGIN_ID, PROTOCOL_VERSION, SCOPE_CLOSE_METHOD, SCOPE_OPEN_METHOD,
};
use serde_json::json;

fn identity(call_id: &str) -> CallIdentity {
    CallIdentity {
        host_epoch: 7,
        plugin_id: "plugin.example".into(),
        scope_id: "session-1".into(),
        scope_generation: 3,
        call_id: call_id.into(),
    }
}

fn request(call_id: &str) -> WireEnvelope {
    WireEnvelope::new(
        identity(call_id),
        WireMessage::Request {
            method: "opaque.method".into(),
            payload: json!({"input": 42}).into(),
        },
    )
}

fn terminal(call_id: &str, status: TerminalStatus) -> WireEnvelope {
    WireEnvelope::new(
        identity(call_id),
        WireMessage::Terminal {
            status,
            payload: json!({"detail": "done"}).into(),
        },
    )
}

fn chunk(call_id: &str) -> WireEnvelope {
    WireEnvelope::new(
        identity(call_id),
        WireMessage::Chunk {
            payload: json!({"delta": "hi"}).into(),
        },
    )
}

fn ledger(call_id: &str) -> CallLedger {
    let mut ledger = CallLedger::new(7).unwrap();
    ledger
        .advance_scope_generation("plugin.example", "session-1", 3)
        .unwrap();
    ledger.register_call(identity(call_id)).unwrap();
    ledger
}

#[test]
fn serde_roundtrip_preserves_all_versioned_message_shapes() {
    let envelopes = [
        request("request"),
        terminal("terminal", TerminalStatus::Success),
        WireEnvelope::new(
            identity("notification"),
            WireMessage::Notification {
                method: "opaque.event".into(),
                payload: json!({"value": true}).into(),
            },
        ),
    ];
    for envelope in envelopes {
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(
            serde_json::from_value::<WireEnvelope>(value).unwrap(),
            envelope
        );
    }
}

#[test]
fn codec_accepts_lf_crlf_chunking_and_multiple_frames() {
    let first = request("one");
    let second = request("two");
    let codec = NdjsonCodec::new(1024);
    let mut input = codec.encode(&first).unwrap();
    input.pop();
    input.extend_from_slice(b"\r\n");
    input.extend(codec.encode(&second).unwrap());

    let mut decoder = NdjsonCodec::new(1024);
    let split = input.len() / 2;
    assert!(decoder.push(&input[..split]).unwrap().is_empty());
    assert_eq!(decoder.push(&input[split..]).unwrap(), vec![first, second]);
    decoder.finish().unwrap();
}

#[test]
fn default_codec_enforces_exact_eight_mib_body_boundary_on_decode_and_encode() {
    let mut envelope = request("boundary");
    let empty_len = serde_json::to_vec(&envelope).unwrap().len();
    let padding = DEFAULT_MAX_FRAME_BYTES - empty_len;
    if let WireMessage::Request { payload, .. } = &mut envelope.message {
        *payload = json!("x".repeat(padding)).into();
    }
    // Replacing `{\"input\":42}` with a JSON string changes the fixed overhead.
    let current = serde_json::to_vec(&envelope).unwrap().len();
    let adjustment = DEFAULT_MAX_FRAME_BYTES as isize - current as isize;
    if let WireMessage::Request { payload, .. } = &mut envelope.message {
        let target = (padding as isize + adjustment) as usize;
        *payload = json!("x".repeat(target)).into();
    }
    assert_eq!(
        serde_json::to_vec(&envelope).unwrap().len(),
        8 * 1024 * 1024
    );

    let codec = NdjsonCodec::default();
    let encoded = codec.encode(&envelope).unwrap();
    assert_eq!(encoded.len(), DEFAULT_MAX_FRAME_BYTES + 1);
    let mut decoder = NdjsonCodec::default();
    assert_eq!(decoder.push(&encoded).unwrap(), vec![envelope.clone()]);

    if let WireMessage::Request { payload, .. } = &mut envelope.message {
        let mut text = payload.to_value().unwrap().as_str().unwrap().to_owned();
        text.push('x');
        *payload = json!(text).into();
    }
    assert!(matches!(
        codec.encode(&envelope),
        Err(CodecError::FrameTooLarge {
            limit: DEFAULT_MAX_FRAME_BYTES
        })
    ));
}

#[test]
fn codec_rejects_malformed_empty_polluted_invalid_utf8_and_wrong_shape() {
    for bytes in [
        b"{not-json}\n".as_slice(),
        b"\n".as_slice(),
        b"plugin wrote to stdout\n".as_slice(),
        b"{}\n".as_slice(),
    ] {
        let mut codec = NdjsonCodec::new(1024);
        assert!(codec.push(bytes).is_err());
        assert!(matches!(codec.push(b"{}\n"), Err(CodecError::Failed)));
    }

    let mut codec = NdjsonCodec::new(1024);
    assert!(matches!(
        codec.push(&[0xff, b'\n']),
        Err(CodecError::InvalidUtf8(_))
    ));
}

#[test]
fn codec_rejects_oversize_input_before_unbounded_growth_and_incomplete_eof() {
    let mut codec = NdjsonCodec::new(16);
    assert!(matches!(
        codec.push(&[b'x'; 17]),
        Err(CodecError::FrameTooLarge { limit: 16 })
    ));

    let mut codec = NdjsonCodec::new(1024);
    codec.push(br#"{"protocol_version":1}"#).unwrap();
    assert!(matches!(
        codec.finish(),
        Err(CodecError::IncompleteFrame { .. })
    ));
}

#[test]
fn public_serde_rejects_unsupported_version_on_encode_and_decode() {
    let mut unsupported = request("direct-serde-encode");
    unsupported.protocol_version = PROTOCOL_VERSION + 1;
    let encode_error = serde_json::to_vec(&unsupported).unwrap_err();
    assert!(encode_error
        .to_string()
        .contains("unsupported protocol version 2; expected 1"));

    let text = serde_json::to_string(&request("direct-serde-decode"))
        .unwrap()
        .replace("\"protocol_version\":1", "\"protocol_version\":2");
    let decode_error = serde_json::from_str::<WireEnvelope>(&text).unwrap_err();
    assert!(decode_error
        .to_string()
        .contains("unsupported protocol version 2; expected 1"));
}

#[test]
fn codec_rejects_unsupported_version_and_unknown_fields() {
    let mut value = serde_json::to_value(request("bad-version")).unwrap();
    value["protocol_version"] = json!(2);
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    let mut codec = NdjsonCodec::new(1024);
    assert!(matches!(
        codec.push(&bytes),
        Err(CodecError::UnsupportedVersion { actual: 2, .. })
    ));
    assert!(matches!(codec.push(b"{}\n"), Err(CodecError::Failed)));

    let mut value = serde_json::to_value(request("extra")).unwrap();
    value["unexpected"] = json!(true);
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    assert!(matches!(
        NdjsonCodec::new(1024).push(&bytes),
        Err(CodecError::InvalidEnvelope(_))
    ));
}

#[test]
fn encode_error_leaves_codec_usable_for_encode_and_decode() {
    let mut codec = NdjsonCodec::new(1024);
    let mut unsupported = request("unsupported");
    unsupported.protocol_version = PROTOCOL_VERSION + 1;
    assert!(matches!(
        codec.encode(&unsupported),
        Err(CodecError::UnsupportedVersion { .. })
    ));

    let valid = request("valid-after-encode-error");
    let encoded = codec.encode(&valid).unwrap();
    assert_eq!(codec.push(&encoded).unwrap(), vec![valid]);
    codec.finish().unwrap();
}

#[test]
fn unknown_call_and_duplicate_terminal_are_rejected() {
    let mut ledger = CallLedger::new(7).unwrap();
    ledger
        .advance_scope_generation("plugin.example", "session-1", 3)
        .unwrap();
    assert_eq!(
        ledger.submit_terminal(&terminal("missing", TerminalStatus::Success)),
        Err(LifecycleError::UnknownCall {
            call_id: "missing".into()
        })
    );

    ledger.register_call(identity("known")).unwrap();
    assert!(matches!(
        ledger
            .submit_terminal(&terminal("known", TerminalStatus::Success))
            .unwrap(),
        TerminalDisposition::Committed(_)
    ));
    assert_eq!(
        ledger.submit_terminal(&terminal("known", TerminalStatus::Error)),
        Err(LifecycleError::DuplicateTerminal {
            call_id: "known".into(),
            status: TerminalStatus::Success
        })
    );
}

#[test]
fn cancellation_and_response_race_has_exactly_one_winner() {
    for (first, second) in [
        (TerminalStatus::Cancelled, TerminalStatus::Success),
        (TerminalStatus::Success, TerminalStatus::Cancelled),
    ] {
        let mut ledger = ledger("race");
        assert_eq!(
            ledger.submit_terminal(&terminal("race", first)).unwrap(),
            TerminalDisposition::Committed(rebon_plugin_protocol::TerminalCommit { status: first })
        );
        assert!(matches!(
            ledger.submit_terminal(&terminal("race", second)),
            Err(LifecycleError::DuplicateTerminal { status, .. }) if status == first
        ));
    }
}

#[test]
fn old_epoch_and_generation_are_classified_stale_without_touching_call() {
    let mut ledger = ledger("stale");
    let mut old_epoch = terminal("stale", TerminalStatus::Error);
    old_epoch.identity.host_epoch = 6;
    assert_eq!(
        ledger.submit_terminal(&old_epoch).unwrap(),
        TerminalDisposition::Stale(StaleReason::HostEpoch {
            message: 6,
            current: 7
        })
    );

    let mut old_generation = terminal("stale", TerminalStatus::Error);
    old_generation.identity.scope_generation = 2;
    assert_eq!(
        ledger.submit_terminal(&old_generation).unwrap(),
        TerminalDisposition::Stale(StaleReason::ScopeGeneration {
            message: 2,
            current: 3
        })
    );

    assert!(matches!(
        ledger
            .submit_terminal(&terminal("stale", TerminalStatus::Success))
            .unwrap(),
        TerminalDisposition::Committed(_)
    ));
}

#[test]
fn scope_generation_cannot_regress_and_revive_a_stale_terminal() {
    let mut ledger = CallLedger::new(7).unwrap();
    ledger
        .advance_scope_generation("plugin.example", "session-1", 3)
        .unwrap();
    ledger
        .advance_scope_generation("plugin.example", "session-1", 3)
        .unwrap();
    ledger.register_call(identity("generation-race")).unwrap();
    ledger
        .advance_scope_generation("plugin.example", "session-1", 4)
        .unwrap();

    let old_terminal = terminal("generation-race", TerminalStatus::Success);
    let stale = TerminalDisposition::Stale(StaleReason::ScopeGeneration {
        message: 3,
        current: 4,
    });
    assert_eq!(ledger.submit_terminal(&old_terminal).unwrap(), stale);
    assert_eq!(
        ledger.advance_scope_generation("plugin.example", "session-1", 3),
        Err(LifecycleError::ScopeGenerationRegression {
            plugin_id: "plugin.example".into(),
            scope_id: "session-1".into(),
            current: 4,
            attempted: 3,
        })
    );
    assert_eq!(ledger.submit_terminal(&old_terminal).unwrap(), stale);

    let mut current_terminal = old_terminal;
    current_terminal.identity.scope_generation = 4;
    assert!(matches!(
        ledger.submit_terminal(&current_terminal),
        Err(LifecycleError::IdentityMismatch {
            field: "scope_generation",
            ..
        })
    ));
    assert_eq!(
        ledger.submit_terminal(&terminal("generation-race", TerminalStatus::Error)),
        Ok(stale)
    );
}

#[test]
fn safe_wire_integers_follow_exact_json_numeric_semantics() {
    fn with_identity_number(field: &str, number: &str) -> String {
        let current = match field {
            "host_epoch" => 7,
            "scope_generation" => 3,
            _ => unreachable!(),
        };
        serde_json::to_string(&request("numeric-semantics"))
            .unwrap()
            .replace(
                &format!("\"{field}\":{current}"),
                &format!("\"{field}\":{number}"),
            )
    }

    for field in ["host_epoch", "scope_generation"] {
        for (number, expected) in [
            ("0", 0),
            ("-0", 0),
            ("-0.0", 0),
            ("-0e999999", 0),
            ("-0.0e999999", 0),
            ("0e999999", 0),
            ("0e-999999", 0),
            ("7", 7),
            ("7e0", 7),
            ("7e-0", 7),
            ("70e-1", 7),
            ("7.0", 7),
            ("9007199254740991", MAX_SAFE_WIRE_INTEGER),
            ("90071992547409910e-1", MAX_SAFE_WIRE_INTEGER),
        ] {
            let envelope =
                serde_json::from_str::<WireEnvelope>(&with_identity_number(field, number))
                    .unwrap_or_else(|error| panic!("{field}={number} should be accepted: {error}"));
            let actual = match field {
                "host_epoch" => envelope.identity.host_epoch,
                "scope_generation" => envelope.identity.scope_generation,
                _ => unreachable!(),
            };
            assert_eq!(actual, expected, "{field}={number}");
            let encoded = serde_json::to_string(&envelope).unwrap();
            assert!(encoded.contains(&format!("\"{field}\":{expected}")));
        }

        for number in [
            "-1",
            "-1.0",
            "-1e-999999",
            "7.5",
            "7e-1",
            "9007199254740992",
            "9007199254740992e0",
            "9007199254740991.5",
            "90071992547409915e-1",
            "18446744073709551615",
            "1e999",
        ] {
            assert!(
                serde_json::from_str::<WireEnvelope>(&with_identity_number(field, number)).is_err(),
                "{field}={number} should be rejected"
            );
        }

        for invalid_json_number in ["-000", "07", ".7", "7.", "+7"] {
            assert!(
                serde_json::from_str::<WireEnvelope>(&with_identity_number(
                    field,
                    invalid_json_number,
                ))
                .is_err(),
                "{invalid_json_number} is not a legal JSON number for {field}"
            );
        }
    }

    let both_negative_zero = with_identity_number("host_epoch", "-0.0e999999")
        .replace("\"scope_generation\":3", "\"scope_generation\":-0e-999999");
    let envelope = serde_json::from_str::<WireEnvelope>(&both_negative_zero).unwrap();
    assert_eq!(envelope.identity.host_epoch, 0);
    assert_eq!(envelope.identity.scope_generation, 0);
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert!(encoded.contains("\"host_epoch\":0"));
    assert!(encoded.contains("\"scope_generation\":0"));
}

#[test]
fn extreme_safe_integer_inputs_are_linear_in_input_size() {
    fn with_host_epoch(number: &str) -> String {
        serde_json::to_string(&request("extreme-numeric-semantics"))
            .unwrap()
            .replace("\"host_epoch\":7", &format!("\"host_epoch\":{number}"))
    }

    let exponent = "9".repeat(200_000);
    let zero_fraction = "0".repeat(200_000);
    for number in [
        format!("0e{exponent}"),
        format!("0e-{exponent}"),
        format!("-0.{zero_fraction}e{exponent}"),
        format!("-0.{zero_fraction}e-{exponent}"),
    ] {
        let envelope = serde_json::from_str::<WireEnvelope>(&with_host_epoch(&number))
            .unwrap_or_else(|error| panic!("extreme zero should be accepted: {error}"));
        assert_eq!(envelope.identity.host_epoch, 0);
    }

    for number in [format!("1e{exponent}"), format!("1{zero_fraction}")] {
        assert!(
            serde_json::from_str::<WireEnvelope>(&with_host_epoch(&number)).is_err(),
            "extreme nonzero should be rejected"
        );
    }

    let oversized_frame = format!("{}\n", with_host_epoch(&format!("0e{exponent}")));
    assert!(matches!(
        NdjsonCodec::new(1024).push(oversized_frame.as_bytes()),
        Err(CodecError::FrameTooLarge { limit: 1024 })
    ));
}
#[test]
fn safe_wire_integer_max_roundtrips_and_overflow_encode_is_non_poisoning() {
    let mut max = request("safe-max");
    max.identity.host_epoch = MAX_SAFE_WIRE_INTEGER;
    max.identity.scope_generation = MAX_SAFE_WIRE_INTEGER;
    let codec = NdjsonCodec::new(2048);
    let encoded = codec.encode(&max).unwrap();
    let mut decoder = NdjsonCodec::new(2048);
    assert_eq!(decoder.push(&encoded).unwrap(), vec![max]);

    let mut unsafe_envelope = request("unsafe");
    unsafe_envelope.identity.host_epoch = MAX_SAFE_WIRE_INTEGER + 1;
    assert!(matches!(
        codec.encode(&unsafe_envelope),
        Err(CodecError::InvalidEnvelope(error))
            if error.to_string().contains("host_epoch value 9007199254740992")
                && error.to_string().contains("9007199254740991")
    ));
    let valid = request("after-unsafe-encode");
    assert!(codec.encode(&valid).is_ok());
}

#[test]
fn unsafe_inbound_integer_poison_decoder() {
    let text = serde_json::to_string(&request("unsafe-inbound"))
        .unwrap()
        .replace("\"host_epoch\":7", "\"host_epoch\":9007199254740992");
    let mut codec = NdjsonCodec::new(2048);
    let error = codec.push(format!("{text}\n").as_bytes()).unwrap_err();
    assert!(matches!(error, CodecError::InvalidEnvelope(error)
        if error.to_string().contains("host_epoch value 9007199254740992")
            && error.to_string().contains("9007199254740991")));
    assert!(matches!(codec.push(b"{}\n"), Err(CodecError::Failed)));
}

#[test]
fn ledger_rejects_unsafe_values_at_every_boundary_without_mutation() {
    assert_eq!(
        CallLedger::new(MAX_SAFE_WIRE_INTEGER + 1).err().unwrap(),
        LifecycleError::UnsafeWireInteger {
            field: "host_epoch",
            value: MAX_SAFE_WIRE_INTEGER + 1,
            max: MAX_SAFE_WIRE_INTEGER,
        }
    );

    let mut ledger = CallLedger::new(7).unwrap();
    assert_eq!(
        ledger.advance_scope_generation("plugin.example", "session-1", MAX_SAFE_WIRE_INTEGER + 1),
        Err(LifecycleError::UnsafeWireInteger {
            field: "scope_generation",
            value: MAX_SAFE_WIRE_INTEGER + 1,
            max: MAX_SAFE_WIRE_INTEGER,
        })
    );
    ledger
        .advance_scope_generation("plugin.example", "session-1", 3)
        .unwrap();

    let mut unsafe_call = identity("boundary-call");
    unsafe_call.scope_generation = MAX_SAFE_WIRE_INTEGER + 1;
    assert!(matches!(
        ledger.register_call(unsafe_call),
        Err(LifecycleError::UnsafeWireInteger {
            field: "scope_generation",
            ..
        })
    ));
    ledger.register_call(identity("boundary-call")).unwrap();

    let mut unsafe_terminal = terminal("boundary-call", TerminalStatus::Error);
    unsafe_terminal.identity.host_epoch = MAX_SAFE_WIRE_INTEGER + 1;
    assert!(matches!(
        ledger.submit_terminal(&unsafe_terminal),
        Err(LifecycleError::UnsafeWireInteger {
            field: "host_epoch",
            ..
        })
    ));
    assert!(matches!(
        ledger.submit_terminal(&terminal("boundary-call", TerminalStatus::Success)),
        Ok(TerminalDisposition::Committed(_))
    ));

    let mut max_ledger = CallLedger::new(MAX_SAFE_WIRE_INTEGER).unwrap();
    max_ledger
        .advance_scope_generation("p", "s", MAX_SAFE_WIRE_INTEGER)
        .unwrap();
    let max_identity = CallIdentity {
        host_epoch: MAX_SAFE_WIRE_INTEGER,
        plugin_id: "p".into(),
        scope_id: "s".into(),
        scope_generation: MAX_SAFE_WIRE_INTEGER,
        call_id: "max".into(),
    };
    max_ledger.register_call(max_identity.clone()).unwrap();
    assert!(matches!(
        max_ledger.submit_terminal(&WireEnvelope::new(
            max_identity,
            WireMessage::Terminal {
                status: TerminalStatus::Success,
                payload: json!(null).into()
            }
        )),
        Ok(TerminalDisposition::Committed(_))
    ));
}

#[test]
fn platform_control_identity_is_reserved_exact_and_roundtrips() {
    let control = CallIdentity::platform_control(7, "control-1").unwrap();
    assert!(control.is_platform_control());
    assert_eq!(control.plugin_id, PLATFORM_PLUGIN_ID);
    assert_eq!(control.scope_id, PLATFORM_CONTROL_SCOPE_ID);
    assert_eq!(control.scope_generation, PLATFORM_CONTROL_GENERATION);
    assert!(!identity("ordinary").is_platform_control());
    assert_eq!(
        CallIdentity::platform_control(MAX_SAFE_WIRE_INTEGER + 1, "bad"),
        Err(WireContractError::UnsafeInteger {
            field: "host_epoch",
            value: MAX_SAFE_WIRE_INTEGER + 1,
            max: MAX_SAFE_WIRE_INTEGER,
        })
    );

    let envelope = WireEnvelope::new(
        control,
        WireMessage::Request {
            method: "platform/initialize".into(),
            payload: json!(null).into(),
        },
    );
    let exact = "{\"protocol_version\":1,\"host_epoch\":7,\"plugin_id\":\"$rebon/platform\",\"scope_id\":\"$rebon/control\",\"scope_generation\":0,\"call_id\":\"control-1\",\"message\":{\"type\":\"request\",\"method\":\"platform/initialize\",\"payload\":null}}\n";
    let encoded = NdjsonCodec::new(1024).encode(&envelope).unwrap();
    assert_eq!(encoded, exact.as_bytes());
    assert_eq!(
        serde_json::from_slice::<WireEnvelope>(&encoded[..encoded.len() - 1]).unwrap(),
        envelope
    );
}

#[test]
fn cancel_shape_is_exact_and_intent_is_idempotent_without_terminal_ownership() {
    assert_eq!(CALL_CANCEL_METHOD, "call/cancel");
    let cancel = WireEnvelope::cancel(identity("cancel-target"));
    assert!(cancel.is_cancel());
    let exact = "{\"protocol_version\":1,\"host_epoch\":7,\"plugin_id\":\"plugin.example\",\"scope_id\":\"session-1\",\"scope_generation\":3,\"call_id\":\"cancel-target\",\"message\":{\"type\":\"notification\",\"method\":\"call/cancel\",\"payload\":null}}\n";
    assert_eq!(
        NdjsonCodec::new(1024).encode(&cancel).unwrap(),
        exact.as_bytes()
    );

    let mut ledger = ledger("cancel-target");
    assert_eq!(
        ledger.submit_cancel(&cancel),
        Ok(CancelDisposition::Accepted)
    );
    assert_eq!(
        ledger.submit_cancel(&cancel),
        Ok(CancelDisposition::Accepted)
    );
    assert!(matches!(
        ledger.submit_terminal(&terminal("cancel-target", TerminalStatus::Success)),
        Ok(TerminalDisposition::Committed(_))
    ));
    assert_eq!(
        ledger.submit_cancel(&cancel),
        Ok(CancelDisposition::Accepted)
    );
    assert!(matches!(
        ledger.submit_terminal(&terminal("cancel-target", TerminalStatus::Cancelled)),
        Err(LifecycleError::DuplicateTerminal {
            status: TerminalStatus::Success,
            ..
        })
    ));

    let malformed = WireEnvelope::new(
        identity("cancel-target"),
        WireMessage::Request {
            method: CALL_CANCEL_METHOD.into(),
            payload: json!(null).into(),
        },
    );
    assert!(matches!(
        ledger.submit_cancel(&malformed),
        Err(LifecycleError::NotCancel { .. })
    ));
}

#[test]
fn cancel_preserves_identity_and_stale_precedence() {
    let mut ledger = ledger("cancel-identity");
    let mut wrong_plugin = WireEnvelope::cancel(identity("cancel-identity"));
    wrong_plugin.identity.plugin_id = "other".into();
    assert!(matches!(
        ledger.submit_cancel(&wrong_plugin),
        Err(LifecycleError::IdentityMismatch {
            field: "plugin_id",
            ..
        })
    ));

    let mut old_epoch = WireEnvelope::cancel(identity("cancel-identity"));
    old_epoch.identity.host_epoch = 6;
    assert_eq!(
        ledger.submit_cancel(&old_epoch),
        Ok(CancelDisposition::Stale(StaleReason::HostEpoch {
            message: 6,
            current: 7
        }))
    );
    let mut old_generation = WireEnvelope::cancel(identity("cancel-identity"));
    old_generation.identity.scope_generation = 2;
    assert_eq!(
        ledger.submit_cancel(&old_generation),
        Ok(CancelDisposition::Stale(StaleReason::ScopeGeneration {
            message: 2,
            current: 3
        }))
    );

    ledger
        .advance_scope_generation("plugin.example", "session-1", 4)
        .unwrap();
    let mut wrong_target_generation = WireEnvelope::cancel(identity("cancel-identity"));
    wrong_target_generation.identity.scope_generation = 4;
    assert!(matches!(
        ledger.submit_cancel(&wrong_target_generation),
        Err(LifecycleError::IdentityMismatch {
            field: "scope_generation",
            ..
        })
    ));
}

#[test]
fn scope_close_reopen_generation_sequence_and_isolation() {
    assert_eq!(SCOPE_OPEN_METHOD, "scope/open");
    assert_eq!(SCOPE_CLOSE_METHOD, "scope/close");
    let mut ledger = CallLedger::new(7).unwrap();
    ledger
        .advance_scope_generation("plugin-a", "scope-a", 3)
        .unwrap();
    ledger
        .advance_scope_generation("plugin-a", "scope-a", 3)
        .unwrap();
    let old = CallIdentity {
        host_epoch: 7,
        plugin_id: "plugin-a".into(),
        scope_id: "scope-a".into(),
        scope_generation: 3,
        call_id: "old".into(),
    };
    ledger.register_call(old.clone()).unwrap();

    // Begin close: advance first; close and reopen both carry/reuse generation 4.
    ledger
        .advance_scope_generation("plugin-a", "scope-a", 4)
        .unwrap();
    let old_terminal = WireEnvelope::new(
        old,
        WireMessage::Terminal {
            status: TerminalStatus::Success,
            payload: json!(null).into(),
        },
    );
    assert_eq!(
        ledger.submit_terminal(&old_terminal),
        Ok(TerminalDisposition::Stale(StaleReason::ScopeGeneration {
            message: 3,
            current: 4
        }))
    );
    let reopened = CallIdentity {
        host_epoch: 7,
        plugin_id: "plugin-a".into(),
        scope_id: "scope-a".into(),
        scope_generation: 4,
        call_id: "reopened".into(),
    };
    ledger.register_call(reopened).unwrap();

    // Other plugin/scope generations remain independent.
    ledger
        .advance_scope_generation("plugin-b", "scope-a", 9)
        .unwrap();
    ledger
        .advance_scope_generation("plugin-a", "scope-b", 2)
        .unwrap();
    ledger
        .advance_scope_generation("plugin-a", "scope-a", 5)
        .unwrap();
    assert_eq!(
        ledger.advance_scope_generation("plugin-b", "scope-a", 9),
        Ok(())
    );
    assert_eq!(
        ledger.advance_scope_generation("plugin-a", "scope-b", 2),
        Ok(())
    );
}

#[test]
fn next_generation_is_safe_and_explicitly_exhausts() {
    assert_eq!(next_scope_generation(3), Ok(4));
    assert_eq!(
        next_scope_generation(MAX_SAFE_WIRE_INTEGER - 1),
        Ok(MAX_SAFE_WIRE_INTEGER)
    );
    assert_eq!(
        next_scope_generation(MAX_SAFE_WIRE_INTEGER),
        Err(LifecycleError::ScopeGenerationExhausted {
            current: MAX_SAFE_WIRE_INTEGER,
            max: MAX_SAFE_WIRE_INTEGER,
        })
    );
    assert!(matches!(
        next_scope_generation(MAX_SAFE_WIRE_INTEGER + 1),
        Err(LifecycleError::UnsafeWireInteger {
            field: "scope_generation",
            ..
        })
    ));
}

#[test]
fn plugin_scope_and_call_identity_mismatches_are_rejected() {
    let mut ledger = ledger("identity");

    let mut wrong_plugin = terminal("identity", TerminalStatus::Success);
    wrong_plugin.identity.plugin_id = "other-plugin".into();
    assert!(matches!(
        ledger.submit_terminal(&wrong_plugin),
        Err(LifecycleError::IdentityMismatch {
            field: "plugin_id",
            ..
        })
    ));

    let mut wrong_scope = terminal("identity", TerminalStatus::Success);
    wrong_scope.identity.scope_id = "other-scope".into();
    assert!(matches!(
        ledger.submit_terminal(&wrong_scope),
        Err(LifecycleError::IdentityMismatch {
            field: "scope_id",
            ..
        })
    ));

    let mut old_epoch = terminal("identity", TerminalStatus::Success);
    old_epoch.identity.host_epoch = 6;
    assert!(matches!(
        ledger.submit_terminal(&old_epoch),
        Ok(TerminalDisposition::Stale(StaleReason::HostEpoch { .. }))
    ));

    let mut future_epoch = terminal("identity", TerminalStatus::Success);
    future_epoch.identity.host_epoch = 8;
    assert!(matches!(
        ledger.submit_terminal(&future_epoch),
        Err(LifecycleError::FutureHostEpoch { .. })
    ));

    let mut future_generation = terminal("identity", TerminalStatus::Success);
    future_generation.identity.scope_generation = 4;
    assert!(matches!(
        ledger.submit_terminal(&future_generation),
        Err(LifecycleError::FutureScopeGeneration { .. })
    ));

    let wrong_call = terminal("different-call", TerminalStatus::Success);
    assert!(matches!(
        ledger.submit_terminal(&wrong_call),
        Err(LifecycleError::UnknownCall { .. })
    ));

    assert!(matches!(
        ledger.submit_terminal(&terminal("identity", TerminalStatus::Success)),
        Ok(TerminalDisposition::Committed(_))
    ));
}

/// A call may carry any number of chunks and still exactly one terminal: the
/// chunks never compete for the slot.
#[test]
fn chunks_do_not_consume_the_terminal_slot() {
    let mut ledger = ledger("stream");
    for _ in 0..3 {
        assert_eq!(
            ledger.submit_chunk(&chunk("stream")).unwrap(),
            ChunkDisposition::Accepted
        );
    }
    assert!(matches!(
        ledger.submit_terminal(&terminal("stream", TerminalStatus::Success)),
        Ok(TerminalDisposition::Committed(_))
    ));
}

/// A chunk after the end is not a late arrival to drop. A duplicate terminal is
/// a race two peers can legitimately lose; a chunk after the terminal means the
/// producer kept emitting after saying it was done, and delivering it would hand
/// a caller content for an answer it has finished reading.
#[test]
fn a_chunk_after_the_terminal_is_an_error() {
    let mut ledger = ledger("stream");
    ledger
        .submit_terminal(&terminal("stream", TerminalStatus::Success))
        .unwrap();
    assert_eq!(
        ledger.submit_chunk(&chunk("stream")),
        Err(LifecycleError::ChunkAfterTerminal {
            call_id: "stream".into(),
            status: TerminalStatus::Success,
        })
    );
}

#[test]
fn a_chunk_for_an_unregistered_call_is_refused() {
    let ledger = ledger("stream");
    assert_eq!(
        ledger.submit_chunk(&chunk("other")),
        Err(LifecycleError::UnknownCall {
            call_id: "other".into()
        })
    );
}

/// Same rule as every other frame: an incarnation that has been invalidated is
/// discarded rather than treated as a fault.
#[test]
fn a_chunk_from_a_past_scope_incarnation_is_stale() {
    let mut ledger = ledger("stream");
    ledger
        .advance_scope_generation("plugin.example", "session-1", 4)
        .unwrap();
    assert!(matches!(
        ledger.submit_chunk(&chunk("stream")).unwrap(),
        ChunkDisposition::Stale(StaleReason::ScopeGeneration { .. })
    ));
}

#[test]
fn a_non_chunk_message_is_not_accepted_as_one() {
    let ledger = ledger("stream");
    assert_eq!(
        ledger.submit_chunk(&request("stream")),
        Err(LifecycleError::NotChunk {
            call_id: "stream".into()
        })
    );
}

/// Chunks are validated, not recorded — a stream of a million must not grow the
/// ledger, and the proof is that the same chunk stays acceptable.
#[test]
fn chunks_leave_no_residue_in_the_ledger() {
    let ledger = ledger("stream");
    for _ in 0..1_000 {
        assert_eq!(
            ledger.submit_chunk(&chunk("stream")).unwrap(),
            ChunkDisposition::Accepted
        );
    }
}
