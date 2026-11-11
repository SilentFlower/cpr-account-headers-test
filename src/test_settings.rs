//! 保存统一测试分组，并通过原生管理接口复用专用测试 Key

use std::collections::BTreeMap;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::{AccessQuery, AccountFacts, ClientKeyFactsQuery},
        host::{
            STATE_GET, STATE_PUT, StateGetRequest, StateGetResult, StateMigrationChange,
            StateMigrationRequest, StateMigrationResult, StatePutRequest, StatePutResult,
        },
        resources::{GroupEnsureRequest, GroupMembersChange},
    },
    client::{HostClient, SessionError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::host_json;

/// 可选已有 Key 或共享检测分组，版本用于保护并发编辑
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Settings {
    pub(crate) key_id: Option<String>,
    pub(crate) group_id: Option<String>,
    pub(crate) version: Option<String>,
    #[serde(skip)]
    managed_keys: BTreeMap<String, String>,
}

/// 原生分组选择项
#[derive(Serialize, Deserialize)]
pub(crate) struct Group {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
}

/// 读取统一设置及其精确版本
/// @param host 当前管理调用
/// @return 已保存设置，首次读取使用自动创建模式
pub(crate) async fn load(host: &HostClient) -> Result<Settings, PluginFault> {
    let reply = host
        .call(
            STATE_GET,
            serde_json::to_value(StateGetRequest {
                namespace: "testing".into(),
                key: "settings".into(),
            })
            .map_err(|_| unavailable())?,
            Vec::new(),
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let record: StateGetResult = serde_json::from_value(reply.result).map_err(|_| unavailable())?;
    let Some(record) = record.record else {
        return Ok(Settings::default());
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Stored {
        key_id: Option<String>,
        group_id: Option<String>,
        #[serde(default)]
        managed_keys: BTreeMap<String, String>,
    }
    let stored: Stored = serde_json::from_value(record.value).map_err(|_| unavailable())?;
    Ok(Settings {
        key_id: stored.key_id,
        group_id: stored.group_id,
        version: Some(record.version.to_string()),
        managed_keys: stored.managed_keys,
    })
}

/// 保存全账号共用的测试分组，已有组的成员由管理员维护
/// @param host 当前管理调用
/// @param input 页面读取后提交的设置
/// @return 持久化设置与新版本
pub(crate) async fn save(host: &HostClient, mut input: Settings) -> Result<Settings, PluginFault> {
    if let Some(id) = &input.key_id {
        if id.is_empty() || id.len() > 128 {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "请选择有效的检测 API Key",
            ));
        }
        let facts = host
            .key_facts(ClientKeyFactsQuery {
                client_key_id: id.clone(),
            })
            .await?;
        if !facts.enabled {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "所选检测 API Key 已停用",
            ));
        }
    }
    if input.key_id.is_none()
        && let Some(id) = &input.group_id
        && (id.is_empty()
            || id.len() > 128
            || !groups(host).await?.iter().any(|g| g.id == *id && g.enabled))
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "请选择已启用的测试分组",
        ));
    }
    input.managed_keys = load(host).await?.managed_keys;
    put(host, input).await
}

/// 配置和受管 Key 身份在同一记录中按版本原子保存
async fn put(host: &HostClient, input: Settings) -> Result<Settings, PluginFault> {
    let version = input
        .version
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| PluginFault::new(ErrorCode::InvalidInput, "测试设置版本无效"))
        })
        .transpose()?;
    let reply = host
        .call(
            STATE_PUT,
            serde_json::to_value(StatePutRequest {
                namespace: "testing".into(),
                key: "settings".into(),
                value: json!({"keyId":input.key_id,"groupId":input.group_id,"managedKeys":input.managed_keys}),
                expected_version: version,
            })
            .map_err(|_| unavailable())?,
            Vec::new(),
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let saved: StatePutResult = serde_json::from_value(reply.result).map_err(|_| unavailable())?;
    Ok(Settings {
        key_id: input.key_id,
        group_id: input.group_id,
        version: Some(saved.version.to_string()),
        managed_keys: input.managed_keys,
    })
}

/// 按原生分页合同读取全部分组选择项
/// @param host 当前管理调用
/// @return 包含停用状态的分组，失败时中止本次设置或测试
pub(crate) async fn groups(host: &HostClient) -> Result<Vec<Group>, PluginFault> {
    #[derive(Deserialize)]
    struct Page {
        items: Vec<Group>,
        page: Pagination,
    }
    #[derive(Deserialize)]
    struct Pagination {
        total: u64,
    }
    let mut result = Vec::new();
    for page in 1..=100 {
        let value = host_json::get(
            host,
            format!("/api/admin/account-groups/options?page={page}&pageSize=100"),
        )
        .await
        .ok_or_else(unavailable)?;
        let value: Page = serde_json::from_value(value).map_err(|_| unavailable())?;
        let done = value.items.is_empty() || page * 100 >= value.page.total;
        result.extend(value.items);
        if done {
            return Ok(result);
        }
    }
    Err(unavailable())
}

/// 留空时为本实例幂等维护一个覆盖全部账号的共享分组
/// @param host 当前管理调用
/// @param instance 插件实例身份
/// @param accounts 宿主当前完整账号事实
/// @param selected 正在测试的账号
/// @return 统一测试 Key ID
pub(crate) async fn resources(
    host: &HostClient,
    instance: &str,
    accounts: &[AccountFacts],
    selected: &AccountFacts,
) -> Result<String, PluginFault> {
    let mut settings = load(host).await?;
    if let Some(key) = settings.key_id.clone() {
        return validate_key(host, key, None, selected).await;
    }
    let (group, group_name) = if let Some(id) = settings.group_id.clone() {
        let group = groups(host)
            .await?
            .into_iter()
            .find(|g| g.id == id && g.enabled)
            .ok_or_else(|| {
                PluginFault::new(ErrorCode::InvalidInput, "测试分组已删除或停用，请重新设置")
            })?;

        (group.id, group.name)
    } else {
        let group = host
            .ensure_group(GroupEnsureRequest {
                resource_key: "test.shared".into(),
                name: "请求头检测".into(),
                color: "#4477AAFF".into(),
                description: Some("账号请求头检测插件的共享分组，自动加入全部账号".into()),
            })
            .await?;
        if !group.enabled {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "共享测试分组已停用",
            ));
        }
        host.change_group_members(GroupMembersChange {
            resource_key: "test.shared".into(),
            add: accounts.iter().map(|a| a.account_id.clone()).collect(),
            remove: Vec::new(),
        })
        .await?;
        (group.id, group.name)
    };
    // 已确认的 Key ID 保存在私有状态，名称和可见标签都无需携带机器 ID
    if let Some(key) = settings.managed_keys.get(&group) {
        return validate_key(host, key.clone(), Some(group.as_str()), selected).await;
    }
    // 创建结果未知时按检测专用标签及完整分组范围找回，旧版按原归属识别
    let name = if group_name == "请求头检测" {
        "请求头检测".into()
    } else {
        format!("请求头检测 · {group_name}")
    };
    let owner = format!("cpr-account-headers:{instance}");
    let label = "请求头检测专用";
    let legacy_name = format!("请求头测试 {instance} {group}");
    let mut cursor: Option<String> = None;
    let mut found = Vec::new();
    loop {
        let uri = {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query
                .append_pair("search", "请求头")
                .append_pair("limit", "100");
            if let Some(cursor) = &cursor {
                query.append_pair("cursor", cursor);
            }
            format!("/api/admin/client-keys?{}", query.finish())
        };
        let data = host_json::get(host, uri).await.ok_or_else(unavailable)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Keys {
            items: Vec<Key>,
            next_cursor: Option<String>,
        }
        #[derive(Deserialize)]
        struct Key {
            id: String,
            name: String,
            label: Option<String>,
            groups: Vec<GroupIdentity>,
        }
        #[derive(Deserialize)]
        struct GroupIdentity {
            id: String,
        }
        let data: Keys = serde_json::from_value(data).map_err(|_| unavailable())?;
        found.extend(
            data.items
                .into_iter()
                .filter(|k| {
                    (k.label.as_deref() == Some(label)
                        && k.groups.len() == 1
                        && k.groups[0].id == group)
                        || (k.label.as_deref() == Some(owner.as_str()) && k.name == legacy_name)
                })
                .map(|k| k.id),
        );
        cursor = data.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let key = match found.as_slice() {
        [id] => id.clone(),
        [] => {
            let created = host_json::send(host, "POST", "/api/admin/client-keys/create".into(), Some(json!({"name":name,"label":label,"groupIds":[group],"maxConcurrency":4,"requestsPerMinute":0,"dailyLimitUsd":"0","weeklyLimitUsd":"0"}))).await.ok_or_else(unavailable)?;
            created
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(unavailable)?
                .to_owned()
        }
        _ => {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "存在重复的专用测试 Key，请在 Key 管理中处理",
            ));
        }
    };
    let key = validate_key(host, key, Some(group.as_str()), selected).await?;
    settings.managed_keys.insert(group, key.clone());
    put(host, settings).await?;
    Ok(key)
}

/// 所有检测身份统一使用宿主事实校验启用状态及目标账号权限
async fn validate_key(
    host: &HostClient,
    key: String,
    expected_group: Option<&str>,
    selected: &AccountFacts,
) -> Result<String, PluginFault> {
    let facts = host
        .key_facts(ClientKeyFactsQuery {
            client_key_id: key.clone(),
        })
        .await?;
    if !facts.enabled {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "检测 API Key 已停用",
        ));
    }
    if expected_group.is_some_and(|group| facts.group_ids != [group]) {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "专用测试 Key 已停用或分组范围已改变",
        ));
    }
    // 权限使用宿主专用接口判定，账号列表的分组展示投影不承担执行授权
    let access = host
        .check_access(AccessQuery {
            client_key_id: key.clone(),
            account_ids: vec![selected.account_id.clone()],
            upstream_model: None,
        })
        .await?;
    if !access
        .accounts
        .iter()
        .any(|a| a.account_id == selected.account_id && a.allowed)
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "所选检测 API Key 没有此账号的使用权限",
        ));
    }
    Ok(key)
}

fn unavailable() -> PluginFault {
    PluginFault::new(ErrorCode::Fault, "测试设置读取或资源准备失败")
}

/// 原生 Key 选择项只公开非秘密身份信息
#[derive(Serialize, Deserialize)]
pub(crate) struct KeyOption {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
}

/// 分页列出当前管理员已有的 Key，选用时仍由宿主复核账号权限
/// @param host 当前管理员调用
/// @return 可搜索的既有 Key，不包含 Key 原文或前缀
pub(crate) async fn keys(host: &HostClient) -> Result<Vec<KeyOption>, PluginFault> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Page {
        items: Vec<KeyOption>,
        next_cursor: Option<String>,
    }
    let mut result = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let uri = {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("limit", "100");
            if let Some(cursor) = &cursor {
                query.append_pair("cursor", cursor);
            }
            format!("/api/admin/client-keys?{}", query.finish())
        };
        let value = host_json::get(host, uri).await.ok_or_else(unavailable)?;
        let page: Page = serde_json::from_value(value).map_err(|_| unavailable())?;
        result.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(result);
        }
    }
    Err(unavailable())
}

/// 升级设置版本时保留原分组，Key 选择由新版本按需保存
/// @param request 宿主提供的旧版暂存状态批次
/// @return 原样保留的有效设置，宿主负责原子提交及版本校验
pub(crate) fn migrate(request: StateMigrationRequest) -> Result<StateMigrationResult, PluginFault> {
    if request.namespace != "testing"
        || request.from_schema_version != 1
        || request.to_schema_version != 2
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "检测设置迁移版本无效",
        ));
    }
    let mut changes = Vec::new();
    for record in request.records {
        let valid = record.key == "settings"
            && record.value.as_object().is_some_and(|v| {
                v.keys().all(|key| key == "groupId")
                    && v.get("groupId")
                        .is_none_or(|group| group.is_null() || group.is_string())
            });
        if !valid {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "旧版检测设置格式无效",
            ));
        }
        changes.push(StateMigrationChange::Keep { key: record.key });
    }
    Ok(StateMigrationResult { changes })
}
