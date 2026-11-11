//! 只读展示账号规则，提供分组配置、账号模型目录与独立定向检测

use crate::{host_json, primary::Primary, test_settings, testing::Tests};
use std::collections::BTreeMap;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        management::{
            ManagementPage, ManagementRegistration, ManagementRequest, ManagementResource,
            ManagementResponse, ManagementRoute,
        },
        middleware::MiddlewareHeader,
    },
    client::{TypedCall, TypedReply},
};
use serde_json::{Value, json};

/// 注册独立检测页，规则修改接口由原请求头插件拥有
/// @return 页面与已支持的管理路由
pub(crate) fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: [
            ("GET", "accounts"),
            ("GET", "rule"),
            ("GET", "models"),
            ("GET", "test/settings"),
            ("POST", "test/settings"),
            ("POST", "test"),
            ("POST", "test/cancel"),
        ]
        .into_iter()
        .map(|(method, path)| ManagementRoute {
            method: method.into(),
            path: path.into(),
            request_content_types: if method == "POST" {
                vec!["application/json".into()]
            } else {
                vec![]
            },
            response_content_types: vec!["application/json".into()],
        })
        .collect(),
        resources: ["web/index.html", "web/app.js", "web/app.css"]
            .into_iter()
            .map(|p| ManagementResource {
                path: p.into(),
                public: false,
            })
            .collect(),
        pages: vec![ManagementPage {
            id: "tests".into(),
            title: "账号请求头检测".into(),
            description: Some("只读查看账号规则，定向验证模型响应与真实请求头覆写".into()),
            entry: "web/index.html".into(),
            icon: None,
        }],
        callbacks: vec![],
    }
}

/// 处理已认证管理员的只读展示及检测操作
/// @param call 当前管理调用
/// @param tests 当前进程有界检测关联
/// @return 安全中文业务错误或实际执行结果
pub(crate) async fn handle(
    call: TypedCall<ManagementRequest>,
    tests: Tests,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let (status, value) = match route(&call, &tests).await {
        Ok(value) => (200, value),
        Err(error) => {
            let status = match error.code {
                ErrorCode::InvalidInput => 400,
                ErrorCode::Conflict | ErrorCode::Cancelled => 409,
                ErrorCode::Timeout => 408,
                _ => 503,
            };
            (
                status,
                json!({"error":{"code":error.code,"message":error.message}}),
            )
        }
    };
    Ok(TypedReply::new(ManagementResponse {
        status,
        content_type: "application/json".into(),
        headers: vec![MiddlewareHeader {
            name: "cache-control".into(),
            value: b"no-store".to_vec(),
        }],
    })
    .with_payload(serde_json::to_vec(&value).map_err(|_| invalid())?))
}
async fn route(call: &TypedCall<ManagementRequest>, tests: &Tests) -> Result<Value, PluginFault> {
    let mut query = BTreeMap::new();
    for (k, v) in form_urlencoded::parse(call.request.query.as_bytes()).into_owned() {
        if query.insert(k, v).is_some() {
            return Err(invalid());
        }
    }
    match (call.request.method.as_str(), call.request.path.as_str()) {
        ("GET", "accounts") if query.keys().all(|k| k == "cursor" || k == "search") => {
            let p = Primary::discover(&call.host).await?;
            p.request(
                &call.host,
                &format!("accounts?{}", call.request.query),
                None,
            )
            .await
        }
        ("GET", "rule")
            if query.len() == 1
                && query
                    .get("accountId")
                    .is_some_and(|s| !s.is_empty() && s.len() <= 128) =>
        {
            let p = Primary::discover(&call.host).await?;
            p.request(&call.host, &format!("rule?{}", call.request.query), None)
                .await
        }
        ("GET", "models")
            if query.len() == 1
                && query
                    .get("accountId")
                    .is_some_and(|s| !s.is_empty() && s.len() <= 128) =>
        {
            let value = host_json::raw(
                &call.host,
                "GET",
                format!("/api/admin/accounts/models?{}", call.request.query),
                None,
                10000,
            )
            .await?;
            if value["code"] != 200 || !value["data"]["models"].is_array() {
                return Err(PluginFault::new(ErrorCode::Fault, "账号模型列表读取失败"));
            }
            Ok(value["data"].clone())
        }
        ("GET", "test/settings") if query.is_empty() => {
            let (settings, groups, keys) = tokio::try_join!(
                test_settings::load(&call.host),
                test_settings::groups(&call.host),
                test_settings::keys(&call.host)
            )?;
            Ok(json!({"settings":settings,"groups":groups,"keys":keys}))
        }
        ("POST", "test/settings") if query.is_empty() => tests.save_settings(call).await,
        ("POST", "test") if query.is_empty() => tests.run(call).await,
        ("POST", "test/cancel") if query.is_empty() => tests.cancel(call),
        _ => Err(invalid()),
    }
}
fn invalid() -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, "请求参数无效或不支持此操作")
}
