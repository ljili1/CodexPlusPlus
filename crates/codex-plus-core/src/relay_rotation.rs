/**
 * @description 聚合供应商轮转选择器，负责按失败、对话、请求和权重策略选择已有中转配置。
 * @author Albert_Luo
 * @email 480199976@qq.com
 * @date 2026-05-27 00:00
 */
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::settings::{
    AggregateRelayProfile, AggregateRelayStrategy, BackendSettings, RelayProfile,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    NoActiveAggregate,
    EmptyAggregateMembers {
        aggregate_id: String,
    },
    UnknownMemberRelay {
        aggregate_id: String,
        relay_id: String,
    },
    InvalidMemberRelay {
        aggregate_id: String,
        relay_id: String,
    },
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectionError::NoActiveAggregate => write!(formatter, "未找到当前聚合供应商"),
            SelectionError::EmptyAggregateMembers { aggregate_id } => {
                write!(formatter, "聚合供应商「{aggregate_id}」没有成员")
            }
            SelectionError::UnknownMemberRelay {
                aggregate_id,
                relay_id,
            } => write!(
                formatter,
                "聚合供应商「{aggregate_id}」引用了不存在的供应商「{relay_id}」"
            ),
            SelectionError::InvalidMemberRelay {
                aggregate_id,
                relay_id,
            } => write!(
                formatter,
                "聚合供应商「{aggregate_id}」成员「{relay_id}」缺少 API Base URL 或 Key"
            ),
        }
    }
}

impl std::error::Error for SelectionError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotationContext {
    pub conversation_id: Option<String>,
    /// 手动覆盖：优先于任何策略，直接路由到指定成员 `relay_id`（需为聚合成员）。
    pub override_relay_id: Option<String>,
    /// 手动覆盖：优先于成员默认模型，强制使用指定模型。
    pub override_model: Option<String>,
}

impl RotationContext {
    pub fn for_conversation(conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: Some(conversation_id.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationEvent {
    Success,
    Failure,
}

#[derive(Debug, Clone)]
pub struct RelayRotationSelector {
    aggregate: AggregateRelayProfile,
    failover_index: usize,
    request_index: usize,
    weighted_index: usize,
    conversation_assignments: HashMap<String, String>,
}

static GLOBAL_SELECTOR: OnceLock<Mutex<Option<RelayRotationSelector>>> = OnceLock::new();

impl RelayRotationSelector {
    pub fn from_settings(settings: &BackendSettings) -> Result<Self, SelectionError> {
        let aggregate = active_aggregate(settings)?.clone();
        validate_aggregate_members(settings, &aggregate)?;
        Ok(Self {
            aggregate,
            failover_index: 0,
            request_index: 0,
            weighted_index: 0,
            conversation_assignments: HashMap::new(),
        })
    }

    pub fn select(
        &mut self,
        settings: &BackendSettings,
        context: RotationContext,
    ) -> Result<RelayProfile, SelectionError> {
        validate_aggregate_members(settings, &self.aggregate)?;
        // 手动覆盖优先于任何策略；非法成员直接报错。
        let relay_id = if let Some(relay_id) = context.override_relay_id.clone() {
            if !self.is_member(&relay_id) {
                return Err(SelectionError::UnknownMemberRelay {
                    aggregate_id: self.aggregate.id.clone(),
                    relay_id,
                });
            }
            relay_id
        } else {
            match self.aggregate.strategy {
                AggregateRelayStrategy::Failover => self.member_id_at(self.failover_index),
                AggregateRelayStrategy::ConversationRoundRobin => {
                    self.select_for_conversation(context.conversation_id)
                }
                AggregateRelayStrategy::RequestRoundRobin => self.select_next_request(),
                AggregateRelayStrategy::WeightedRoundRobin => self.select_next_weighted(),
                AggregateRelayStrategy::Manual => self.manual_member_id(),
            }
        };
        let mut profile = relay_profile_by_id(settings, &relay_id)
            .ok_or_else(|| SelectionError::UnknownMemberRelay {
                aggregate_id: self.aggregate.id.clone(),
                relay_id,
            })?;
        if let Some(model) = context.override_model.clone().filter(|m| !m.trim().is_empty()) {
            profile.model = model;
        } else if !self.aggregate.model.trim().is_empty() {
            profile.model = self.aggregate.model.clone();
        }
        Ok(profile)
    }

    pub fn peek(&self, settings: &BackendSettings) -> Result<RelayProfile, SelectionError> {
        validate_aggregate_members(settings, &self.aggregate)?;
        let relay_id = match self.aggregate.strategy {
            AggregateRelayStrategy::Failover => self.member_id_at(self.failover_index),
            AggregateRelayStrategy::ConversationRoundRobin
            | AggregateRelayStrategy::RequestRoundRobin => self.member_id_at(self.request_index),
            AggregateRelayStrategy::WeightedRoundRobin => {
                let schedule = self.weighted_schedule();
                schedule[self.weighted_index % schedule.len()].clone()
            }
            AggregateRelayStrategy::Manual => self.manual_member_id(),
        };
        relay_profile_by_id(settings, &relay_id).ok_or_else(|| SelectionError::UnknownMemberRelay {
            aggregate_id: self.aggregate.id.clone(),
            relay_id,
        })
    }

    pub fn record_event(&mut self, event: RotationEvent) {
        if event == RotationEvent::Failure
            && self.aggregate.strategy == AggregateRelayStrategy::Failover
            && !self.aggregate.members.is_empty()
        {
            self.failover_index = (self.failover_index + 1) % self.aggregate.members.len();
        }
    }

    fn select_for_conversation(&mut self, conversation_id: Option<String>) -> String {
        let Some(conversation_id) = conversation_id else {
            return self.select_next_request();
        };
        if let Some(relay_id) = self.conversation_assignments.get(&conversation_id) {
            return relay_id.clone();
        }

        let relay_id = self.select_next_request();
        self.conversation_assignments
            .insert(conversation_id, relay_id.clone());
        relay_id
    }

    fn select_next_request(&mut self) -> String {
        let relay_id = self.member_id_at(self.request_index);
        self.request_index = (self.request_index + 1) % self.aggregate.members.len();
        relay_id
    }

    fn select_next_weighted(&mut self) -> String {
        let schedule = self.weighted_schedule();
        let relay_id = schedule[self.weighted_index % schedule.len()].clone();
        self.weighted_index = (self.weighted_index + 1) % schedule.len();
        relay_id
    }

    fn weighted_schedule(&self) -> Vec<String> {
        self.aggregate
            .members
            .iter()
            .flat_map(|member| {
                std::iter::repeat_n(member.relay_id.clone(), member.weight.max(1) as usize)
            })
            .collect()
    }

    fn member_id_at(&self, index: usize) -> String {
        self.aggregate.members[index % self.aggregate.members.len()]
            .relay_id
            .clone()
    }

    /// `Manual` 策略使用的固定成员：优先 `active_member_relay_id`，非法或为空时退回第一个成员。
    fn manual_member_id(&self) -> String {
        let chosen = self.aggregate.active_member_relay_id.trim();
        if !chosen.is_empty() && self.is_member(chosen) {
            chosen.to_string()
        } else {
            self.member_id_at(0)
        }
    }

    fn is_member(&self, relay_id: &str) -> bool {
        self.aggregate
            .members
            .iter()
            .any(|member| member.relay_id == relay_id)
    }
}

pub fn select_relay_for_request(
    settings: &BackendSettings,
    context: RotationContext,
) -> Result<RelayProfile, SelectionError> {
    let Some(active_aggregate) = settings.active_aggregate_relay_profile() else {
        clear_global_selector();
        return Ok(settings.active_relay_profile());
    };

    let lock = GLOBAL_SELECTOR.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let needs_new_selector = guard
        .as_ref()
        .map(|selector| selector.aggregate != active_aggregate)
        .unwrap_or(true);
    if needs_new_selector {
        *guard = Some(RelayRotationSelector::from_settings(settings)?);
    }
    // 逐对话绑定覆盖：conversation_id -> 成员 relay_id，优先于轮转策略。
    // 仅在调用方未通过模型前缀显式指定成员时才应用（模型前缀优先级最高）。
    let mut context = context;
    if context.override_relay_id.is_none() {
        if let Some(conversation_id) = context.conversation_id.clone() {
            if let Some(relay_id) = settings.conversation_relay_overrides.get(&conversation_id) {
                context.override_relay_id = Some(relay_id.clone());
            }
        }
    }
    guard
        .as_mut()
        .expect("selector initialized")
        .select(settings, context)
}

pub fn select_relay_for_probe(settings: &BackendSettings) -> Result<RelayProfile, SelectionError> {
    let Some(active_aggregate) = settings.active_aggregate_relay_profile() else {
        clear_global_selector();
        return Ok(settings.active_relay_profile());
    };

    let lock = GLOBAL_SELECTOR.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let needs_new_selector = guard
        .as_ref()
        .map(|selector| selector.aggregate != active_aggregate)
        .unwrap_or(true);
    if needs_new_selector {
        *guard = Some(RelayRotationSelector::from_settings(settings)?);
    }
    guard.as_ref().expect("selector initialized").peek(settings)
}

pub fn fallback_relays_after(
    settings: &BackendSettings,
    relay_id: &str,
) -> Result<Vec<RelayProfile>, SelectionError> {
    let Some(active_aggregate) = settings.active_aggregate_relay_profile() else {
        return Ok(Vec::new());
    };
    validate_aggregate_members(settings, &active_aggregate)?;
    let start_index = active_aggregate
        .members
        .iter()
        .position(|member| member.relay_id == relay_id)
        .map(|index| index + 1)
        .unwrap_or(0);
    (0..active_aggregate.members.len().saturating_sub(1))
        .map(|offset| {
            let index = (start_index + offset) % active_aggregate.members.len();
            &active_aggregate.members[index]
        })
        .map(|member| {
            relay_profile_by_id(settings, &member.relay_id).ok_or_else(|| {
                SelectionError::UnknownMemberRelay {
                    aggregate_id: active_aggregate.id.clone(),
                    relay_id: member.relay_id.clone(),
                }
            })
        })
        .collect()
}

pub fn record_relay_request_event(settings: &BackendSettings, event: RotationEvent) {
    if settings.active_aggregate_relay_profile().is_none() {
        clear_global_selector();
        return;
    }
    let lock = GLOBAL_SELECTOR.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(selector) = guard.as_mut() {
        selector.record_event(event);
    }
}

pub fn record_relay_request_failure(settings: &BackendSettings) {
    record_relay_request_event(settings, RotationEvent::Failure);
}

fn active_aggregate(settings: &BackendSettings) -> Result<&AggregateRelayProfile, SelectionError> {
    let active_id = settings
        .active_aggregate_relay_profile()
        .map(|aggregate| aggregate.id)
        .ok_or(SelectionError::NoActiveAggregate)?;

    settings
        .aggregate_relay_profiles
        .iter()
        .find(|aggregate| aggregate.id == active_id)
        .ok_or(SelectionError::NoActiveAggregate)
}

fn validate_aggregate_members(
    settings: &BackendSettings,
    aggregate: &AggregateRelayProfile,
) -> Result<(), SelectionError> {
    if aggregate.members.is_empty() {
        return Err(SelectionError::EmptyAggregateMembers {
            aggregate_id: aggregate.id.clone(),
        });
    }

    let relay_by_id = settings
        .relay_profiles
        .iter()
        .map(|profile| (profile.id.as_str(), profile))
        .collect::<HashMap<_, _>>();
    for member in &aggregate.members {
        let Some(relay) = relay_by_id.get(member.relay_id.as_str()) else {
            return Err(SelectionError::UnknownMemberRelay {
                aggregate_id: aggregate.id.clone(),
                relay_id: member.relay_id.clone(),
            });
        };
        if relay.base_url.trim().is_empty() || relay.api_key.trim().is_empty() {
            return Err(SelectionError::InvalidMemberRelay {
                aggregate_id: aggregate.id.clone(),
                relay_id: member.relay_id.clone(),
            });
        }
    }
    Ok(())
}

fn clear_global_selector() {
    let lock = GLOBAL_SELECTOR.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

fn relay_profile_by_id(settings: &BackendSettings, relay_id: &str) -> Option<RelayProfile> {
    settings
        .relay_profiles
        .iter()
        .find(|profile| profile.id == relay_id)
        .cloned()
}

/// 聚合模型目录使用的供应商前缀：优先使用成员配置名称（display name），
/// 名称为空时回退到 relay id。调用方需同时传入名称与 id，以便回退。
pub fn member_model_prefix(name: &str, relay_id: &str) -> String {
    let candidate = name.trim();
    if candidate.is_empty() {
        relay_id.trim().to_string()
    } else {
        candidate.to_string()
    }
}

/// 解析形如 `prefix/model` 的请求模型：
/// - 当聚合已激活且前缀路由开关开启时，候选为聚合成员；
/// - 当全局聚合开关（`universal_model_catalog_enabled`）开启时，候选为【所有】已配置供应商，
///   无需创建聚合供应商；
/// - 若 `prefix` 精确（或大小写不敏感）命中某个候选供应商的 **配置名称** 或 **relay id**，
///   返回 `(供应商 relay_id, 去掉前缀后的裸模型名)`，供上游按裸模型名请求。
///
/// 两个开关都关闭、无 `/` 分隔或前缀未命中任何候选时返回 `None`，
/// 由调用方回退到原有的轮转/故障转移策略。
pub fn resolve_prefixed_model(settings: &BackendSettings, model: &str) -> Option<(String, String)> {
    let (prefix, rest) = model.split_once('/')?;
    let prefix = prefix.trim();
    let rest = rest.trim();
    if prefix.is_empty() || rest.is_empty() {
        return None;
    }

    // 收集候选供应商：全局聚合（universal）优先，表示「所有」已配置供应商，
    // 无需聚合供应商；否则若聚合前缀开关开启且存在激活聚合，则使用聚合成员。
    let candidates: Vec<RelayProfile> = if settings.universal_model_catalog_enabled {
        settings.relay_profiles.clone()
    } else if settings.aggregate_prefixed_catalog_enabled {
        match settings.active_aggregate_relay_profile() {
            Some(aggregate) => aggregate
                .members
                .iter()
                .filter_map(|member| {
                    settings
                        .relay_profiles
                        .iter()
                        .find(|profile| profile.id == member.relay_id)
                        .cloned()
                })
                .collect(),
            None => Vec::new(),
        }
    } else {
        return None;
    };
    if candidates.is_empty() {
        return None;
    }

    let find_match = |case_insensitive: bool| -> Option<String> {
        candidates
            .iter()
            .find(|profile| {
                let name_prefix = member_model_prefix(&profile.name, &profile.id);
                if case_insensitive {
                    name_prefix.eq_ignore_ascii_case(prefix) || profile.id.eq_ignore_ascii_case(prefix)
                } else {
                    name_prefix == prefix || profile.id == prefix
                }
            })
            .map(|profile| profile.id.clone())
    };

    find_match(false)
        .or_else(|| find_match(true))
        .map(|relay_id| (relay_id, rest.to_string()))
}
