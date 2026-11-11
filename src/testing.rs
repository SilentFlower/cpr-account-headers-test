//! 管理定向检测的准备、取消与真实出站证据关联

use crate::{
    execution::{Execution, Target},
    primary::Primary,
    test_settings,
};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        data::{AccountFacts, AccountFactsQuery},
        management::ManagementRequest,
    },
    client::TypedCall,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const LIFETIME: Duration = Duration::from_secs(90);
/// 本地只保存有界短期执行及提前取消墓碑
#[derive(Default, Clone)]
pub(crate) struct Tests(
    Arc<Mutex<HashMap<String, Entry>>>,
    Arc<tokio::sync::Mutex<()>>,
);
/// 取消信号与执行入口共享，墓碑用于覆盖管理请求乱序
struct Entry {
    created: Instant,
    account: String,
    cancel: watch::Sender<bool>,
}
/// 父调用结束时释放检测名额
struct Reservation {
    tests: Tests,
    id: String,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.tests
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}
/// 页面提交的固定账号与模型
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    id: String,
    account_id: String,
    model_id: String,
}
/// 页面取消当前随机检测编号
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct End {
    id: String,
}

impl Tests {
    /// 以固定账号执行检测，使用独立请求头插件的真实出站处理中间件
    /// @param call 当前管理员提交的检测参数
    /// @return 模型结果与同一真实请求 ID 的覆写记录
    pub(crate) async fn run(
        &self,
        call: &TypedCall<ManagementRequest>,
    ) -> Result<Value, PluginFault> {
        let input: Input = serde_json::from_slice(&call.payload).map_err(|_| invalid())?;
        if !valid_id(&input.id)
            || input.account_id.is_empty()
            || input.account_id.len() > 128
            || input.model_id.trim().is_empty()
            || input.model_id.len() > 256
            || input.model_id.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let (cancel, mut cancelled) = watch::channel(false);
        {
            let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            entries.retain(|_, e| e.created.elapsed() < LIFETIME);
            if entries.get(&input.id).is_some_and(|e| *e.cancel.borrow()) {
                return Err(stopped());
            }
            if entries.contains_key(&input.id)
                || entries
                    .values()
                    .any(|e| !*e.cancel.borrow() && e.account == input.account_id)
            {
                return Err(PluginFault::new(
                    ErrorCode::Conflict,
                    "此账号已有检测正在执行",
                ));
            }
            if entries.len() >= 64 || entries.values().filter(|e| !*e.cancel.borrow()).count() >= 4
            {
                return Err(PluginFault::new(
                    ErrorCode::Conflict,
                    "已有 4 个检测正在执行",
                ));
            }
            entries.insert(
                input.id.clone(),
                Entry {
                    created: Instant::now(),
                    account: input.account_id.clone(),
                    cancel,
                },
            );
        }
        let _reservation = Reservation {
            tests: self.clone(),
            id: input.id.clone(),
        };
        let primary = Primary::discover(&call.host).await?;
        let status = primary
            .request(&call.host, "diagnostics/status", None)
            .await
            .map_err(|_| {
                PluginFault::new(
                    ErrorCode::InvalidInput,
                    "请将账号请求头插件更新到 v0.2.1 或以上版本",
                )
            })?;
        if status["version"] != 1 {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "账号请求头插件的诊断接口版本不匹配",
            ));
        }
        let mut execution = Execution::default();
        let action = async {
            let (key, provider) = {
                let _guard = self.1.lock().await;
                resources(call, &input.account_id).await?
            };
            primary
                .request(
                    &call.host,
                    "diagnostics/register",
                    Some(json!({"id":input.id,"accountId":input.account_id,"clientKeyId":key})),
                )
                .await?;
            execution
                .run(
                    &call.host,
                    Target {
                        id: &input.id,
                        account: &input.account_id,
                        key: &key,
                        provider: &provider,
                        model: &input.model_id,
                    },
                )
                .await
        };
        let outcome = tokio::select! {
            biased;
            _=cancelled.changed()=>Err(stopped()),
            _=call.cancellation.cancelled()=>Err(stopped()),
            _=tokio::time::sleep(Duration::from_secs(45))=>Err(PluginFault::new(ErrorCode::Timeout,"检测超过 45 秒")),
            result=action=>result,
        };
        execution.close(&call.host).await;
        let mut evidence = json!([]);
        let mut evidence_error = false;
        if let Some(request_id) = &execution.request_id {
            let query = form_urlencoded::Serializer::new(String::new())
                .append_pair("id", &input.id)
                .append_pair("requestId", request_id)
                .finish();
            match primary
                .request(&call.host, &format!("diagnostics/result?{query}"), None)
                .await
            {
                Ok(value) if value["outbound"].is_array() => evidence = value["outbound"].clone(),
                _ => evidence_error = true,
            }
        }
        let cancelled = outcome
            .as_ref()
            .is_err_and(|e| matches!(e.code, ErrorCode::Cancelled | ErrorCode::Timeout));
        let _ = primary
            .request(
                &call.host,
                if cancelled {
                    "diagnostics/cancel"
                } else {
                    "diagnostics/finish"
                },
                Some(json!({"id":input.id})),
            )
            .await;
        if let Err(error) = outcome {
            if matches!(error.code, ErrorCode::Cancelled | ErrorCode::Timeout)
                || (execution.request_id.is_none() && error.code == ErrorCode::InvalidInput)
            {
                return Err(error);
            }
            return Ok(
                json!({"status":"failed","message":error.message,"requestId":execution.request_id,"outbound":evidence}),
            );
        }
        if execution.error.is_some() || !execution.completed {
            let raw = execution
                .error
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default();
            let message = execution
                .error
                .as_ref()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .or_else(|| v.pointer("/response/error/message"))
                        .or_else(|| v.get("message"))
                })
                .and_then(Value::as_str)
                .unwrap_or("模型请求未完成");
            return Ok(
                json!({"status":"failed","message":message,"requestId":execution.request_id,"outbound":evidence,"error":{"upstreamStatus":execution.upstream_status.unwrap_or(200),"upstreamBody":raw}}),
            );
        }
        if evidence_error || evidence.as_array().is_none_or(Vec::is_empty) {
            return Ok(
                json!({"status":"unverified","message":"模型已响应，尚未取得覆写记录，请检查账号请求头插件的绑定范围","requestId":execution.request_id,"outbound":evidence,"text":execution.text}),
            );
        }
        Ok(
            json!({"status":"success","requestId":execution.request_id,"outbound":evidence,"text":execution.text}),
        )
    }

    /// 保存全局测试分组期间禁止并发改变正在使用的测试身份
    /// @param call 管理员提交的分组配置
    /// @return 已保存设置
    pub(crate) async fn save_settings(
        &self,
        call: &TypedCall<ManagementRequest>,
    ) -> Result<Value, PluginFault> {
        let _guard = self.1.lock().await;
        if self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|e| !*e.cancel.borrow() && e.created.elapsed() < LIFETIME)
        {
            return Err(PluginFault::new(
                ErrorCode::Conflict,
                "请先结束正在执行的检测",
            ));
        }
        let input = serde_json::from_slice(&call.payload).map_err(|_| invalid())?;
        serde_json::to_value(test_settings::save(&call.host, input).await?).map_err(|_| invalid())
    }

    /// 取消当前执行或登记提前取消，信号唤醒原父调用中的模型清理
    /// @param call 包含随机检测编号的管理调用
    /// @return 取消已登记
    pub(crate) fn cancel(&self, call: &TypedCall<ManagementRequest>) -> Result<Value, PluginFault> {
        let input: End = serde_json::from_slice(&call.payload).map_err(|_| invalid())?;
        if !valid_id(&input.id) {
            return Err(invalid());
        }
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, e| e.created.elapsed() < LIFETIME);
        if let Some(e) = entries.get(&input.id) {
            e.cancel.send_replace(true);
        } else if entries.len() < 64 {
            let (cancel, _) = watch::channel(true);
            entries.insert(
                input.id,
                Entry {
                    created: Instant::now(),
                    account: String::new(),
                    cancel,
                },
            );
        }
        Ok(json!({"ended":true}))
    }
}
async fn resources(
    call: &TypedCall<ManagementRequest>,
    account: &str,
) -> Result<(String, String), PluginFault> {
    let mut accounts: Vec<AccountFacts> = Vec::new();
    let mut cursor = None;
    loop {
        let page = call
            .host
            .account_facts(AccountFactsQuery {
                provider_id: None,
                cursor,
                limit: 200,
            })
            .await?;
        accounts.extend(page.accounts);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let selected = accounts
        .iter()
        .find(|a| a.account_id == account)
        .ok_or_else(|| PluginFault::new(ErrorCode::InvalidInput, "检测账号已不存在"))?;
    if !selected.enabled {
        return Err(PluginFault::new(ErrorCode::InvalidInput, "所选账号已停用"));
    }
    let key = test_settings::resources(&call.host, &call.context.instance_id, &accounts, selected)
        .await?;
    Ok((key, selected.provider_id.clone()))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn invalid() -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, "检测参数无效")
}
fn stopped() -> PluginFault {
    PluginFault::new(ErrorCode::Cancelled, "检测已停止")
}
