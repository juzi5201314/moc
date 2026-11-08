//! 普通委托的固定角色目录；只有配置和已加载插件能够提供角色定义。

use std::collections::BTreeMap;

use super::{AgentPreset, ToolPolicy, preset};
use crate::{ResolvedConfig, service::CoreInner};

pub(crate) fn fixed_roles(
    inner: &CoreInner,
    config: &ResolvedConfig,
    plugins: &moc_plugin::PluginHost,
) -> BTreeMap<String, AgentPreset> {
    let mut roles = BTreeMap::new();
    if config.roles.explore != "none" {
        let mut explorer = inner
            .agent_presets
            .get(preset::EXPLORER_ROLE)
            .expect("内置 Explorer 已注册")
            .clone();
        explorer.model.clone_from(&config.roles.explore);
        roles.insert(explorer.name.clone(), explorer);
    }
    for declaration in plugins.agent_preset_declarations().into_values() {
        // 内置角色名不能被插件冒用；禁用 Explorer 也不能由同名插件重新开启。
        if matches!(
            declaration.name.as_str(),
            preset::EXPLORER_ROLE | preset::SIDEKICK_PRESET
        ) || declaration.model == "none"
        {
            continue;
        }
        let tools = match declaration.allow {
            Some(names) => ToolPolicy::Allow(names.into_iter().collect()),
            None => ToolPolicy::Deny(declaration.deny.unwrap_or_default().into_iter().collect()),
        };
        let role = preset::sanitize_preset(AgentPreset {
            name: declaration.name,
            description: declaration.description,
            model: declaration.model,
            system_prompt: declaration.system_prompt,
            tools,
        });
        roles.insert(role.name.clone(), role);
    }
    roles
}

pub(crate) fn descriptions(
    inner: &CoreInner,
    config: &ResolvedConfig,
    plugins: &moc_plugin::PluginHost,
) -> BTreeMap<String, String> {
    let mut descriptions = BTreeMap::new();
    if config.roles.explore != "none" {
        let explorer = inner
            .agent_presets
            .get(preset::EXPLORER_ROLE)
            .expect("内置 Explorer 已注册");
        descriptions.insert(explorer.name.clone(), explorer.description.clone());
    }
    for role in plugins.agent_preset_declarations().into_values() {
        if role.model != "none"
            && !matches!(
                role.name.as_str(),
                preset::EXPLORER_ROLE | preset::SIDEKICK_PRESET
            )
        {
            descriptions.insert(role.name, role.description);
        }
    }
    descriptions
}
