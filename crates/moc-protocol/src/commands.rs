//! 斜杠命令注册表（Gemini `BuiltinCommandLoader` 式数据化）：内置命令是数据
//! 记录而不是散落的字符串字面量，插件命令作为第二来源动态并入同一张表。
//!
//! 注册表只管元数据与查找：分派仍由前端按 `CommandSpec::id` 落到自己的处理器；
//! 门控（会话/运行中/聚焦/flag）由前端拿 spec 字段统一判定。这样补全、帮助、
//! 用法提示、别名与未知命令检测共用一份事实，不会再出现「命令能跑但补全表
//! 里没有」的漂移。
//!
//! 与 Gemini 字段的对应：`门控 → needs_session/available_during_task/
//! available_in_side/flag`，`别名 → aliases`，`分组 → group`，
//! `参数级 completion() → args`，`isSafeConcurrent/autoExecute →
//! available_during_task`（moc 命令一律即时执行、无确认态，故无 autoExecute）。

use crate::features::FeatureFlag;
use crate::methods::{PluginCommandInfo, PluginInfo};

/// 命令来源（Gemini 七类来源的子集；新增注册方时扩展成员）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSource {
    /// 前端内置（`&'static CommandSpec`）。
    Builtin,
    /// 插件注册；`origin` 是插件发现来源（project/user/config 目录）。
    Plugin { plugin: String, origin: String },
}

/// 命令分组：补全排序与帮助分节共用同一组序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandGroup {
    /// 会话生命周期（new/resume/close/quit）。
    Session,
    /// 模式与引擎（engine/plan/autonomy/permission/model…）。
    Mode,
    /// 上下文（compact/context/usage/retry）。
    Context,
    /// 子代理与任务（agents/jobs/workflow/workflows）。
    Agents,
    /// 插件（/plugins 与插件注册的命令）。
    Plugins,
    /// 界面与诊断（config/help/doctor/features）。
    Ui,
}

impl CommandGroup {
    /// 帮助/清单里的组标题。
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "会话",
            Self::Mode => "模式与引擎",
            Self::Context => "上下文",
            Self::Agents => "子代理与任务",
            Self::Plugins => "插件",
            Self::Ui => "界面",
        }
    }

    /// 跨线传输的机器键（`/commands.json`、日志），与成员名一致的小写形式。
    pub fn key(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Mode => "mode",
            Self::Context => "context",
            Self::Agents => "agents",
            Self::Plugins => "plugins",
            Self::Ui => "ui",
        }
    }
}

/// 参数补全策略（命令名之后的 args 段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgCompletion {
    /// 无参数或自由文本（不补全）。
    None,
    /// 固定候选词。
    Choices(&'static [&'static str]),
    /// 动态候选；前端按种类供给（模型列表、沙盒模式等）。
    Dynamic(DynamicArg),
}

/// 动态参数候选的种类；每个前端自行供给。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicArg {
    /// 沙盒权限模式（按平台能力过滤）。
    SandboxMode,
}

/// 命令元数据的可序列化视图：发给非 Rust 前端（WebUI）时剥离 `id` 与
/// `args`（id 是各前端内部分派键，`args` 引用前端自供的动态候选），
/// `flag` 展开为 `config.get` 特性表里的键名，前端按 `features[flag]` 判开关。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandMeta {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub description: &'static str,
    /// [`CommandGroup::key`] 的机器键。
    pub group: &'static str,
    pub needs_session: bool,
    pub available_during_task: bool,
    pub available_in_side: bool,
    /// 特性键名（`FeatureSpec::key`）；`None` 表示不受 flag 门控。
    pub flag: Option<&'static str>,
}

/// 一条内置斜杠命令的元数据。
///
/// `I` 是前端定义的命令 id（通常是一个枚举），分派按它而不是按字符串。
pub struct CommandSpec<I> {
    pub id: I,
    /// 主名，不含 `/`。
    pub name: &'static str,
    /// 别名；同样参与补全与分派（如 `settings → config`、`exit → quit`）。
    pub aliases: &'static [&'static str],
    /// 参数用法提示（如 `"[reload]"`、`"<问题>"`）；`""` 表示无参数。
    pub usage: &'static str,
    pub description: &'static str,
    pub group: CommandGroup,
    /// 需要活动会话。
    pub needs_session: bool,
    /// 生成中可执行（Codex `available_during_task` / Gemini `isSafeConcurrent`）：
    /// `false` 时前端在提示运行中直接给出提示，而不是让 RPC 撞 SessionBusy。
    pub available_during_task: bool,
    /// 旁支会话（聚焦子代理）里可用（Codex `available_in_side_conversation`）：
    /// `false` 用于切换/卸载当前会话等会破坏聚焦栈的命令。
    pub available_in_side: bool,
    /// 补全与帮助中隐藏，仍可输入执行。
    pub hidden: bool,
    /// 参数补全策略。
    pub args: ArgCompletion,
    /// 依赖的特性 flag；flag 关闭时命令不出现在补全，执行得到明确提示。
    pub flag: Option<FeatureFlag>,
}

impl<I> CommandSpec<I> {
    /// 跨线元数据视图（见 [`CommandMeta`]）。
    pub fn meta(&self) -> CommandMeta {
        CommandMeta {
            name: self.name,
            aliases: self.aliases,
            usage: self.usage,
            description: self.description,
            group: self.group.key(),
            needs_session: self.needs_session,
            available_during_task: self.available_during_task,
            available_in_side: self.available_in_side,
            flag: self.flag.map(|flag| flag.spec().key),
        }
    }
}

/// 插件注册的命令项（[`PluginCommandInfo`] + 宿主插件信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub plugin: String,
    pub origin: String,
}

/// [`CommandRegistry::resolve`] 的结果。
///
/// 手动实现 `Clone`/`Copy`/`Debug` 而不 derive：derive 会给 `I` 加上不必要的
/// `Clone`/`Copy`/`Debug` 边界，而这里的字段全是引用，与 `I` 无关。
pub enum CommandRef<'a, I: 'static> {
    Builtin(&'static CommandSpec<I>),
    Plugin(&'a PluginCommand),
}

impl<I> Clone for CommandRef<'_, I> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I> Copy for CommandRef<'_, I> {}

impl<I> std::fmt::Debug for CommandRef<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin(spec) => f.debug_tuple("Builtin").field(&spec.name).finish(),
            Self::Plugin(command) => f.debug_tuple("Plugin").field(command).finish(),
        }
    }
}

/// 补全候选：命令名或参数值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCandidate {
    /// 写入输入框的文本（命令含前导 `/`；参数候选为裸值）。
    pub value: String,
    /// 展示名（别名展示 `/alias`，描述仍指向主命令）。
    pub label: String,
    pub description: String,
    /// 选中后是否追加空格（参数候选为 `false`，后面可能还有自由文本）。
    pub append_space: bool,
    /// 候选来源。
    pub source: CommandSource,
}

/// 命令注册表：静态内置表 + 运行时并入的插件命令。
///
/// `I` 是前端的命令 id 类型；同一套查找/补全/门控数据被各前端复用。
pub struct CommandRegistry<I: 'static> {
    builtins: &'static [CommandSpec<I>],
    plugins: Vec<PluginCommand>,
}

// `I` 要 `'static`，因为 builtin 表是 `&'static` 切片、resolve 返回 `&'static CommandSpec`。
impl<I: 'static> CommandRegistry<I> {
    pub fn new(builtins: &'static [CommandSpec<I>]) -> Self {
        Self {
            builtins,
            plugins: Vec::new(),
        }
    }

    /// 用 `plugin/list` 的结果重建动态来源；加载失败的插件不贡献命令。
    pub fn set_plugins(&mut self, plugins: &[PluginInfo]) {
        self.plugins = plugins
            .iter()
            .filter(|plugin| plugin.error.is_none())
            .flat_map(|plugin| {
                plugin.commands.iter().map(move |command| PluginCommand {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    usage: command.usage.clone(),
                    plugin: plugin.name.clone(),
                    origin: plugin.source.clone(),
                })
            })
            .collect();
    }

    /// 直接注入插件命令（不经过 `PluginInfo`，测试与窄调用方用）。
    pub fn set_plugin_commands(&mut self, commands: Vec<PluginCommand>) {
        self.plugins = commands;
    }

    pub fn builtins(&self) -> &'static [CommandSpec<I>] {
        self.builtins
    }

    pub fn plugins(&self) -> &[PluginCommand] {
        &self.plugins
    }

    /// 名称（或别名）解析；内置命令优先于同名插件命令。
    pub fn resolve(&self, name: &str) -> Option<CommandRef<'_, I>> {
        let name = name.trim_start_matches('/');
        if let Some(spec) = self
            .builtins
            .iter()
            .find(|spec| spec.name == name || spec.aliases.contains(&name))
        {
            return Some(CommandRef::Builtin(spec));
        }
        self.plugins
            .iter()
            .find(|command| command.name == name)
            .map(CommandRef::Plugin)
    }

    /// 只找内置命令（参数补全需要 spec 的 args 字段）。
    pub fn builtin(&self, name: &str) -> Option<&'static CommandSpec<I>> {
        let name = name.trim_start_matches('/');
        self.builtins
            .iter()
            .find(|spec| spec.name == name || spec.aliases.contains(&name))
    }

    /// 命令名补全：`query` 是 `/` 之后的部分（不含空格）。
    ///
    /// `visible` 过滤当前上下文不可用的内置命令（`hidden`、flag 关闭、聚焦时不
    /// 可用于旁支、运行中不可用的命令一律不提示——它们执行也只会得到拒绝文案）。
    /// 插件命令没有 spec 元数据可判定，始终可见。别名与主名都产出候选，命中别名
    /// 时写入别名原文。
    pub fn complete(
        &self,
        query: &str,
        mut visible: impl FnMut(&CommandSpec<I>) -> bool,
    ) -> Vec<CommandCandidate> {
        let mut candidates: Vec<CommandCandidate> = self
            .builtins
            .iter()
            .filter(|spec| !spec.hidden && visible(spec))
            .flat_map(|spec| {
                std::iter::once((spec.name, spec))
                    .chain(spec.aliases.iter().map(move |alias| (*alias, spec)))
                    .filter(move |(name, _)| name.starts_with(query))
                    .map(move |(name, spec)| CommandCandidate {
                        value: format!("/{name}"),
                        label: format!("/{name}"),
                        description: spec.description.to_owned(),
                        append_space: true,
                        source: CommandSource::Builtin,
                    })
            })
            .collect();
        candidates.extend(
            self.plugins
                .iter()
                .filter(|command| command.name.starts_with(query))
                .map(|command| CommandCandidate {
                    value: format!("/{}", command.name),
                    label: format!("/{}", command.name),
                    description: if command.description.is_empty() {
                        "插件命令".to_owned()
                    } else {
                        command.description.clone()
                    },
                    append_space: true,
                    source: CommandSource::Plugin {
                        plugin: command.plugin.clone(),
                        origin: command.origin.clone(),
                    },
                }),
        );
        candidates
    }
}

/// 把插件命令转成注册表条目（`plugin/list` 之外的来源复用同一条路径）。
impl From<(&PluginInfo, &PluginCommandInfo)> for PluginCommand {
    fn from((plugin, command): (&PluginInfo, &PluginCommandInfo)) -> Self {
        Self {
            name: command.name.clone(),
            description: command.description.clone(),
            usage: command.usage.clone(),
            plugin: plugin.name.clone(),
            origin: plugin.source.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Id {
        Config,
        Quit,
        Ask,
    }

    const SPECS: &[CommandSpec<Id>] = &[
        CommandSpec {
            id: Id::Config,
            name: "config",
            aliases: &["settings"],
            usage: "",
            description: "打开设置",
            group: CommandGroup::Ui,
            needs_session: false,
            available_during_task: true,
            available_in_side: true,
            hidden: false,
            args: ArgCompletion::None,
            flag: None,
        },
        CommandSpec {
            id: Id::Quit,
            name: "quit",
            aliases: &["exit"],
            usage: "",
            description: "退出",
            group: CommandGroup::Session,
            needs_session: false,
            available_during_task: true,
            available_in_side: true,
            hidden: false,
            args: ArgCompletion::None,
            flag: None,
        },
        CommandSpec {
            id: Id::Ask,
            name: "ask",
            aliases: &[],
            usage: "<问题>",
            description: "只读问答",
            group: CommandGroup::Mode,
            needs_session: true,
            available_during_task: true,
            available_in_side: true,
            hidden: true,
            args: ArgCompletion::None,
            flag: None,
        },
    ];

    fn plugin(name: &str, commands: &[&str]) -> PluginInfo {
        PluginInfo {
            name: name.into(),
            version: None,
            description: None,
            source: "project".into(),
            dir: "/tmp".into(),
            tools: Vec::new(),
            commands: commands
                .iter()
                .map(|name| PluginCommandInfo {
                    name: (*name).into(),
                    description: String::new(),
                    usage: String::new(),
                })
                .collect(),
            error: None,
        }
    }

    #[test]
    fn resolve_matches_canonical_alias_and_plugin() {
        let mut registry = CommandRegistry::new(SPECS);
        registry.set_plugins(&[plugin("demo", &["deploy", "config"])]);
        // 别名解析到主命令；与插件同名的命令内置优先。
        match registry.resolve("settings") {
            Some(CommandRef::Builtin(spec)) => assert_eq!(spec.id, Id::Config),
            _ => panic!("settings 应解析到 config"),
        }
        match registry.resolve("config") {
            Some(CommandRef::Builtin(spec)) => assert_eq!(spec.id, Id::Config),
            _ => panic!("插件同名命令不得遮蔽内置命令"),
        }
        match registry.resolve("deploy") {
            Some(CommandRef::Plugin(command)) => assert_eq!(command.plugin, "demo"),
            _ => panic!("deploy 应解析到插件命令"),
        }
        assert!(registry.resolve("/quit").is_some(), "前导斜杠可省略");
        assert!(registry.resolve("nothing").is_none());
    }

    #[test]
    fn complete_lists_canonical_and_alias_candidates() {
        let mut registry = CommandRegistry::new(SPECS);
        registry.set_plugins(&[plugin("demo", &["deploy"])]);
        // hidden 的 /ask 不出候选；/quit 与别名 /exit 都出现。
        let values: Vec<_> = registry
            .complete("", |_| true)
            .into_iter()
            .map(|candidate| candidate.value)
            .collect();
        assert_eq!(values, ["/config", "/settings", "/quit", "/exit", "/deploy"]);

        // visible 只过滤内置命令（插件命令无 spec 元数据，始终可见）。
        let values: Vec<_> = registry
            .complete("", |spec| spec.id == Id::Config)
            .into_iter()
            .map(|candidate| candidate.value)
            .collect();
        assert_eq!(values, ["/config", "/settings", "/deploy"]);

        assert!(registry.complete("se", |_| true)[0].value == "/settings");
        assert!(registry.complete("zzz", |_| true).is_empty());
    }

    #[test]
    fn failed_plugins_contribute_no_commands() {
        let mut broken = plugin("broken", &["oops"]);
        broken.error = Some("load failed".into());
        let mut registry = CommandRegistry::new(SPECS);
        registry.set_plugins(&[broken]);
        assert!(registry.plugins().is_empty());
        assert!(registry.resolve("oops").is_none());
    }
}
