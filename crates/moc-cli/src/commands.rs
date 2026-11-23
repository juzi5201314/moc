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
    CompactSkills,
    Usage,
    DebugInfo,
    Export,
    Session,
    Model,
    Mcp,
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
        id: CommandId::Export,
        name: "export",
        aliases: &[],
        usage: "[path]",
        description: "cli.commands.export.desc",
        group: CommandGroup::Session,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::DebugInfo,
        name: "debuginfo",
        aliases: &[],
        usage: "",
        description: "cli.commands.debuginfo.desc",
        group: CommandGroup::Ui,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Session,
        name: "session",
        aliases: &[],
        usage: "",
        description: "cli.commands.session.desc",
        group: CommandGroup::Session,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::New,
        name: "new",
        aliases: &[],
        usage: "",
        description: "cli.commands.new.desc",
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
        description: "cli.commands.sessions.desc",
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
        description: "cli.commands.cancel.desc",
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
        description: "cli.commands.quit.desc",
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
        description: "cli.commands.engine.desc",
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
        description: "cli.commands.clm_experimental.desc",
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
        description: "cli.commands.autonomy.desc",
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
        description: "cli.commands.permission.desc",
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
        usage: "cli.commands.plan.usage",
        description: "cli.commands.plan.desc",
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
        description: "cli.commands.goal.desc",
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
        usage: "cli.commands.approve.usage",
        description: "cli.commands.approve.desc",
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
        usage: "cli.commands.ask.usage",
        description: "cli.commands.ask.desc",
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
        description: "cli.commands.retry.desc",
        group: CommandGroup::Context,
        needs_session: true,
        available_during_task: false,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::CompactSkills,
        name: "compact-skills",
        aliases: &[],
        usage: "[compact | force | cleanup [all]]",
        description: "cli.commands.compact_skills.desc",
        group: CommandGroup::Context,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Choices(&["compact", "force", "cleanup", "cleanup all"]),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Compact,
        name: "compact",
        aliases: &[],
        usage: "",
        description: "cli.commands.compact.desc",
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
        description: "cli.commands.usage.desc",
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
        description: "cli.commands.model.desc",
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
        description: "cli.commands.agents.desc",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Mcp,
        name: "mcp",
        aliases: &[],
        usage: "[reconnect [server]]",
        description: "cli.commands.mcp.desc",
        group: CommandGroup::Plugins,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::Choices(&["reconnect"]),
        flag: Some(FeatureFlag::Mcp),
    },
    CommandSpec {
        id: CommandId::Plugins,
        name: "plugins",
        aliases: &[],
        usage: "[reload]",
        description: "cli.commands.plugins.desc",
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
        description: "cli.commands.help.desc",
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
        description: "cli.commands.features.desc",
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
    use moc_protocol::commands::{description_text, usage_text};
    use rust_i18n::t;
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
                format!("/{} {}", spec.name, usage_text(spec.usage))
            };
            let disabled = spec.flag.is_some_and(|flag| !features.enabled(flag));
            let description = description_text(spec.description);
            rows.push((
                usage,
                if disabled {
                    t!("cli.help.disabled", desc = description.as_ref()).into_owned()
                } else {
                    description.as_ref().to_owned()
                },
            ));
            for alias in spec.aliases {
                rows.push((
                    format!("/{alias}"),
                    t!(
                        "cli.help.alias_of",
                        desc = description.as_ref(),
                        name = spec.name
                    )
                    .into_owned(),
                ));
            }
        }
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(text, "{}", t!("cli.help.group_fmt", group = group.label()));
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
        let export = registry.builtin("export").expect("注册 /export");
        assert_eq!(export.id, CommandId::Export);
        assert_eq!(export.usage, "[path]");
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
        rust_i18n::set_locale("zh");
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
