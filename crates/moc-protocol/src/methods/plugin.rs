//! 插件系统的方法：列出已加载插件、执行插件斜杠命令。

use crate::method::{ClientRequest, Method};
use serde::{Deserialize, Serialize};

/// `plugin/list`：本工作区已发现并加载的插件及其注册项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginList;
impl Method for PluginList {
    const NAME: &'static str = "plugin/list";
    type Params = PluginListParams;
    type Result = PluginListResult;
}
impl ClientRequest for PluginList {}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginListParams {
    /// 查询哪个工作目录的插件；`None` 表示用会话自己的 cwd。
    #[serde(default)]
    pub session_id: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginListResult {
    pub plugins: Vec<PluginInfo>,
}
/// 一个插件的可见状态。`error` 非空表示它加载失败或已在本进程内被禁用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// 发现来源（项目/用户/配置目录）。
    pub source: String,
    pub dir: String,
    #[serde(default)]
    pub tools: Vec<PluginToolInfo>,
    #[serde(default)]
    pub commands: Vec<PluginCommandInfo>,
    #[serde(default)]
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginToolInfo {
    pub name: String,
    pub description: String,
    /// 该工具可见的执行模式（`default`/`ask`/`plan`）。
    pub modes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCommandInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub usage: String,
}

/// `plugin/reload`：丢弃该工作区的插件宿主并重新发现、加载。
///
/// 插件在本进程内因失败被停用后，这是除重启 moc 之外的唯一恢复路径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginReload;
impl Method for PluginReload {
    const NAME: &'static str = "plugin/reload";
    type Params = PluginReloadParams;
    type Result = PluginReloadResult;
}
impl ClientRequest for PluginReload {}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReloadParams {
    /// 重载哪个工作目录的插件；`None` 表示用会话自己的 cwd。
    #[serde(default)]
    pub session_id: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReloadResult {
    /// 重载后的完整插件清单（与 `plugin/list` 相同）。
    pub plugins: Vec<PluginInfo>,
}

/// `plugin/run_command`：执行一个插件注册的斜杠命令。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginRunCommand;
impl Method for PluginRunCommand {
    const NAME: &'static str = "plugin/run_command";
    type Params = PluginRunCommandParams;
    type Result = PluginRunCommandResult;
}
impl ClientRequest for PluginRunCommand {}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRunCommandParams {
    pub session_id: String,
    /// 命令名，不含前导斜杠。
    pub name: String,
    #[serde(default)]
    pub args: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRunCommandResult {
    /// 插件返回的文本；前端按普通输出展示。
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plugin_tool_info_round_trips_the_current_contract() {
        let expected = json!({
            "name": "demo",
            "description": "演示",
            "modes": ["default", "plan"],
        });
        let tool: PluginToolInfo = serde_json::from_value(expected.clone()).expect("工具声明");
        assert_eq!(serde_json::to_value(tool).expect("序列化"), expected);
    }
}
