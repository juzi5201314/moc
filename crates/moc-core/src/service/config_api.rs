//! 配置快照协议投影。

use super::Core;
use crate::service::to_rpc;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::AgentProfile;
use moc_protocol::methods::ConfigGetParams;
use moc_protocol::methods::ConfigGetResult;
use moc_protocol::methods::ModelInfo;
use moc_protocol::methods::ModelInput;

pub(super) async fn config_get(
    core: &Core,
    _ctx: RequestContext,
    _params: ConfigGetParams,
) -> Result<ConfigGetResult, RpcError> {
    let config = core.inner.config();
    let models = config
        .available_models()
        .into_iter()
        .map(|model| ModelInfo {
            // 目录里的展示名就是模型 id；`ResolvedModel.display_name` 是
            // 带默认等级的运行时名字（`id @level`），不属于目录。
            display_name: model.id.clone(),
            id: model.id,
            thinking_levels: Some(
                std::iter::once("off".to_owned())
                    .chain(
                        model
                            .thinking_levels
                            .iter()
                            .map(|level| level.as_str().to_owned()),
                    )
                    .collect(),
            ),
            context_window: Some(model.context_window),
            max_output_tokens: Some(model.max_output_tokens),
            pricing: model.pricing,
            inputs: [
                (model.capabilities.text, ModelInput::Text),
                (model.capabilities.image, ModelInput::Image),
            ]
            .into_iter()
            .filter_map(|(supported, input)| supported.then_some(input))
            .collect(),
            fast_mode: model.fast_mode,
        })
        .collect::<Vec<_>>();
    let fusion = if config.fusion.lead.is_empty() || config.fusion.sidekick.is_empty() {
        None
    } else {
        Some(moc_protocol::methods::FusionPair {
            lead: config
                .resolve_model(&config.fusion.lead)
                .map_err(to_rpc)?
                .selector,
            sidekick: config
                .resolve_model(&config.fusion.sidekick)
                .map_err(to_rpc)?
                .selector,
        })
    };
    Ok(ConfigGetResult {
        models,
        default_model: config
            .last_model()
            .map_err(to_rpc)?
            .map(|model| model.selector),
        agents: vec![AgentProfile {
            id: "moc".into(),
            display_name: "moc".into(),
            description: Some(rust_i18n::t!("core.preset.agent_default").into_owned()),
        }],
        default_agent: Some("moc".into()),
        cwd: config.cwd.to_string_lossy().into_owned(),
        fusion,
        features: config.features,
        language: Some(config.language.clone()),
    })
}
