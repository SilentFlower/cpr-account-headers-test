//! 使用宿主定向执行及有界模型流，固定账号并保留原生响应错误

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host::{
            MODEL_EXECUTE_STREAM, MODEL_STREAM_CLOSE, MODEL_STREAM_READ, ModelEventBatch,
            ModelExecuteRequest, ModelOperation, ModelStreamReadResult, ModelStreamResult,
        },
        model::{CanonicalEvent, WirePayload},
    },
    client::{HostClient, SessionError},
};
use serde_json::{Value, json};

/// 一次定向模型执行的实际结果及待关闭句柄
#[derive(Default)]
pub(crate) struct Execution {
    pub(crate) request_id: Option<String>,
    pub(crate) stream: Option<String>,
    pub(crate) completed: bool,
    pub(crate) text: String,
    pub(crate) error: Option<Value>,
    pub(crate) upstream_status: Option<u16>,
}

/// 模型目标由管理员所选账号与测试专用 Key 冻结
pub(crate) struct Target<'a> {
    pub(crate) id: &'a str,
    pub(crate) account: &'a str,
    pub(crate) key: &'a str,
    pub(crate) model: &'a str,
    pub(crate) provider: &'a str,
}
impl Execution {
    /// 从宿主模型流读取实际终态和输出，当前检测插件在子调用中跳过
    /// @param host 当前管理员调用的宿主客户端
    /// @param target 已校验的测试目标
    /// @return 成功读取终态，模型本身失败保存在 error 中
    pub(crate) async fn run(
        &mut self,
        host: &HostClient,
        target: Target<'_>,
    ) -> Result<(), PluginFault> {
        let request = ModelExecuteRequest {
            client_key_id: Some(target.key.into()),
            model: target.model.into(),
            protocol: "openai".into(),
            operation: ModelOperation::Generate,
            provider: Some(target.provider.into()),
            account_id: Some(target.account.into()),
            previous_response_id: None,
        };
        let body = json!({"model":target.model,"input":"Reply with exactly OK.","stream":true,"store":false,"metadata":{"cpr_account_headers_test":target.id}});
        let reply = host
            .call(
                MODEL_EXECUTE_STREAM,
                serde_json::to_value(request).map_err(|_| invalid())?,
                serde_json::to_vec(&body).map_err(|_| invalid())?,
            )
            .await
            .map_err(SessionError::into_plugin_fault)?;
        let started: ModelStreamResult =
            serde_json::from_value(reply.result).map_err(|_| invalid())?;
        self.request_id = Some(started.request_id);
        self.stream = Some(started.stream.clone());
        let mut bytes = 0usize;
        loop {
            let reply = host
                .call(
                    MODEL_STREAM_READ,
                    json!({"stream":started.stream,"maximum_bytes":262144}),
                    Vec::new(),
                )
                .await
                .map_err(SessionError::into_plugin_fault)?;
            let read: ModelStreamReadResult =
                serde_json::from_value(reply.result).map_err(|_| invalid())?;
            bytes = bytes.saturating_add(reply.payload.len());
            if bytes > 512 * 1024 {
                return Err(PluginFault::new(
                    ErrorCode::Capacity,
                    "测试响应超过 512 KiB",
                ));
            }
            if !reply.payload.is_empty() {
                let batch = ModelEventBatch::decode(&reply.payload).map_err(|_| invalid())?;
                for event in batch.events {
                    for fact in event.facts {
                        match fact {
                            CanonicalEvent::TextDelta { text, .. } => self.text.push_str(&text),
                            CanonicalEvent::Completed { .. } => self.completed = true,
                            _ => {}
                        }
                    }
                    if let Some(host) = event.host.and_then(|host| host.observation) {
                        self.upstream_status = host.status_code.or(self.upstream_status);
                    }
                    if let Some(wire) = event.wire {
                        let value = match wire.payload {
                            WirePayload::Json { data, .. } => Some(data),
                            WirePayload::WebSocket { data, .. } => data,
                            WirePayload::RawJson { body } => serde_json::from_slice(&body).ok(),
                            WirePayload::RawSse { frame } => {
                                let data = String::from_utf8_lossy(&frame)
                                    .lines()
                                    .filter_map(|line| line.strip_prefix("data:"))
                                    .map(str::trim_start)
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                serde_json::from_str(&data).ok()
                            }
                            _ => None,
                        };
                        if let Some(value) = value {
                            let response = value.get("response").unwrap_or(&value);
                            if response.get("status").and_then(Value::as_str) == Some("completed") {
                                self.completed = true;
                                if let Some(output) =
                                    response.get("output").and_then(Value::as_array)
                                {
                                    let text = output
                                        .iter()
                                        .filter_map(|item| {
                                            item.get("content").and_then(Value::as_array)
                                        })
                                        .flatten()
                                        .filter(|part| {
                                            part.get("type").and_then(Value::as_str)
                                                == Some("output_text")
                                        })
                                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("");
                                    if !text.is_empty() {
                                        self.text = text;
                                    }
                                }
                            }

                            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                            if matches!(kind, "error" | "response.failed" | "response.incomplete")
                                || value.get("error").is_some_and(|v| !v.is_null())
                            {
                                self.error = Some(value);
                                self.completed = false;
                            }
                        }
                    }
                }
            }
            if read.end {
                return Ok(());
            }
        }
    }

    /// 无论完成、超时还是取消，均在原父调用内关闭模型句柄
    /// @param host 创建模型流的原宿主客户端
    /// @return 无，宿主会兜底回收已结束的父调用资源
    pub(crate) async fn close(&mut self, host: &HostClient) {
        if let Some(stream) = self.stream.take() {
            let _ = host
                .call(MODEL_STREAM_CLOSE, json!({"stream":stream}), Vec::new())
                .await;
        }
    }
}
fn invalid() -> PluginFault {
    PluginFault::new(ErrorCode::Fault, "宿主模型执行返回无效数据")
}
