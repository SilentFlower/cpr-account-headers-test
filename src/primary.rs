//! 通过公开管理桥读取请求头插件的账号规则并登记真实出站诊断

use crate::host_json;
use gateway_plugin_sdk::{ErrorCode, PluginFault, client::HostClient};
use serde_json::Value;

/// 当前已启用的请求头插件管理入口，版本切换后重新发现
#[derive(Clone)]
pub(crate) struct Primary {
    prefix: String,
}
impl Primary {
    /// 按插件身份与启用实例发现精确版本入口
    /// @param host 当前管理调用
    /// @return 原请求头插件的管理入口
    pub(crate) async fn discover(host: &HostClient) -> Result<Self, PluginFault> {
        let artifacts = host_json::get(host, "/api/admin/plugins/artifacts".into())
            .await
            .ok_or_else(unavailable)?;
        let artifacts = artifacts.as_array().ok_or_else(unavailable)?;
        let hashes: Vec<_> = artifacts
            .iter()
            .filter(|a| a["metadata"]["pluginId"] == "silentflower.account-headers")
            .filter_map(|a| a["metadata"]["sha256"].as_str())
            .collect();
        let instances = host_json::get(host, "/api/admin/plugins/instances".into())
            .await
            .ok_or_else(unavailable)?;
        let instance = instances
            .as_array()
            .and_then(|items| {
                items.iter().find(|i| {
                    i["enabled"] == true
                        && i["running"] == true
                        && i["artifactSha256"]
                            .as_str()
                            .is_some_and(|sha| hashes.contains(&sha))
                })
            })
            .ok_or_else(unavailable)?;
        let id = instance["id"].as_str().ok_or_else(unavailable)?;
        let sha = instance["artifactSha256"]
            .as_str()
            .ok_or_else(unavailable)?;
        let revision = instance["revision"].as_u64().ok_or_else(unavailable)?;
        Ok(Self {
            prefix: format!("/api/admin/plugins/extensions/{id}/{sha}/{revision}/api/"),
        })
    }

    /// 访问原插件的只读规则接口或短期诊断接口
    /// @param host 当前管理调用
    /// @param path 原插件已注册的相对管理路由
    /// @param body 短期诊断正文，省略时为只读查询
    /// @return 原插件的业务 JSON
    pub(crate) async fn request(
        &self,
        host: &HostClient,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, PluginFault> {
        host_json::raw(
            host,
            if body.is_some() { "POST" } else { "GET" },
            format!("{}{path}", self.prefix),
            body,
            5000,
        )
        .await
    }
}
fn unavailable() -> PluginFault {
    PluginFault::new(
        ErrorCode::InvalidInput,
        "请安装并启用账号请求头插件 v0.2.1 或以上版本",
    )
}
