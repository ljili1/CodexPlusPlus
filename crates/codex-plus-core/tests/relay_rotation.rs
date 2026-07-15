use codex_plus_core::relay_rotation::{
    RelayRotationSelector, RotationContext, RotationEvent, SelectionError, fallback_relays_after,
    member_model_prefix, record_relay_request_failure, resolve_prefixed_model,
    select_relay_for_probe, select_relay_for_request,
};
use codex_plus_core::settings::{
    AggregateRelayMember, AggregateRelayProfile, AggregateRelayStrategy, BackendSettings,
    RelayMode, RelayProfile,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn global_selector_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn profile(id: &str) -> RelayProfile {
    RelayProfile {
        id: id.to_string(),
        name: id.to_string(),
        base_url: format!("https://{id}.example/v1"),
        api_key: format!("sk-{id}"),
        ..RelayProfile::default()
    }
}

fn aggregate(strategy: AggregateRelayStrategy) -> AggregateRelayProfile {
    AggregateRelayProfile {
        id: "agg".to_string(),
        name: "聚合".to_string(),
        strategy,
        members: vec![
            AggregateRelayMember {
                relay_id: "relay-a".to_string(),
                weight: 1,
            },
            AggregateRelayMember {
                relay_id: "relay-b".to_string(),
                weight: 2,
            },
            AggregateRelayMember {
                relay_id: "relay-c".to_string(),
                weight: 1,
            },
        ],
        active_member_relay_id: String::new(),
        model: String::new(),
    }
}

fn aggregate_with_id(id: &str, strategy: AggregateRelayStrategy) -> AggregateRelayProfile {
    AggregateRelayProfile {
        id: id.to_string(),
        name: "聚合".to_string(),
        strategy,
        members: vec![
            AggregateRelayMember {
                relay_id: "relay-a".to_string(),
                weight: 1,
            },
            AggregateRelayMember {
                relay_id: "relay-b".to_string(),
                weight: 2,
            },
        ],
        active_member_relay_id: String::new(),
        model: String::new(),
    }
}

fn settings(strategy: AggregateRelayStrategy) -> BackendSettings {
    BackendSettings {
        relay_profiles: vec![
            profile("relay-a"),
            profile("relay-b"),
            profile("relay-c"),
            RelayProfile {
                id: "agg".to_string(),
                name: "聚合".to_string(),
                relay_mode: RelayMode::Aggregate,
                ..RelayProfile::default()
            },
        ],
        aggregate_relay_profiles: vec![aggregate(strategy)],
        active_relay_id: "agg".to_string(),
        active_aggregate_relay_id: "agg".to_string(),
        ..BackendSettings::default()
    }
}

#[test]
fn failover_keeps_current_provider_until_failure_then_moves_to_next_member() {
    let settings = settings(AggregateRelayStrategy::Failover);
    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();

    let first = selector
        .select(&settings, RotationContext::for_conversation("chat-1"))
        .unwrap();
    selector.record_event(RotationEvent::Success);
    let second = selector
        .select(&settings, RotationContext::for_conversation("chat-1"))
        .unwrap();
    selector.record_event(RotationEvent::Failure);
    let third = selector
        .select(&settings, RotationContext::for_conversation("chat-1"))
        .unwrap();

    assert_eq!(first.id, "relay-a");
    assert_eq!(second.id, "relay-a");
    assert_eq!(third.id, "relay-b");
}

#[test]
fn conversation_rotation_sticks_each_conversation_to_a_stable_member() {
    let settings = settings(AggregateRelayStrategy::ConversationRoundRobin);
    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();

    let chat_a_first = selector
        .select(&settings, RotationContext::for_conversation("chat-a"))
        .unwrap();
    let chat_a_second = selector
        .select(&settings, RotationContext::for_conversation("chat-a"))
        .unwrap();
    let chat_b_first = selector
        .select(&settings, RotationContext::for_conversation("chat-b"))
        .unwrap();

    assert_eq!(chat_a_first.id, "relay-a");
    assert_eq!(chat_a_second.id, "relay-a");
    assert_eq!(chat_b_first.id, "relay-b");
}

#[test]
fn request_rotation_advances_on_every_request() {
    let settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();

    let selected = (0..5)
        .map(|_| {
            selector
                .select(&settings, RotationContext::default())
                .unwrap()
                .id
        })
        .collect::<Vec<_>>();

    assert_eq!(
        selected,
        vec!["relay-a", "relay-b", "relay-c", "relay-a", "relay-b"]
    );
}

#[test]
fn weighted_rotation_repeats_members_by_configured_weight() {
    let settings = settings(AggregateRelayStrategy::WeightedRoundRobin);
    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();

    let selected = (0..6)
        .map(|_| {
            selector
                .select(&settings, RotationContext::default())
                .unwrap()
                .id
        })
        .collect::<Vec<_>>();

    assert_eq!(
        selected,
        vec![
            "relay-a", "relay-b", "relay-b", "relay-c", "relay-a", "relay-b"
        ]
    );
}

#[test]
fn aggregate_members_must_reference_existing_relay_profiles() {
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings.aggregate_relay_profiles[0]
        .members
        .push(AggregateRelayMember {
            relay_id: "missing-relay".to_string(),
            weight: 1,
        });

    let error = RelayRotationSelector::from_settings(&settings).unwrap_err();

    assert_eq!(
        error,
        SelectionError::UnknownMemberRelay {
            aggregate_id: "agg".to_string(),
            relay_id: "missing-relay".to_string()
        }
    );
}

#[test]
fn aggregate_with_one_member_is_allowed_without_rotation() {
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings.aggregate_relay_profiles[0].members.truncate(1);

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let first = selector
        .select(&settings, RotationContext::default())
        .unwrap();
    let second = selector
        .select(&settings, RotationContext::default())
        .unwrap();

    assert_eq!(first.id, "relay-a");
    assert_eq!(second.id, "relay-a");
}

#[test]
fn aggregate_members_must_be_api_capable_relay_profiles() {
    let mut settings = settings(AggregateRelayStrategy::WeightedRoundRobin);
    settings.relay_profiles.push(RelayProfile {
        id: "official-login".to_string(),
        name: "官方登录".to_string(),
        base_url: String::new(),
        api_key: String::new(),
        ..RelayProfile::default()
    });
    settings.aggregate_relay_profiles[0]
        .members
        .push(AggregateRelayMember {
            relay_id: "official-login".to_string(),
            weight: 1,
        });

    let error = RelayRotationSelector::from_settings(&settings).unwrap_err();

    assert_eq!(
        error,
        SelectionError::InvalidMemberRelay {
            aggregate_id: "agg".to_string(),
            relay_id: "official-login".to_string()
        }
    );
}

#[test]
fn select_relay_for_request_uses_active_relay_id_as_aggregate_source_of_truth() {
    let _guard = global_selector_test_lock();
    let mut settings = settings(AggregateRelayStrategy::WeightedRoundRobin);
    settings.active_relay_id = "agg".to_string();
    settings.active_aggregate_relay_id.clear();

    let selected = select_relay_for_request(&settings, RotationContext::default()).unwrap();

    assert_eq!(selected.id, "relay-a");
}

#[test]
fn select_relay_for_request_ignores_stale_active_aggregate_id_for_regular_relay() {
    let _guard = global_selector_test_lock();
    let mut settings = settings(AggregateRelayStrategy::WeightedRoundRobin);
    settings.active_relay_id = "relay-b".to_string();
    settings.active_aggregate_relay_id = "agg".to_string();

    let selected = select_relay_for_request(&settings, RotationContext::default()).unwrap();

    assert_eq!(selected.id, "relay-b");
}

#[test]
fn select_relay_for_request_resets_rotation_after_switching_to_regular_relay() {
    let _guard = global_selector_test_lock();
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings.active_relay_id = "agg".to_string();

    let first = select_relay_for_request(&settings, RotationContext::default()).unwrap();
    let mut regular_settings = settings.clone();
    regular_settings.active_relay_id = "relay-c".to_string();
    regular_settings.active_aggregate_relay_id.clear();
    let regular = select_relay_for_request(&regular_settings, RotationContext::default()).unwrap();
    let after_reselect = select_relay_for_request(&settings, RotationContext::default()).unwrap();

    assert_eq!(first.id, "relay-a");
    assert_eq!(regular.id, "relay-c");
    assert_eq!(after_reselect.id, "relay-a");
}

#[test]
fn record_relay_request_failure_advances_global_failover_selector() {
    let _guard = global_selector_test_lock();
    let aggregate_id = "agg-global-failure";
    let settings = BackendSettings {
        relay_profiles: vec![
            profile("relay-a"),
            profile("relay-b"),
            RelayProfile {
                id: aggregate_id.to_string(),
                name: "聚合".to_string(),
                relay_mode: RelayMode::Aggregate,
                ..RelayProfile::default()
            },
        ],
        aggregate_relay_profiles: vec![aggregate_with_id(
            aggregate_id,
            AggregateRelayStrategy::Failover,
        )],
        active_relay_id: aggregate_id.to_string(),
        active_aggregate_relay_id: aggregate_id.to_string(),
        ..BackendSettings::default()
    };

    let first = select_relay_for_request(&settings, RotationContext::default()).unwrap();
    record_relay_request_failure(&settings);
    let second = select_relay_for_request(&settings, RotationContext::default()).unwrap();

    assert_eq!(first.id, "relay-a");
    assert_eq!(second.id, "relay-b");
}

#[test]
fn select_relay_for_probe_does_not_advance_request_rotation() {
    let _guard = global_selector_test_lock();
    let aggregate_id = "agg-probe";
    let settings = BackendSettings {
        relay_profiles: vec![
            profile("relay-a"),
            profile("relay-b"),
            RelayProfile {
                id: aggregate_id.to_string(),
                name: "聚合".to_string(),
                relay_mode: RelayMode::Aggregate,
                ..RelayProfile::default()
            },
        ],
        aggregate_relay_profiles: vec![aggregate_with_id(
            aggregate_id,
            AggregateRelayStrategy::RequestRoundRobin,
        )],
        active_relay_id: aggregate_id.to_string(),
        active_aggregate_relay_id: aggregate_id.to_string(),
        ..BackendSettings::default()
    };

    let first_probe = select_relay_for_probe(&settings).unwrap();
    let second_probe = select_relay_for_probe(&settings).unwrap();
    let first_request = select_relay_for_request(&settings, RotationContext::default()).unwrap();
    let second_request = select_relay_for_request(&settings, RotationContext::default()).unwrap();

    assert_eq!(first_probe.id, "relay-a");
    assert_eq!(second_probe.id, "relay-a");
    assert_eq!(first_request.id, "relay-a");
    assert_eq!(second_request.id, "relay-b");
}

#[test]
fn fallback_relays_after_returns_remaining_aggregate_members_after_current_then_wraps() {
    let settings = settings(AggregateRelayStrategy::RequestRoundRobin);

    let fallbacks = fallback_relays_after(&settings, "relay-b").unwrap();

    assert_eq!(
        fallbacks
            .iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>(),
        vec!["relay-c", "relay-a"]
    );
}

#[test]
fn fallback_relays_after_regular_relay_returns_empty_candidates() {
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings.active_relay_id = "relay-a".to_string();

    let fallbacks = fallback_relays_after(&settings, "relay-a").unwrap();

    assert!(fallbacks.is_empty());
}

#[test]
fn select_relay_for_request_rebuilds_selector_when_active_aggregate_changes() {
    let _guard = global_selector_test_lock();
    let aggregate_id = "agg-refresh";
    let mut settings = BackendSettings {
        relay_profiles: vec![
            profile("relay-a"),
            profile("relay-b"),
            RelayProfile {
                id: aggregate_id.to_string(),
                name: "聚合".to_string(),
                relay_mode: RelayMode::Aggregate,
                ..RelayProfile::default()
            },
        ],
        aggregate_relay_profiles: vec![aggregate_with_id(
            aggregate_id,
            AggregateRelayStrategy::Failover,
        )],
        active_relay_id: aggregate_id.to_string(),
        active_aggregate_relay_id: aggregate_id.to_string(),
        ..BackendSettings::default()
    };

    let first = select_relay_for_request(&settings, RotationContext::default()).unwrap();
    settings.aggregate_relay_profiles[0].strategy = AggregateRelayStrategy::WeightedRoundRobin;

    let selected = (0..3)
        .map(|_| {
            select_relay_for_request(&settings, RotationContext::default())
                .unwrap()
                .id
        })
        .collect::<Vec<_>>();

    assert_eq!(first.id, "relay-a");
    assert_eq!(selected, vec!["relay-a", "relay-b", "relay-b"]);
}

#[test]
fn manual_strategy_pins_configured_active_member() {
    let mut settings = settings(AggregateRelayStrategy::Manual);
    settings.aggregate_relay_profiles[0].active_member_relay_id = "relay-b".to_string();

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let first = selector
        .select(&settings, RotationContext::for_conversation("chat-1"))
        .unwrap();
    let second = selector
        .select(&settings, RotationContext::for_conversation("chat-1"))
        .unwrap();

    assert_eq!(first.id, "relay-b");
    assert_eq!(second.id, "relay-b");
}

#[test]
fn manual_strategy_falls_back_to_first_member_when_unset() {
    let settings = settings(AggregateRelayStrategy::Manual);

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let selected = selector
        .select(&settings, RotationContext::default())
        .unwrap();

    assert_eq!(selected.id, "relay-a");
}

#[test]
fn override_relay_id_takes_precedence_over_strategy() {
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings.aggregate_relay_profiles[0].active_member_relay_id = "relay-a".to_string();

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let overridden = selector
        .select(
            &settings,
            RotationContext {
                override_relay_id: Some("relay-c".to_string()),
                ..RotationContext::default()
            },
        )
        .unwrap();

    assert_eq!(overridden.id, "relay-c");
}

#[test]
fn override_model_is_applied_to_selected_profile() {
    let settings = settings(AggregateRelayStrategy::Manual);

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let selected = selector
        .select(
            &settings,
            RotationContext {
                override_relay_id: Some("relay-b".to_string()),
                override_model: Some("gpt-4o".to_string()),
                ..RotationContext::default()
            },
        )
        .unwrap();

    assert_eq!(selected.id, "relay-b");
    assert_eq!(selected.model, "gpt-4o");
}

#[test]
fn override_with_unknown_member_errors() {
    let settings = settings(AggregateRelayStrategy::Manual);

    let mut selector = RelayRotationSelector::from_settings(&settings).unwrap();
    let error = selector
        .select(
            &settings,
            RotationContext {
                override_relay_id: Some("relay-z".to_string()),
                ..RotationContext::default()
            },
        )
        .unwrap_err();

    assert_eq!(
        error,
        SelectionError::UnknownMemberRelay {
            aggregate_id: "agg".to_string(),
            relay_id: "relay-z".to_string()
        }
    );
}

#[test]
fn conversation_override_via_settings_map_routes_request() {
    let _guard = global_selector_test_lock();
    let mut settings = settings(AggregateRelayStrategy::RequestRoundRobin);
    settings
        .conversation_relay_overrides
        .insert("chat-x".to_string(), "relay-c".to_string());

    let selected = select_relay_for_request(
        &settings,
        RotationContext {
            conversation_id: Some("chat-x".to_string()),
            ..RotationContext::default()
        },
    )
    .unwrap();

    assert_eq!(selected.id, "relay-c");
}

#[test]
fn member_model_prefix_prefers_name_then_fallback_to_id() {
    assert_eq!(member_model_prefix("DeepSeek", "relay-a"), "DeepSeek");
    assert_eq!(member_model_prefix("", "relay-a"), "relay-a");
    assert_eq!(member_model_prefix("  ", "relay-a"), "relay-a");
}

#[test]
fn resolve_prefixed_model_routes_prefix_by_name_or_id() {
    let settings = settings(AggregateRelayStrategy::Failover);
    // relay-b has name "relay-b" in the test fixture, so both id and name match.
    let resolved = resolve_prefixed_model(&settings, "relay-b/gpt-5").unwrap();
    assert_eq!(resolved.0, "relay-b");
    assert_eq!(resolved.1, "gpt-5");
}

#[test]
fn resolve_prefixed_model_is_case_insensitive_on_prefix() {
    let settings = settings(AggregateRelayStrategy::Failover);
    let resolved = resolve_prefixed_model(&settings, "RELAY-A/gpt-4").unwrap();
    assert_eq!(resolved.0, "relay-a");
    assert_eq!(resolved.1, "gpt-4");
}

#[test]
fn resolve_prefixed_model_falls_back_when_no_prefix() {
    let settings = settings(AggregateRelayStrategy::Failover);
    assert!(resolve_prefixed_model(&settings, "gpt-5").is_none());
}

#[test]
fn resolve_prefixed_model_returns_none_for_unknown_prefix() {
    let settings = settings(AggregateRelayStrategy::Failover);
    assert!(resolve_prefixed_model(&settings, "unknown/gpt-5").is_none());
}

#[test]
fn resolve_prefixed_model_disabled_when_toggle_off() {
    let mut settings = settings(AggregateRelayStrategy::Failover);
    settings.aggregate_prefixed_catalog_enabled = false;
    assert!(resolve_prefixed_model(&settings, "relay-b/gpt-5").is_none());
}

#[test]
fn resolve_prefixed_model_disabled_without_active_aggregate() {
    let mut settings = settings(AggregateRelayStrategy::Failover);
    settings.aggregate_relay_profiles.clear();
    assert!(resolve_prefixed_model(&settings, "relay-b/gpt-5").is_none());
}

#[test]
fn resolve_prefixed_model_universal_aggregates_all_profiles_without_aggregate() {
    let mut settings = settings(AggregateRelayStrategy::Failover);
    settings.universal_model_catalog_enabled = true;
    settings.aggregate_prefixed_catalog_enabled = false;
    // relay-c 是已配置供应商但不是聚合成员，开启全局聚合后也能按前缀路由。
    let resolved = resolve_prefixed_model(&settings, "relay-c/gpt-5").unwrap();
    assert_eq!(resolved.0, "relay-c");
    assert_eq!(resolved.1, "gpt-5");
    // 聚合成员同样可路由。
    let resolved_a = resolve_prefixed_model(&settings, "relay-a/gpt-4").unwrap();
    assert_eq!(resolved_a.0, "relay-a");
    assert_eq!(resolved_a.1, "gpt-4");
}

#[test]
fn resolve_prefixed_model_universal_routes_by_display_name_prefix() {
    let mut settings = settings(AggregateRelayStrategy::Failover);
    settings.universal_model_catalog_enabled = true;
    settings.aggregate_prefixed_catalog_enabled = false;
    settings.relay_profiles[0].name = "DeepSeek".to_string();
    // 前缀优先匹配配置名称。
    let resolved = resolve_prefixed_model(&settings, "DeepSeek/gpt-5").unwrap();
    assert_eq!(resolved.0, "relay-a");
    assert_eq!(resolved.1, "gpt-5");
}

#[test]
fn resolve_prefixed_model_universal_disabled_when_toggle_off() {
    let mut settings = settings(AggregateRelayStrategy::Failover);
    settings.universal_model_catalog_enabled = false;
    settings.aggregate_prefixed_catalog_enabled = false;
    assert!(resolve_prefixed_model(&settings, "relay-b/gpt-5").is_none());
}
