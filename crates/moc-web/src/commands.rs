//! WebUI 斜杠命令表：与 TUI/CLI 共用 `moc_protocol::commands` 的注册表结构。
//! 元数据经 `/commands.json` 交付浏览器，前端不再维护独立静态表；
//! `needs_session` 标记草稿会话不可用的命令（web 的草稿会被发送动作物化，
//! 所以 plan/autonomy/model/ask 不标）。

use moc_protocol::commands::{ArgCompletion, CommandGroup, CommandMeta, CommandSpec};
use moc_protocol::features::FeatureFlag;

/// WebUI 内置命令 id（Rust 侧登记键；前端按名字分派，不跨线传输）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    New,
    Plan,
    Goal,
    Autonomy,
    Ask,
    Retry,
    Compact,
    Model,
    Btw,
    Context,
    Usage,
    Settings,
    Jobs,
    Workflow,
    Workflows,
}

/// 自主程度命令的取值（与 `SessionAutonomy` 的 serde 名一致）。
const AUTONOMY_CHOICES: &[&str] = &["default", "dont_ask", "unattended"];

/// WebUI 内置命令表。
pub const COMMANDS: &[CommandSpec<CommandId>] = &[
    CommandSpec {
        id: CommandId::New,
        name: "new",
        aliases: &[],
        usage: "",
        description: "新建会话",
        group: CommandGroup::Session,
        needs_session: false,
        available_during_task: true,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Plan,
        name: "plan",
        aliases: &[],
        usage: "[提示]",
        description: "进入或退出计划状态，可附带提示",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Goal,
        name: "goal",
        aliases: &[],
        usage: "[<objective>|edit <objective>|pause|resume|clear]",
        description: "查看或设置会话目标；目标存在时每轮提示结束自动续跑，直到达成或叫停",
        group: CommandGroup::Session,
        needs_session: true,
        available_during_task: true,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(&["edit", "pause", "resume", "clear"]),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Autonomy,
        name: "autonomy",
        aliases: &[],
        usage: "[default|dont_ask|unattended]",
        description: "切换自主程度，无参数时循环",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(AUTONOMY_CHOICES),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Ask,
        name: "ask",
        aliases: &[],
        usage: "<问题>",
        description: "只读问答：本条提示不改动文件",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Retry,
        name: "retry",
        aliases: &[],
        usage: "",
        description: "接着当前上下文重新生成",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: false,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Compact,
        name: "compact",
        aliases: &[],
        usage: "",
        description: "压缩当前上下文",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: false,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Model,
        name: "model",
        aliases: &[],
        usage: "",
        description: "切换模型与思考强度",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Btw,
        name: "btw",
        aliases: &[],
        usage: "<问题>",
        description: "旁路提问，不进入主对话上下文",
        group: CommandGroup::Ui,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Context,
        name: "context",
        aliases: &[],
        usage: "",
        description: "查看上下文占用",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Usage,
        name: "usage",
        aliases: &[],
        usage: "",
        description: "查看本会话用量",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Settings,
        name: "settings",
        aliases: &[],
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
        id: CommandId::Jobs,
        name: "jobs",
        aliases: &[],
        usage: "",
        description: "查看和管理后台任务",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Workflow,
        name: "workflow",
        aliases: &[],
        usage: "[指令]",
        description: "激活 workflow 功能，可附带指令",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: Some(FeatureFlag::Workflow),
    },
    CommandSpec {
        id: CommandId::Workflows,
        name: "workflows",
        aliases: &[],
        usage: "",
        description: "查看 workflow run 列表",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: Some(FeatureFlag::Workflow),
    },
];

/// 交付给前端的元数据清单（剥离 `id`/`args`，`flag` 是特性键名）。
pub fn command_metas() -> Vec<CommandMeta> {
    COMMANDS.iter().map(CommandSpec::meta).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metas_serialize_with_camel_case_and_flag_keys() {
        let json = serde_json::to_string(&command_metas()).expect("序列化");
        assert!(json.contains("\"needsSession\""));
        assert!(json.contains("\"availableDuringTask\""));
        assert!(json.contains("\"group\":\"agents\""));
        // flag 展开成特性键名而不是 Rust 枚举名。
        assert!(json.contains("\"flag\":\"workflow\""));
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = COMMANDS
            .iter()
            .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
    }
}
