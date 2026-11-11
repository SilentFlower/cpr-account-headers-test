//! 独立检测插件只注册管理页面，定向执行继续经过原请求头插件

mod execution;
mod host_json;
mod management;
mod primary;
mod test_settings;
mod testing;
use gateway_plugin_sdk::{
    Manifest,
    client::{PluginBuilder, PluginSession, SessionConfig},
};
use std::error::Error;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json"))?;
    if session.handshake().plugin_id != manifest.plugin_id()?
        || session.handshake().contributes != manifest.contributes
    {
        return Err("插件握手身份不匹配".into());
    }
    let tests = testing::Tests::default();
    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .management(management::registration(), move |call| {
            management::handle(call, tests.clone())
        })?
        .build()?;
    session.run(plugin).await?;
    Ok(())
}
