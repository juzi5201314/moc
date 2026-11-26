//! WebUI 斜杠命令表：与 TUI/CLI 共用 `moc_protocol::commands` 的注册表结构。
//! 元数据经 `/commands.json` 交付浏览器，前端不再维护独立静态表；
//! `needs_session` 标记草稿会话不可用的命令（web 的草稿会被发送动作物化，
//! 所以 plan/autonomy/model/ask 不标）。

use moc_protocol::commands::{
    ArgCompletion, CommandGroup, CommandMeta, CommandParamSpec, CommandSpec, DynamicArg,
    DynamicSource, ParamChoiceSpec, ParamKind,
};
use moc_protocol::features::FeatureFlag;

/// WebUI 内置命令 id（Rust 侧登记键；前端按名字分派，不跨线传输）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    New,
    Resume,
    Close,
    Plan,
    ConfirmPlan,
    Goal,
    Autonomy,
    Permission,
    Ask,
    Retry,
    Compact,
    CompactSkills,
    Model,
    Engine,
    ClmExperimental,
    Btw,
    Context,
    Tools,
    Usage,
    DebugInfo,
    Export,
    Session,
    Settings,
    Agents,
    Jobs,
    Workflow,
    Workflows,
    Mcp,
    Plugins,
    Features,
    Help,
    Doctor,
    Monitor,
}

/// 自主程度命令的取值（与 `SessionAutonomy` 的 serde 名一致）。
const AUTONOMY_CHOICES: &[&str] = &["default", "dont_ask", "unattended"];

/// WebUI 内置命令表。
pub const COMMANDS: &[CommandSpec<CommandId>] = &[
    CommandSpec {
        id: CommandId::Export,
        name: "export",
        aliases: &[],
        usage: "[path]",
        description: "web.commands.export.desc",
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
        description: "web.commands.debuginfo.desc",
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
        description: "web.commands.session.desc",
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
        description: "web.commands.new.desc",
        group: CommandGroup::Session,
        needs_session: false,
        available_during_task: true,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Resume,
        name: "resume",
        aliases: &[],
        usage: "",
        description: "web.commands.resume.desc",
        group: CommandGroup::Session,
        needs_session: false,
        available_during_task: true,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Close,
        name: "close",
        aliases: &[],
        usage: "",
        description: "web.commands.close.desc",
        group: CommandGroup::Session,
        needs_session: true,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Plan,
        name: "plan",
        aliases: &[],
        usage: "web.commands.plan.usage",
        description: "web.commands.plan.desc",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::ConfirmPlan,
        name: "confirm-plan",
        aliases: &[],
        usage: "",
        description: "web.commands.confirm_plan.desc",
        group: CommandGroup::Mode,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Goal,
        name: "goal",
        aliases: &[],
        usage: "[<objective>|edit <objective>|pause|resume|clear]",
        description: "web.commands.goal.desc",
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
        description: "web.commands.autonomy.desc",
        group: CommandGroup::Mode,
        needs_session: false,
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
        usage: "web.commands.permission.usage",
        description: "web.commands.permission.desc",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::Dynamic(DynamicArg::SandboxMode),
        flag: None,
    },
    CommandSpec {
        id: CommandId::Ask,
        name: "ask",
        aliases: &[],
        usage: "web.commands.ask.usage",
        description: "web.commands.ask.desc",
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
        description: "web.commands.retry.desc",
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
        description: "web.commands.compact_skills.desc",
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
        usage: "web.commands.compact.usage",
        description: "web.commands.compact.desc",
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
        description: "web.commands.model.desc",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Engine,
        name: "engine",
        aliases: &[],
        usage: "",
        description: "web.commands.engine.desc",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::ClmExperimental,
        name: "clm-experimental",
        aliases: &[],
        usage: "",
        description: "web.commands.clm_experimental.desc",
        group: CommandGroup::Mode,
        needs_session: false,
        available_during_task: false,
        available_in_side: false,
        hidden: false,
        args: ArgCompletion::None,
        flag: Some(FeatureFlag::Clm),
    },
    CommandSpec {
        id: CommandId::Btw,
        name: "btw",
        aliases: &[],
        usage: "web.commands.btw.usage",
        description: "web.commands.btw.desc",
        group: CommandGroup::Ui,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Tools,
        name: "tools",
        aliases: &[],
        usage: "",
        description: "web.commands.tools.desc",
        group: CommandGroup::Context,
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
        description: "web.commands.context.desc",
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
        description: "web.commands.usage.desc",
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
        description: "web.commands.settings.desc",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Agents,
        name: "agents",
        aliases: &[],
        usage: "",
        description: "web.commands.agents.desc",
        group: CommandGroup::Agents,
        needs_session: true,
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
        description: "web.commands.jobs.desc",
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
        usage: "web.commands.workflow.usage",
        description: "web.commands.workflow.desc",
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
        description: "web.commands.workflows.desc",
        group: CommandGroup::Agents,
        needs_session: true,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: Some(FeatureFlag::Workflow),
    },
    CommandSpec {
        id: CommandId::Mcp,
        name: "mcp",
        aliases: &[],
        usage: "[reconnect [server]]",
        description: "web.commands.mcp.desc",
        group: CommandGroup::Plugins,
        needs_session: false,
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
        description: "web.commands.plugins.desc",
        group: CommandGroup::Plugins,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::Choices(&["reload"]),
        flag: Some(FeatureFlag::Plugins),
    },
    CommandSpec {
        id: CommandId::Features,
        name: "features",
        aliases: &[],
        usage: "",
        description: "web.commands.features.desc",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Help,
        name: "help",
        aliases: &[],
        usage: "",
        description: "web.commands.help.desc",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Doctor,
        name: "doctor",
        aliases: &[],
        usage: "",
        description: "web.commands.doctor.desc",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
    CommandSpec {
        id: CommandId::Monitor,
        name: "monitor",
        aliases: &["top"],
        usage: "",
        description: "web.commands.monitor.desc",
        group: CommandGroup::Ui,
        needs_session: false,
        available_during_task: true,
        available_in_side: true,
        hidden: false,
        args: ArgCompletion::None,
        flag: None,
    },
];

const EXPORT_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "path",
    kind: ParamKind::Path,
    required: false,
    accepts_text: false,
    description: "web.commands.export.param.path.desc",
    placeholder: "web.commands.export.param.path.placeholder",
    default_behavior: "web.commands.export.param.path.default",
    choices: &[],
    dynamic: None,
}];

const PLAN_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "prompt",
    kind: ParamKind::Text,
    required: false,
    accepts_text: false,
    description: "web.commands.plan.param.prompt.desc",
    placeholder: "web.commands.plan.param.prompt.placeholder",
    default_behavior: "web.commands.plan.param.prompt.default",
    choices: &[],
    dynamic: None,
}];

const GOAL_EDIT_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "objective",
    kind: ParamKind::Text,
    required: true,
    accepts_text: false,
    description: "web.commands.goal.param.edit_objective.desc",
    placeholder: "web.commands.goal.param.edit_objective.placeholder",
    default_behavior: "",
    choices: &[],
    dynamic: None,
}];

const GOAL_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "objective",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: true,
    description: "web.commands.goal.param.action.desc",
    placeholder: "web.commands.goal.param.action.placeholder",
    default_behavior: "web.commands.goal.param.action.default",
    choices: &[
        ParamChoiceSpec {
            value: "edit",
            label: "",
            description: "web.commands.goal.choice.edit.desc",
            params: GOAL_EDIT_PARAMS,
        },
        ParamChoiceSpec {
            value: "pause",
            label: "",
            description: "web.commands.goal.choice.pause.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "resume",
            label: "",
            description: "web.commands.goal.choice.resume.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "clear",
            label: "",
            description: "web.commands.goal.choice.clear.desc",
            params: &[],
        },
    ],
    dynamic: None,
}];

const AUTONOMY_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "level",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: false,
    description: "web.commands.autonomy.param.level.desc",
    placeholder: "",
    default_behavior: "web.commands.autonomy.param.level.default",
    choices: &[
        ParamChoiceSpec {
            value: "default",
            label: "",
            description: "web.commands.autonomy.choice.default.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "dont_ask",
            label: "",
            description: "web.commands.autonomy.choice.dont_ask.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "unattended",
            label: "",
            description: "web.commands.autonomy.choice.unattended.desc",
            params: &[],
        },
    ],
    dynamic: None,
}];

const PERMISSION_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "mode",
    kind: ParamKind::Dynamic,
    required: false,
    accepts_text: false,
    description: "web.commands.permission.param.mode.desc",
    placeholder: "",
    default_behavior: "web.commands.permission.param.mode.default",
    choices: &[ParamChoiceSpec {
        value: "clear",
        label: "",
        description: "web.commands.permission.choice.clear.desc",
        params: &[],
    }],
    dynamic: Some(DynamicSource::SandboxMode),
}];

const ASK_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "question",
    kind: ParamKind::Text,
    required: true,
    accepts_text: false,
    description: "web.commands.ask.param.question.desc",
    placeholder: "web.commands.ask.param.question.placeholder",
    default_behavior: "",
    choices: &[],
    dynamic: None,
}];

const COMPACT_SKILLS_CLEANUP_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "scope",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: false,
    description: "web.commands.compact_skills.param.cleanup_scope.desc",
    placeholder: "",
    default_behavior: "web.commands.compact_skills.param.cleanup_scope.default",
    choices: &[ParamChoiceSpec {
        value: "all",
        label: "",
        description: "web.commands.compact_skills.choice.all.desc",
        params: &[],
    }],
    dynamic: None,
}];

const COMPACT_SKILLS_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "action",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: false,
    description: "web.commands.compact_skills.param.action.desc",
    placeholder: "",
    default_behavior: "web.commands.compact_skills.param.action.default",
    choices: &[
        ParamChoiceSpec {
            value: "compact",
            label: "",
            description: "web.commands.compact_skills.choice.compact.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "force",
            label: "",
            description: "web.commands.compact_skills.choice.force.desc",
            params: &[],
        },
        ParamChoiceSpec {
            value: "cleanup",
            label: "",
            description: "web.commands.compact_skills.choice.cleanup.desc",
            params: COMPACT_SKILLS_CLEANUP_PARAMS,
        },
    ],
    dynamic: None,
}];

const COMPACT_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "instructions",
    kind: ParamKind::Text,
    required: false,
    accepts_text: false,
    description: "web.commands.compact.param.instructions.desc",
    placeholder: "web.commands.compact.param.instructions.placeholder",
    default_behavior: "web.commands.compact.param.instructions.default",
    choices: &[],
    dynamic: None,
}];

const BTW_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "question",
    kind: ParamKind::Text,
    required: true,
    accepts_text: false,
    description: "web.commands.btw.param.question.desc",
    placeholder: "web.commands.btw.param.question.placeholder",
    default_behavior: "",
    choices: &[],
    dynamic: None,
}];

const WORKFLOW_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "instruction",
    kind: ParamKind::Text,
    required: false,
    accepts_text: false,
    description: "web.commands.workflow.param.instruction.desc",
    placeholder: "web.commands.workflow.param.instruction.placeholder",
    default_behavior: "web.commands.workflow.param.instruction.default",
    choices: &[],
    dynamic: None,
}];

const MCP_RECONNECT_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "server",
    kind: ParamKind::Dynamic,
    required: false,
    accepts_text: false,
    description: "web.commands.mcp.param.server.desc",
    placeholder: "web.commands.mcp.param.server.placeholder",
    default_behavior: "web.commands.mcp.param.server.default",
    choices: &[],
    dynamic: Some(DynamicSource::McpServer),
}];

const MCP_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "action",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: false,
    description: "web.commands.mcp.param.action.desc",
    placeholder: "",
    default_behavior: "web.commands.mcp.param.action.default",
    choices: &[ParamChoiceSpec {
        value: "reconnect",
        label: "",
        description: "web.commands.mcp.choice.reconnect.desc",
        params: MCP_RECONNECT_PARAMS,
    }],
    dynamic: None,
}];

const PLUGINS_PARAMS: &[CommandParamSpec] = &[CommandParamSpec {
    name: "action",
    kind: ParamKind::Choice,
    required: false,
    accepts_text: false,
    description: "web.commands.plugins.param.action.desc",
    placeholder: "",
    default_behavior: "web.commands.plugins.param.action.default",
    choices: &[ParamChoiceSpec {
        value: "reload",
        label: "",
        description: "web.commands.plugins.choice.reload.desc",
        params: &[],
    }],
    dynamic: None,
}];

fn params_for(id: CommandId) -> &'static [CommandParamSpec] {
    match id {
        CommandId::Export => EXPORT_PARAMS,
        CommandId::Plan => PLAN_PARAMS,
        CommandId::Goal => GOAL_PARAMS,
        CommandId::Autonomy => AUTONOMY_PARAMS,
        CommandId::Permission => PERMISSION_PARAMS,
        CommandId::Ask => ASK_PARAMS,
        CommandId::CompactSkills => COMPACT_SKILLS_PARAMS,
        CommandId::Compact => COMPACT_PARAMS,
        CommandId::Btw => BTW_PARAMS,
        CommandId::Workflow => WORKFLOW_PARAMS,
        CommandId::Mcp => MCP_PARAMS,
        CommandId::Plugins => PLUGINS_PARAMS,
        _ => &[],
    }
}

/// 交付给前端的元数据清单（剥离 `id`/`args`，附带结构化 `params`，`flag` 是特性键名）。
pub fn command_metas() -> Vec<CommandMeta> {
    COMMANDS
        .iter()
        .map(|spec| spec.meta_with_params(params_for(spec.id)))
        .collect()
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
        assert!(json.contains("\"defaultBehavior\""));
        assert!(json.contains("\"acceptsText\":true"));
        assert!(json.contains("\"dynamic\":\"mcp_server\""));
        assert!(json.contains("\"dynamic\":\"sandbox_mode\""));
    }

    #[test]
    fn param_specs_match_usage_and_translate_cleanly() {
        fn check_params(params: &[CommandParamSpec]) {
            for param in params {
                assert!(
                    moc_protocol::commands::try_translate(param.description).is_some(),
                    "参数 {} 缺少 locale 键 {}",
                    param.name,
                    param.description
                );
                if !param.placeholder.is_empty() {
                    assert!(
                        moc_protocol::commands::try_translate(param.placeholder).is_some(),
                        "参数 {} 缺少 placeholder 键 {}",
                        param.name,
                        param.placeholder
                    );
                }
                if !param.default_behavior.is_empty() {
                    assert!(
                        moc_protocol::commands::try_translate(param.default_behavior).is_some(),
                        "参数 {} 缺少 default_behavior 键 {}",
                        param.name,
                        param.default_behavior
                    );
                }
                for choice in param.choices {
                    assert!(
                        moc_protocol::commands::try_translate(choice.description).is_some(),
                        "选项 {} 缺少 locale 键 {}",
                        choice.value,
                        choice.description
                    );
                    check_params(choice.params);
                }
            }
        }

        for spec in COMMANDS {
            let params = params_for(spec.id);
            // 带 usage 的内置命令一律声明结构化参数，避免漏掉前端引导。
            assert_eq!(
                !spec.usage.is_empty(),
                !params.is_empty(),
                "命令 /{} 的 usage 与结构化 params 必须成对维护",
                spec.name
            );
            check_params(params);
        }
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
