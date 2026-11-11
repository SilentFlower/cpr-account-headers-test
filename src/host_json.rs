//! 通过公开宿主 HTTP 回调读取页面所需的原生管理数据

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::middleware::{MiddlewareHeader, http::Version},
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
};
use serde_json::Value;

/// 读取有界 JSON 成功信封，缺失或失败由页面保留未知状态
/// @param host 当前管理调用的受管客户端
/// @param uri 已确认的宿主只读管理接口
/// @return 原生业务数据，失败时返回 None
pub(crate) async fn get(host: &HostClient, uri: String) -> Option<Value> {
    send(host, "GET", uri, None).await
}

/// 调用原生管理接口，写操作只用于本插件的专用测试 Key
/// @param host 当前管理员调用的受管客户端
/// @param method 已确认的 HTTP 方法
/// @param uri 已确认的管理接口
/// @param body 可选 JSON 正文
/// @return 已解包的成功业务数据
pub(crate) async fn send(
    host: &HostClient,
    method: &str,
    uri: String,
    body: Option<Value>,
) -> Option<Value> {
    let value = raw(
        host,
        method,
        uri,
        body,
        if method == "GET" { 1500 } else { 5000 },
    )
    .await
    .ok()?;
    if value.get("code").and_then(Value::as_u64) != Some(200) {
        return None;
    }
    value.get("data").cloned()
}

/// 有界读取原生或插件管理 JSON，失败时只返回安全业务提示
/// @param host 当前调用的受管客户端
/// @param method 已确认的管理 HTTP 方法
/// @param uri 已确认的管理 URI
/// @param body 可选 JSON 正文
/// @param timeout_ms 本次受管调用预算
/// @return 未解包的业务 JSON
pub(crate) async fn raw(
    host: &HostClient,
    method: &str,
    uri: String,
    body: Option<Value>,
    timeout_ms: u64,
) -> Result<Value, PluginFault> {
    let bytes = body
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| unavailable())?;
    let mut response = host
        .dispatch_http(HttpRequest {
            settings: Value::Null,
            method: method.into(),
            uri,
            version: Version::Http11,
            headers: if bytes.is_some() {
                vec![MiddlewareHeader {
                    name: "content-type".into(),
                    value: b"application/json".to_vec(),
                }]
            } else {
                Vec::new()
            },
            timeout_ms: Some(timeout_ms),
            body: bytes.map_or_else(HttpBody::empty, HttpBody::from_bytes),
        })
        .await?;
    let status = response.status;
    let mut bytes = Vec::new();
    loop {
        let frame = match response.body.read().await {
            Ok(frame) => frame,
            Err(error) => {
                let _ = response.body.close().await;
                return Err(error);
            }
        };
        let Some(frame) = frame else { break };
        if let HttpFrame::Data(chunk) = frame {
            if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                let _ = response.body.close().await;
                return Err(unavailable());
            }
            bytes.extend(chunk);
        }
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    if !matches!(status, 200 | 201) {
        let message = value
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("请求头插件或宿主管理接口访问失败");
        return Err(PluginFault::new(
            if status == 409 {
                ErrorCode::Conflict
            } else {
                ErrorCode::InvalidInput
            },
            message,
        ));
    }
    Ok(value)
}
fn unavailable() -> PluginFault {
    PluginFault::new(ErrorCode::Fault, "管理接口返回无效数据")
}
