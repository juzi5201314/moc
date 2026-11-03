//! REPL 斜杠命令表：每条命令一份 [`CommandSpec`] 元数据，分派、帮助与
//! 门控从这一张表取事实；与 TUI 共用 `moc_protocol::commands` 的注册表结构。
//!
//! REPL 是行式前端：没有补全、旁支聚焦与运行中并发分派，但元数据仍照实
//! 填写——`flag` 门控命令开关，其余字段供未来的调用方（或文档生成）读取。

use moc_protocol::commands::{ArgCompletion, CommandGroup, CommandRegistry, CommandSpec};
use moc_protocol::features::FeatureFlag;

/// REPL 内置命令 id：`run_command` 按它分派到具体处理器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    Help,
    Quit,
    Cancel,
    New,
    Sessions,
    Engine,
    ClmExperimental,
    Autonomy,
    Permission,
    Plan,
    Goal,
    Ask,
    Approve,
    Retry,
    Agents,
    Compact,
    Usage,
    Model,
    Plugins,
    Features,
}

/// 自主程度命令的取值（与 `SessionAutonomy` 的 serde 名一致）。
const AUTONOMY_CHOICES: &[&str] = &["default", "dont_ask", "unattended"];

/// 引擎命令的取值（`parse_engine` 接受的别名另行处理）。
const ENGINE_CHOICES: &[&str] = &["standard", "ptc", "clm"];

/// REPL 内置命令表：同一批记录驱动 `run_command` 分派与 `/help` 文本。
pub const COMMANDS: &[CommandSpec<CommandId>] = &[
    CommandSpec {
        id: CommandId::New,
        name: "new",
        aliases: &[],
        usage: "",
        description: "创建新会话",
        group: CommandGroup::Session,
        needs_session: false,
        available_during_task: true,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Sessions,
        name: "sessions",
        aliases: &[],
        usage: "",
        description: "列出会话",
        group: CommandGroup::Session,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Cancel,
        name: "cancel",
        aliases: &[],
        usage: "",
        description: "取消当前提示（运行中按 Ctrl+C）",
        group: CommandGroup::Session,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Quit,
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
        id: CommandId::Engine,
        name: "engine",
        aliases: &[],
        usage: "<standard|ptc|clm>",
        description: "切换会话引擎（跑过首条提示后引擎锁定）",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(ENGINE_CHOICES),
        flag: None,
    },
    CommandSpec {
        id: CommandId::ClmExperimental,
        name: "clm-experimental",
        aliases: &[],
        usage: "",
        description: "启用 CLM 实验引擎；首次请求后引擎锁定",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: Some(FeatureFlag::Clm),
    },
    CommandSpec {
        id: CommandId::Autonomy,
        name: "autonomy",
        aliases: &[],
        usage: "[default|dont_ask|unattended]",
        description: "切换自主程度；无参数时循环",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(AUTONOMY_CHOICES),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Permission,
        name: "permission",
        aliases: &[],
        usage: "<full|workspace|workspace-only|clear>",
        description: "切换命令权限或清空本会话沙盒授权",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(&["full", "workspace", "workspace-only", "clear"]),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Plan,
        name: "plan",
        aliases: &[],
        usage: "[提示词]",
        description: "进入/退出计划模式；带提示词时进入并发送",
        group: CommandGroup::Mode,
        needs_session: true,
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
        id: CommandId::Approve,
        name: "approve",
        aliases: &[],
        usage: "<execute|compact|new [引擎]|reject>",
        description: "处理已提交的计划",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(&["execute", "compact", "new", "reject"]),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Ask,
        name: "ask",
        aliases: &[],
        usage: "<问题>",
        description: "仅本条消息按问答模式（只读）处理",
        group: CommandGroup::Mode,
        needs_session: true,
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
        description: "接着现有上下文继续跑（不追加用户消息，仅空闲时可用）",
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
        id: CommandId::Usage,
        name: "usage",
        aliases: &[],
        usage: "",
        description: "查看当前会话模型用量",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Model,
        name: "model",
        aliases: &[],
        usage: "<selector>",
        description: "切换模型（装不下当前上下文时先询问再压缩）",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Agents,
        name: "agents",
        aliases: &[],
        usage: "",
        description: "列出当前会话的直接子代理（创建、交流、停止都在 TUI）",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Plugins,
        name: "plugins",
        aliases: &[],
        usage: "[reload]",
        description: "查看已加载的插件；reload 重载插件宿主并重新发现",
        group: CommandGroup::Plugins,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::Choices(&["reload"]),
        flag: Some(FeatureFlag::Plugins),
    },
    CommandSpec {
        id: CommandId::Help,
        name: "help",
        aliases: &[],
        usage: "",
        description: "显示本帮助",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Features,
        name: "features",
        aliases: &[],
        usage: "",
        description: "特性开关一览：阶段、状态与上下文成本",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
];

/// REPL 命令注册表类型；插件命令经 `plugin/list` 在分派时确认，不进注册表。
pub type Registry = CommandRegistry<CommandId>;

/// 以 REPL 命令表建注册表。
pub fn registry() -> Registry {
    CommandRegistry::new(COMMANDS)
}

/// `/help` 文本：按组生成，命令与别名都成行；flag 关闭的命令仍列出并注明。
pub fn help_text(features: &moc_protocol::features::FeatureStates) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for group in [
        CommandGroup::Session,
        CommandGroup::Mode,
        CommandGroup::Context,
        CommandGroup::Agents,
        CommandGroup::Plugins,
        CommandGroup::Ui,
    ] {
        let mut rows: Vec<(String, String)> = Vec::new();
        for spec in COMMANDS
            .iter()
            .filter(|spec| !spec.hidden && spec.group == group)
        {
            let usage = if spec.usage.is_empty() {
                format!("/{}", spec.name)
            } else {
                format!("/{} {}", spec.name, spec.usage)
            };
            let disabled = spec.flag.is_some_and(|flag| !features.enabled(flag));
            rows.push((
                usage,
                if disabled {
                    format!("{}（特性已禁用）", spec.description)
                } else {
                    spec.description.to_owned()
                },
            ));
            for alias in spec.aliases {
                rows.push((
                    format!("/{alias}"),
                    format!("退出（/{} 的别名）", spec.name),
                ));
            }
        }
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(text, "{}：", group.label());
        let width = rows.iter().map(|(usage, _)| usage.len()).max().unwrap_or(0);
        for (usage, description) in rows {
            let _ = writeln!(text, "  {usage:<width$}  {description}");
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_resolves_by_name_and_alias() {
        let registry = registry();
        let mut ids: Vec<CommandId> = Vec::new();
        for spec in COMMANDS {
            assert_eq!(registry.builtin(spec.name).map(|s| s.id), Some(spec.id));
            ids.push(spec.id);
            for alias in spec.aliases {
                match registry.resolve(alias) {
                    Some(moc_protocol::commands::CommandRef::Builtin(found)) => {
                        assert_eq!(found.id, spec.id, "别名 /{alias} 未指回原命令")
                    }
                    _ => panic!("别名 /{alias} 未解析为内置命令"),
                }
            }
        }
        let total = ids.len();
        ids.sort_by_key(|id| *id as u8);
        ids.dedup();
        assert_eq!(ids.len(), total, "存在重复 CommandId");
    }

    #[test]
    fn aliases_are_unique_across_table() {
        let mut all: Vec<&str> = COMMANDS
            .iter()
            .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
            .collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total, "命令名/别名存在重复");
    }

    /// 帮助从注册表生成，天然不会漏命令；守住「每条可见命令与别名都有行」。
    #[test]
    fn help_lists_every_visible_command_and_alias() {
        let text = help_text(&moc_protocol::features::FeatureStates::defaults());
        for spec in COMMANDS.iter().filter(|spec| !spec.hidden) {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "帮助缺 /{}",
                spec.name
            );
            for alias in spec.aliases {
                assert!(text.contains(&format!("/{alias}")), "帮助缺别名 /{alias}");
            }
        }
        // 关闭 flag 的命令注明禁用而不是消失。
        let mut features = moc_protocol::features::FeatureStates::defaults();
        features.set(FeatureFlag::Clm, false);
        let text = help_text(&features);
        assert!(text.contains("特性已禁用"));
    }
}
