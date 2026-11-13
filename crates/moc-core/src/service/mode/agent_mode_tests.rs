use super::{Mode, allowed_tools, mode_allows};
use crate::tools::{Tier, ToolOrigin};
use moc_protocol::SessionEngine;

#[test]
fn clm_context_is_limited_to_clm_engine() {
    assert!(mode_allows(
        SessionEngine::ClmExperimental,
        Mode::Default,
        "clm_context",
        Tier::Write,
        ToolOrigin::Builtin,
    ));
    for engine in [SessionEngine::Standard, SessionEngine::Ptc] {
        assert!(
            !mode_allows(
                engine,
                Mode::Default,
                "clm_context",
                Tier::Write,
                ToolOrigin::Builtin
            ),
            "{engine:?} 不能开放实时上下文写入"
        );
    }
}

#[test]
fn child_agent_controls_only_exist_in_full_engines() {
    let agents = [
        "spawn_agent",
        "list_agents",
        "message_agent",
        "stop_agent",
        "wait_agent",
        "yield",
        "sidekick",
    ];
    for engine in [SessionEngine::Standard, SessionEngine::ClmExperimental] {
        let allowed = allowed_tools(engine);
        assert!(agents.iter().all(|tool| allowed.contains(tool)));
    }
    assert!(
        agents
            .iter()
            .all(|tool| !allowed_tools(SessionEngine::Ptc).contains(tool))
    );
    // 只读模式在引擎面之上收紧：spawn/stop 等控制工具被剔除。
    for mode in [Mode::Ask, Mode::Plan] {
        assert!(
            agents.iter().all(|tool| !mode_allows(
                SessionEngine::Standard,
                mode,
                tool,
                Tier::Exec,
                ToolOrigin::Builtin
            )),
            "{mode:?} 模式不能放行代理控制"
        );
    }
}

#[test]
fn mode_tightens_permission_not_engine_table() {
    // 引擎表常驻写工具与 plan 工具：进出模式不重写表。
    for engine in [SessionEngine::Standard, SessionEngine::ClmExperimental] {
        let allowed = allowed_tools(engine);
        for tool in ["edit", "write", "plan", "exit_plan"] {
            assert!(allowed.contains(&tool), "{engine:?} 引擎面缺 {tool}");
        }
    }
    // 模式只拦执行：内置写工具被拒，plan 工具按模式放行。
    assert!(!mode_allows(
        SessionEngine::Standard,
        Mode::Plan,
        "edit",
        Tier::Write,
        ToolOrigin::Builtin,
    ));
    assert!(mode_allows(
        SessionEngine::Standard,
        Mode::Plan,
        "plan",
        Tier::Write,
        ToolOrigin::Builtin,
    ));
    assert!(!mode_allows(
        SessionEngine::Standard,
        Mode::Ask,
        "plan",
        Tier::Write,
        ToolOrigin::Builtin,
    ));
    // 只读模式里非内置工具按 Tier::Read 放行，MCP（Exec）一律被拒。
    assert!(!mode_allows(
        SessionEngine::Standard,
        Mode::Ask,
        "mcp__s__t",
        Tier::Exec,
        ToolOrigin::Mcp,
    ));
    assert!(mode_allows(
        SessionEngine::Standard,
        Mode::Ask,
        "mcp__s__t",
        Tier::Read,
        ToolOrigin::Mcp,
    ));
    // PTC 引擎下模式不放宽可达面：MCP 依旧只能走 execute。
    assert!(!mode_allows(
        SessionEngine::Ptc,
        Mode::Default,
        "mcp__s__t",
        Tier::Exec,
        ToolOrigin::Mcp,
    ));
}
