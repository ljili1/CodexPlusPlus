//! 「统一模型目录」相关测试。
//!
//! 单独成文件是有意的：这些用例要改写 `CODEX_HOME` 与测试用 settings 路径，
//! 而 Rust 的集成测试按文件编译成独立二进制、并行跑在不同进程里，
//! 放进 `model_catalog.rs` 会和该文件里已有的用例抢同一份全局状态。

use codex_plus_core::relay_rotation::{member_model_prefix, resolve_prefixed_model};
use codex_plus_core::settings::{
    BackendSettings, RelayMode, RelayProfile, RelayProtocol, SettingsStore,
};

fn profile(id: &str, name: &str, model_list: &str) -> RelayProfile {
    RelayProfile {
        id: id.to_string(),
        name: name.to_string(),
        model: model_list.lines().next().unwrap_or("").to_string(),
        base_url: format!("https://{id}.test/v1"),
        protocol: RelayProtocol::Responses,
        relay_mode: RelayMode::MixedApi,
        model_list: model_list.to_string(),
        ..RelayProfile::default()
    }
}

fn settings_with(profiles: Vec<RelayProfile>, enabled: bool) -> BackendSettings {
    BackendSettings {
        active_relay_id: profiles
            .first()
            .map(|item| item.id.clone())
            .unwrap_or_default(),
        universal_model_catalog_enabled: enabled,
        relay_profiles: profiles,
        ..BackendSettings::default()
    }
}

#[test]
fn member_model_prefix_falls_back_to_id_then_empty() {
    assert_eq!(member_model_prefix("Relay A", "relay-a"), "Relay A/");
    // 显示名为空时回退到 id。
    assert_eq!(member_model_prefix("  ", "relay-a"), "relay-a/");
    // 两者皆空则不参与前缀匹配。
    assert_eq!(member_model_prefix("", ""), "");
    // 显示名两侧的空白要去掉。
    assert_eq!(member_model_prefix("  Relay A  ", "relay-a"), "Relay A/");
}

#[test]
fn resolve_prefixed_model_is_disabled_and_case_insensitive_aware() {
    let settings = settings_with(
        vec![
            profile("relay-a", "Relay A", "qwen3-coder\ndeepseek-coder"),
            profile("relay-b", "Relay B", "gpt-5"),
        ],
        true,
    );

    // 精确命中：返回供应商 id + 去掉前缀的模型名。
    assert_eq!(
        resolve_prefixed_model(&settings, "Relay B/gpt-5"),
        Some(("relay-b".to_string(), "gpt-5".to_string()))
    );

    // 大小写不敏感回退。
    assert_eq!(
        resolve_prefixed_model(&settings, "relay a/qwen3-coder"),
        Some(("relay-a".to_string(), "qwen3-coder".to_string()))
    );

    // 无前缀的模型不解析。
    assert_eq!(resolve_prefixed_model(&settings, "gpt-5"), None);
    // 空模型名直接短路。
    assert_eq!(resolve_prefixed_model(&settings, ""), None);

    // 关闭开关后一律不解析。
    let disabled = settings_with(
        vec![profile("relay-a", "Relay A", "qwen3-coder")],
        false,
    );
    assert_eq!(resolve_prefixed_model(&disabled, "Relay A/qwen3-coder"), None);
}

#[test]
fn resolve_prefixed_model_keeps_non_ascii_model_names_intact() {
    let settings = settings_with(
        vec![profile("relay-a", "我的供应商", "通义千问-Plus")],
        true,
    );
    // 大小写不敏感匹配要按字符数截断，不能把多字节字符切坏。
    assert_eq!(
        resolve_prefixed_model(&settings, "我的供应商/通义千问-Plus"),
        Some(("relay-a".to_string(), "通义千问-Plus".to_string()))
    );
}

#[tokio::test]
async fn model_catalog_merges_all_providers_with_prefix_when_universal_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let settings_path = temp.path().join("settings.json");
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    let previous_settings_path =
        codex_plus_core::paths::set_settings_path_for_tests(Some(settings_path.clone()));
    unsafe {
        std::env::set_var("CODEX_HOME", &codex_home);
    }

    let result = async {
        SettingsStore::new(settings_path)
            .save(&settings_with(
                vec![
                    profile("relay-a", "Relay A", "qwen3-coder\ndeepseek-coder"),
                    profile("relay-b", "Relay B", "gpt-5"),
                ],
                true,
            ))
            .unwrap();

        codex_plus_core::model_catalog::read_codex_model_catalog().await
    }
    .await;

    match previous_codex_home {
        Some(value) => unsafe {
            std::env::set_var("CODEX_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("CODEX_HOME");
        },
    }
    codex_plus_core::paths::set_settings_path_for_tests(previous_settings_path);

    assert_eq!(result["status"], "ok");
    assert_eq!(result["model_provider"], "");
    let models = result["models"].as_array().expect("models array");
    let ids: Vec<&str> = models.iter().filter_map(|value| value.as_str()).collect();
    assert!(
        ids.contains(&"Relay A/qwen3-coder"),
        "missing Relay A prefix: {ids:?}"
    );
    assert!(
        ids.contains(&"Relay A/deepseek-coder"),
        "missing Relay A prefix: {ids:?}"
    );
    assert!(
        ids.contains(&"Relay B/gpt-5"),
        "missing Relay B prefix: {ids:?}"
    );
    // 顺序与供应商配置顺序一致，默认模型取第一个。
    assert_eq!(ids, ["Relay A/qwen3-coder", "Relay A/deepseek-coder", "Relay B/gpt-5"]);
    assert_eq!(result["default_model"], "Relay A/qwen3-coder");
    let sources = result["sources"].as_array().expect("sources array");
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["type"], "relay_profile_model_list");
    assert_eq!(sources[0]["name"], "Relay A");
    assert_eq!(sources[1]["name"], "Relay B");
}

#[tokio::test]
async fn model_catalog_reports_not_configured_when_universal_has_no_models() {
    let temp = tempfile::tempdir().unwrap();
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let settings_path = temp.path().join("settings.json");
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    let previous_settings_path =
        codex_plus_core::paths::set_settings_path_for_tests(Some(settings_path.clone()));
    unsafe {
        std::env::set_var("CODEX_HOME", &codex_home);
    }

    let result = async {
        SettingsStore::new(settings_path)
            .save(&settings_with(Vec::new(), true))
            .unwrap();
        codex_plus_core::model_catalog::read_codex_model_catalog().await
    }
    .await;

    match previous_codex_home {
        Some(value) => unsafe {
            std::env::set_var("CODEX_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("CODEX_HOME");
        },
    }
    codex_plus_core::paths::set_settings_path_for_tests(previous_settings_path);

    assert_eq!(result["status"], "not_configured");
    assert_eq!(result["models"].as_array().map(Vec::len), Some(0));
}
