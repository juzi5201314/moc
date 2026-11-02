//! workflow 工具：仅会话激活后进入 catalog（激活不造工具，只放行执行与可见性）。
//! 路由纪律写进描述：仅当用户点名 workflow/工作流 时调用，且先加载 workflow-authoring skill。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::error::CoreError;
use crate::tools::{Tier, Tool, ToolCtx, ToolError, ToolOutcome};
use moc_llm::ToolDef;

const SKILL: &str = "workflow-authoring";

/// workflow 工具名集：未激活会话在 catalog/声明/signature 中一律过滤（零泄漏）。
pub const WORKFLOW_TOOLS: &[&str] = &[
    "create_workflow",
    "amend_workflow",
    "resume_workflow_run",
    "stop_workflow_run",
    "fill_workflow_hole",
    "save_workflow",
    "list_saved_workflows",
    "workflow_status",
    "eval_workflow_snippet",
];


/// skill gate：提交脚本的工具要求模型可见历史里有成功的 workflow-authoring 加载。
/// 豁免：create{saved}、无 script/path 的 amend、resume/stop/status/list/fill/save。
fn skill_gate(ctx: &ToolCtx) -> Result<(), ToolError> {
    let loaded = ctx.loaded_skills.read();
    if loaded.contains(SKILL) {
        Ok(())
    } else {
        Err(ToolError::new(
            "E_WORKFLOW_SKILL",
            format!("load the `{SKILL}` skill first (tool `skill` or `skills` command), then retry"),
        ))
    }
}

/// 激活 gate：会话未激活 → 明确错误（正常路径下工具根本不可见，这里是防线）。
async fn activated(ctx: &ToolCtx) -> Result<Arc<super::WorkflowManager>, ToolError> {
    let active = ctx.state.lock().await.workflow_activated;
    if !active {
        return Err(ToolError::new(
            "E_WORKFLOW_INACTIVE",
            "workflows are not activated in this session (session/workflow_activate)",
        ));
    }
    let runtime = ctx.core.runtime(&ctx.session_id).await;
    Ok(runtime
        .workflows
        .get_or_init(|| async {
            super::WorkflowManager::new(Arc::clone(&ctx.core), &ctx.session_id, ctx.paths.clone())
        })
        .await
        .clone())
}

fn wf_outcome(v: Value) -> Result<ToolOutcome, ToolError> {
    Ok(ToolOutcome::success(v.to_string()))
}

macro_rules! wf_tool {
    ($name:ident, $tool_name:literal, $desc:expr, $schema:literal, $run:ident) => {
        pub struct $name;
        impl Tool for $name {
            fn name(&self) -> &str { $tool_name }
            fn description(&self) -> &str { $desc }
            fn tier(&self) -> Tier { Tier::Write }
            fn parallel_safe(&self) -> bool { false }
            fn def(&self) -> ToolDef {
                ToolDef::new($tool_name, serde_json::from_str($schema).unwrap())
            }
            fn run<'a>(
                &'a self,
                ctx: &'a ToolCtx,
                args: Value,
            ) -> futures_util::future::BoxFuture<'a, Result<ToolOutcome, ToolError>> {
                Box::pin(self.$run(ctx, args))
            }
        }
        impl $name {}
    };
}

wf_tool!(CreateWorkflowTool, "create_workflow", concat!(
    "Launch a multi-actor dynamic workflow from a TypeScript script (or a saved one).",
    " Call only when the user asks for a workflow; otherwise use spawn_agent or do it yourself.",
    " Load the workflow-authoring skill before authoring scripts."),
    r#"{"type":"object","properties":{
        "script":{"type":"string","description":"Inline TypeScript workflow source"},
        "saved":{"type":"string","description":"Name of a saved workflow"},
        "path":{"type":"string","description":"Path to a .ts workflow file"},
        "args":{"description":"Value for the script's `args` global"},
        "max_concurrency":{"type":"integer","description":"In-flight ask cap (1..ceiling)"},
        "subagent_model":{"type":"string","description":"Default model for actors without a persona model"}},
        "additionalProperties":false}"#,
    run_create);

wf_tool!(AmendWorkflowTool, "amend_workflow", concat!(
    "Amend a workflow: new run that inherits named actors' matching asks from a previous run.",
    " Call only when the user asks to amend a workflow."),
    r#"{"type":"object","properties":{
        "run_id":{"type":"string","description":"Predecessor run to amend from"},
        "script":{"type":"string","description":"Amended TypeScript source (omit to reuse predecessor's draft)"},
        "path":{"type":"string"},
        "subagent_model":{"type":"string"}},
        "required":["run_id"],"additionalProperties":false}"#,
    run_amend);

wf_tool!(ResumeWorkflowTool, "resume_workflow_run", "Resume a stopped workflow run; journal hits replay with zero cost.", 
    r#"{"type":"object","properties":{"run_id":{"type":"string"}},"required":["run_id"],"additionalProperties":false}"#,
    run_resume);

wf_tool!(StopWorkflowTool, "stop_workflow_run", "Stop a running workflow (terminal stopped(user); resumable).",
    r#"{"type":"object","properties":{"run_id":{"type":"string"}},"required":["run_id"],"additionalProperties":false}"#,
    run_stop);

wf_tool!(FillHoleTool, "fill_workflow_hole", "Fill an open hole in a running workflow with a TypeScript function body.",
    r#"{"type":"object","properties":{"run_id":{"type":"string"},"hole":{"type":"string"},"body":{"type":"string"}},"required":["run_id","hole","body"],"additionalProperties":false}"#,
    run_fill);

wf_tool!(SaveWorkflowTool, "save_workflow", "Save a workflow script (from a run or a path) for reuse.",
    r#"{"type":"object","properties":{"name":{"type":"string"},"run_id":{"type":"string"},"path":{"type":"string"},"description":{"type":"string"},"args":{"type":"array","items":{"type":"object"}},"scope":{"type":"string","enum":["project","global"]}},"required":["name","description"],"additionalProperties":false}"#,
    run_save);

wf_tool!(ListSavedTool, "list_saved_workflows", "List saved workflows (project and global scope).",
    r#"{"type":"object","properties":{},"additionalProperties":false}"#,
    run_list_saved);

wf_tool!(WorkflowStatusTool, "workflow_status", "Show workflow run status (all runs or one).",
    r#"{"type":"object","properties":{"run_id":{"type":"string"}},"additionalProperties":false}"#,
    run_status);

wf_tool!(EvalSnippetTool, "eval_workflow_snippet", concat!(
    "Evaluate a deterministic workflow snippet synchronously (no agents: args/run/log/files/git/world.run/channel/future only).",
    " Use to probe before authoring a full workflow."),
    r#"{"type":"object","properties":{"code":{"type":"string"},"path":{"type":"string"},"timeout_secs":{"type":"integer","description":"Default 60, max 600"}},"additionalProperties":false}"#,
    run_eval);

// —— 实现 ——

impl CreateWorkflowTool {
    async fn run_create(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        // skill gate：saved 豁免（脚本已定稿），其余提交脚本的路径都要过。
        let source = match (args.get("saved").and_then(|v| v.as_str()), args.get("script").and_then(|v| v.as_str()), args.get("path").and_then(|v| v.as_str())) {
            (Some(saved), _, _) => load_saved(ctx, saved)?,
            (None, Some(script), _) => script.to_string(),
            (None, None, Some(path)) => read_script(ctx, path)?,
            _ => return Err(ToolError::new("E_ARGS", "create_workflow requires `script`, `saved` or `path`")),
        };
        if args.get("saved").is_none() {
            skill_gate(ctx)?;
        }
        launch_flow(ctx, &manager, source, args, "fresh", None).await
    }
}

impl AmendWorkflowTool {
    async fn run_amend(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        let run_id = args.get("run_id").and_then(|v| v.as_str()).map(str::to_string)
            .ok_or_else(|| ToolError::new("E_ARGS", "amend_workflow requires `run_id`"))?;
        let prior_dir = manager.run_dir(&run_id);
        let source = match (args.get("script").and_then(|v| v.as_str()), args.get("path").and_then(|v| v.as_str())) {
            (Some(script), _) => script.to_string(),
            (None, Some(path)) => read_script(ctx, path)?,
            // 无 script/path：复用前驱草稿（豁免 skill gate——脚本已定稿）。
            (None, None) => std::fs::read_to_string(prior_dir.join("script.ts"))
                .map_err(|e| ToolError::new("E_ARGS", format!("predecessor draft unreadable: {e}")))?,
        };
        if args.get("script").is_some() || args.get("path").is_some() {
            skill_gate(ctx)?;
        }
        launch_flow(ctx, &manager, source, args, "amend", Some(run_id)).await
    }
}

impl ResumeWorkflowTool {
    async fn run_resume(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        let run_id = args.get("run_id").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("E_ARGS", "resume_workflow_run requires `run_id`"))?;
        resume_run(ctx, &manager, run_id).await
    }
}

impl StopWorkflowTool {
    async fn run_stop(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        let run_id = args.get("run_id").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("E_ARGS", "stop_workflow_run requires `run_id`"))?;
        let summary = manager.stop(run_id).await.map_err(tool_err)?;
        wf_outcome(serde_json::to_value(summary).unwrap_or_default())
    }
}

impl FillHoleTool {
    async fn run_fill(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        let run_id = args.get("run_id").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("E_ARGS", "fill_workflow_hole requires `run_id`"))?;
        let hole = args.get("hole").and_then(|v| v.as_str()).unwrap_or_default();
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or_default();
        // hole 体跑在 hole 所在 phase 的上下文里：用该 phase 前缀编译（site_id
        // 带 `hole@<名>/` 前缀避免与脚本内站点冲突），交付降级后源码——裸
        // `world.run` 没有 site_id，guest eval 时无法寻址 facade。
        let phase = manager
            .meta(run_id)
            .ok()
            .and_then(|m| {
                let site_id = m
                    .holes
                    .iter()
                    .find(|h| h["name"].as_str() == Some(hole))?
                    ["site_id"]
                    .as_str()?
                    .to_string();
                m.plan
                    .get("sites")
                    .and_then(|s| s.as_array())
                    .and_then(|sites| {
                        sites.iter().find_map(|s| {
                            (s.get("site_id").and_then(|v| v.as_str()) == Some(site_id.as_str()))
                                .then(|| s.get("phase").and_then(|v| v.as_str()).map(str::to_string))
                                .flatten()
                        })
                    })
            })
            .unwrap_or_default();
        let source = if phase.is_empty() {
            body.to_string()
        } else {
            format!("phase({phase:?});\n{body}")
        };
        let compiled = crate::workflow::compile::compile(
            &source,
            &crate::workflow::compile::CompileOptions {
                site_prefix: Some(format!("hole@{hole}/")),
                ..Default::default()
            },
        );
        match compiled {
            Err(diags) => Err(ToolError::new(
                "E_WORKFLOW_COMPILE",
                format!("hole body has diagnostics: {}", diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>().join("; ")),
            )),
            Ok(c) => {
                // 引入未批准的 world.run 命令 → hole_fill 审批（只列新增命令）。
                let known: std::collections::BTreeSet<String> =
                    manager.meta(run_id).map(|m| m.commands).unwrap_or_default().into_iter().collect();
                let new_commands: Vec<String> = c
                    .plan
                    .commands
                    .iter()
                    .map(|c| &c.command)
                    .filter(|c| !known.contains(*c))
                    .cloned()
                    .collect();
                if !new_commands.is_empty() {
                    let approval = moc_protocol::WorkflowApproval {
                        request_id: format!("hole-{}", crate::session::store::now_ms()),
                        run_id: run_id.to_string(),
                        script: body.to_string(),
                        plan: serde_json::to_value(&c.plan).unwrap_or_default(),
                        defaults: moc_protocol::WorkflowApprovalDefaults {
                            role_models: Default::default(),
                            model_ref_map: Default::default(),
                        },
                        args: Value::Null,
                        max_concurrency: 0,
                        kind: moc_protocol::WorkflowApprovalKind::HoleFill,
                        new_commands: Some(new_commands),
                    };
                    let rx = manager.request_approval(approval, &ctx.request.updates());
                    let decision = rx.await.map_err(|_| ToolError::new("E_CANCELLED", "approval channel closed"))?;
                    match decision {
                        moc_protocol::WorkflowDecision::Approve { .. } => {}
                        moc_protocol::WorkflowDecision::Reject { feedback } => {
                            return Err(ToolError::new("E_REJECTED", feedback.unwrap_or_else(|| "hole fill rejected".into())));
                        }
                    }
                }
                // 交付降级体（guest evalFill 经 eval 运行）：包成函数表达式。
                let filled = format!("(async()=>{{\n{}\n}})", c.lowered);
                manager.fill_hole(run_id, hole, filled).await.map_err(tool_err)?;
                wf_outcome(json!({"filled": hole}))
            }
        }
    }
}

impl SaveWorkflowTool {
    async fn run_save(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        activated(ctx).await?;
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("E_ARGS", "save_workflow requires `name`"))?;
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or_default();
        let scope = args.get("scope").and_then(|v| v.as_str()).unwrap_or("project");
        let source = match (args.get("run_id").and_then(|v| v.as_str()), args.get("path").and_then(|v| v.as_str())) {
            (Some(run_id), _) => {
                let manager = activated(ctx).await?;
                std::fs::read_to_string(manager.run_dir(run_id).join("script.ts"))
                    .map_err(|e| ToolError::new("E_IO", format!("draft unreadable: {e}")))?
            }
            (None, Some(path)) => read_script(ctx, path)?,
            _ => return Err(ToolError::new("E_ARGS", "save_workflow requires `run_id` or `path`")),
        };
        let dir = match scope {
            "global" => ctx.config.home.join("workflows"),
            _ => ctx.cwd.join(".moc").join("workflows"),
        };
        std::fs::create_dir_all(&dir).map_err(|e| ToolError::new("E_IO", e.to_string()))?;
        std::fs::write(dir.join(format!("{name}.ts")), &source)
            .map_err(|e| ToolError::new("E_IO", e.to_string()))?;
        let meta = json!({"name": name, "description": description, "args": args.get("args").cloned().unwrap_or(json!([]))});
        std::fs::write(dir.join(format!("{name}.json")), serde_json::to_vec_pretty(&meta).unwrap_or_default())
            .map_err(|e| ToolError::new("E_IO", e.to_string()))?;
        wf_outcome(json!({"saved": name, "scope": scope, "path": dir.join(format!("{name}.ts")).display().to_string()}))
    }
}

impl ListSavedTool {
    async fn run_list_saved(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutcome, ToolError> {
        activated(ctx).await?;
        let mut out = Vec::new();
        for (scope, dir) in [
            ("project", ctx.cwd.join(".moc").join("workflows")),
            ("global", ctx.config.home.join("workflows")),
        ] {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let path = e.path();
                    if path.extension().is_some_and(|x| x == "json")
                        && let Ok(meta) = std::fs::read_to_string(&path)
                        && let Ok(v) = serde_json::from_str::<Value>(&meta)
                    {
                        out.push(json!({
                            "name": v["name"], "description": v["description"],
                            "scope": scope,
                            "args": v["args"],
                        }));
                    }
                }
            }
        }
        wf_outcome(json!({"workflows": out}))
    }
}

impl WorkflowStatusTool {
    async fn run_status(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        let manager = activated(ctx).await?;
        if let Some(run_id) = args.get("run_id").and_then(|v| v.as_str()) {
            let meta = manager.meta(run_id).map_err(tool_err)?;
            let handle = manager.running(run_id);
            let view = handle.as_ref().map(|h| h.view.lock());
            let snapshot = super::snapshot_of(&meta, view.as_deref(), &manager.run_dir(run_id));
            wf_outcome(serde_json::to_value(snapshot).unwrap_or_default())
        } else {
            wf_outcome(json!({"runs": serde_json::to_value(manager.list()).unwrap_or_default()}))
        }
    }
}

impl EvalSnippetTool {
    async fn run_eval(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutcome, ToolError> {
        activated(ctx).await?;
        skill_gate(ctx)?;
        let code = match (args.get("code").and_then(|v| v.as_str()), args.get("path").and_then(|v| v.as_str())) {
            (Some(code), _) => code.to_string(),
            (None, Some(path)) => read_script(ctx, path)?,
            _ => return Err(ToolError::new("E_ARGS", "eval_workflow_snippet requires `code` or `path`")),
        };
        let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(60).min(600);
        let compiled = crate::workflow::compile::compile(&code, &crate::workflow::compile::CompileOptions::default())
            .map_err(|d| ToolError::new("E_WORKFLOW_COMPILE", format!("snippet diagnostics: {}", d.iter().map(|x| x.message.clone()).collect::<Vec<_>>().join("; "))))?;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            eval_loop(compiled, ctx),
        )
        .await;
        match outcome {
            Ok(Ok(v)) => wf_outcome(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ToolError::new("E_TIMEOUT", format!("snippet exceeded {timeout_secs}s"))),
        }
    }
}

/// eval 同步求值：同一 worker 进程 + 内联驱动——world/files/git 即时执行，
/// ask/agent/hole 直接失败（eval 没有 actor 驱动），channel/future/log 可用。
async fn eval_loop(
    compiled: crate::workflow::compile::CompiledWorkflow,
    ctx: &ToolCtx,
) -> Result<Value, ToolError> {
    use moc_ptc_protocol::wire::{
        GuestState, HostCompletion, WorkflowGuestMessage, WorkflowHostMessage, WorkflowOperation,
        WorkflowStartRequest, WorkflowRunInfo as WireRunInfo,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (program, args) = super::worker_command(ctx.core.as_ref())
        .map_err(|e| ToolError::new("E_HOST", e.to_string()))?;
    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::new("E_HOST", format!("worker 启动失败：{e}")))?;
    let mut stdin = child.stdin.take().ok_or_else(|| ToolError::new("E_HOST", "stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| ToolError::new("E_HOST", "stdout"))?;
    let ptc = ctx.config.tools.ptc.clone();
    async fn send(m: &WorkflowHostMessage, stdin: &mut tokio::process::ChildStdin) -> bool {
        let mut f = serde_json::to_vec(m).unwrap_or_default();
        f.push(b'\n');
        stdin.write_all(&f).await.is_ok() && stdin.flush().await.is_ok()
    }
    let policy = ctx.sandbox_policy();
    let cap = ctx.config.workflow.file_read_bytes;
    let cwd = ctx.cwd.clone();
    let cfg = (
        ptc.memory_limit_bytes,
        ptc.max_output_bytes,
        ptc.max_result_bytes,
        ptc.max_pending_calls,
        ptc.max_total_calls,
    );
    let start = WorkflowHostMessage::Start {
        request: WorkflowStartRequest {
            code: compiled.lowered.clone(),
            args: Value::Null,
            run: WireRunInfo { id: "eval".into(), resumed_from: None },
            memory_limit_bytes: cfg.0,
            max_output_bytes: cfg.1,
            max_message_bytes: cfg.2,
            max_pending_calls: cfg.3,
            max_total_calls: cfg.4,
        },
    };
    if !send(&start, &mut stdin).await {
        return Err(ToolError::new("E_HOST", "worker 写 Start 失败"));
    }
    let mut lines = BufReader::new(stdout).lines();
    let mut logs: Vec<String> = Vec::new();
    let result = loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| ToolError::new("E_HOST", e.to_string()))?
            .ok_or_else(|| ToolError::new("E_HOST", "worker 提前退出"))?;
        let Ok(WorkflowGuestMessage::Step { step }) = serde_json::from_str::<WorkflowGuestMessage>(&line)
        else {
            continue;
        };
        let mut completions = Vec::new();
        for event in &step.events {
            if let moc_ptc_protocol::wire::WorkflowEvent::Log { message, .. } = event {
                logs.push(message.clone());
            }
        }
        for req in step.requests {
            let outcome = match &req.operation {
                WorkflowOperation::WorldRead { kind, arguments } => {
                    let op = format!("{kind:?}").to_lowercase();
                    super::driver::world_read_impl(&cwd, cap, &op, &Value::Array(arguments.clone()))
                        .await
                }
                WorkflowOperation::WorldRun { command, args, options } => {
                    super::driver::world_run_impl(
                        &policy,
                        command,
                        args,
                        options.timeout_ms.unwrap_or(300_000),
                        &options.output.clone().unwrap_or(Value::Null),
                    )
                    .await
                }
                WorkflowOperation::Ask { .. } => crate::workflow::engine::DriverOutcome::Failed {
                    code: "E_EVAL".into(),
                    message: "eval snippets cannot create agents or ask".into(),
                },
                WorkflowOperation::Hole { .. } => crate::workflow::engine::DriverOutcome::Failed {
                    code: "E_EVAL".into(),
                    message: "eval snippets cannot open holes".into(),
                },
                WorkflowOperation::PublishArtifact { .. } => {
                    crate::workflow::engine::DriverOutcome::Completed(json!({"artifact_id": "eval"}))
                }
            };
            let result = match outcome {
                crate::workflow::engine::DriverOutcome::Completed(v) => Ok(v),
                crate::workflow::engine::DriverOutcome::Failed { code, message } => {
                    Err(moc_ptc_protocol::wire::ScriptError { code, message, result: None })
                }
            };
            completions.push(HostCompletion { id: req.id, result });
        }
        if !completions.is_empty() {
            let complete = WorkflowHostMessage::Complete { completions };
            if !send(&complete, &mut stdin).await {
                return Err(ToolError::new("E_HOST", "worker 写 Complete 失败"));
            }
        }
        match step.state {
            GuestState::Done { value } => break Ok(json!({"status": "done", "value": value, "logs": logs})),
            GuestState::Failed { error } => {
                break Err(ToolError::new(
                    "E_SNIPPET",
                    format!("{}: {}", error.code, error.message),
                )
                .with_hint(logs.join("\n")))
            }
            GuestState::Pending => {}
        }
    };
    let _ = send(&WorkflowHostMessage::Shutdown, &mut stdin).await;
    let _ = child.kill().await;
    result
}

// —— 共用辅助 ——

/// 编译 → 诊断则工具错误 → 审批（阻塞）→ 后台启动。
async fn launch_flow(
    ctx: &ToolCtx,
    manager: &Arc<super::WorkflowManager>,
    source: String,
    args: Value,
    mode: &str,
    resumed_from: Option<String>,
) -> Result<ToolOutcome, ToolError> {
    let run_id = manager.alloc_run_id();
    let compiled = match manager.compile_script(&run_id, &source) {
        Ok(c) => c,
        Err(diags) => {
            return Err(ToolError::new(
                "E_WORKFLOW_COMPILE",
                format!("script diagnostics:\n{}", diags.iter().map(|d| format!("L{}:C{} {} {}", d.line, d.col, d.code, d.message)).collect::<Vec<_>>().join("\n")),
            ));
        }
    };
    let session_model = ctx.state.lock().await.model.clone();
    let cfg = ctx.config.workflow.clone();
    let max_concurrency = args
        .get("max_concurrency")
        .and_then(|v| v.as_u64())
        .map_or(0, |n| (n as usize).clamp(1, cfg.max_concurrency_ceiling));
    // 审批默认：字面量模型否则当前会话模型。
    let mut role_models = std::collections::BTreeMap::new();
    for role in &compiled.plan.roles {
        let default = match &role.model_source {
            crate::workflow::compile::ModelSource::Literal { model } => model.clone(),
            _ => session_model.clone(),
        };
        role_models.insert(role.role_id.clone(), default);
    }
    let approval = moc_protocol::WorkflowApproval {
        request_id: format!("wf-{}", crate::session::store::now_ms()),
        run_id: run_id.clone(),
        script: source.clone(),
        plan: serde_json::to_value(&compiled.plan).unwrap_or_default(),
        defaults: moc_protocol::WorkflowApprovalDefaults {
            role_models,
            model_ref_map: Default::default(),
        },
        args: args.get("args").cloned().unwrap_or(Value::Null),
        max_concurrency,
        kind: if mode == "amend" {
            moc_protocol::WorkflowApprovalKind::Amend
        } else {
            moc_protocol::WorkflowApprovalKind::Launch
        },
        new_commands: None,
    };
    let rx = manager.request_approval(approval, &ctx.request.updates());
    let decision = rx
        .await
        .map_err(|_| ToolError::new("E_CANCELLED", "approval channel closed"))?;
    let (role_models, model_ref_map) = match decision {
        moc_protocol::WorkflowDecision::Approve { role_models, model_ref_map } => (
            role_models.unwrap_or_default().into_iter().collect(),
            model_ref_map.unwrap_or_default().into_iter().collect(),
        ),
        moc_protocol::WorkflowDecision::Reject { feedback } => {
            return Err(ToolError::new("E_REJECTED", feedback.unwrap_or_else(|| "workflow rejected".into())));
        }
    };
    let state = ctx.state.lock().await;
    let meta = super::RunMeta {
        run_id: run_id.clone(),
        status: "running".into(),
        stop_reason: None,
        title: state.session.title.clone(),
        plan: serde_json::to_value(&compiled.plan).unwrap_or_default(),
        args: args.get("args").cloned().unwrap_or(Value::Null),
        commands: compiled.plan.commands.iter().map(|c| c.command.clone()).collect(),
        role_models,
        model_ref_map,
        subagent_model: args.get("subagent_model").and_then(|v| v.as_str()).map(str::to_string),
        session_model: Some(session_model),
        resumed_from: resumed_from.clone(),
        mode: mode.into(),
        created_at_ms: crate::session::store::now_ms(),
        finished_at_ms: None,
        result: None,
        error: None,
        reports: Vec::new(),
        artifacts: Vec::new(),
        holes: Vec::new(),
        roles: Vec::new(),
        phases: Vec::new(),
        usage: serde_json::json!({}),
    };
    drop(state);
    let policy = ctx.sandbox_policy();
    let cwd = ctx.cwd.clone();
    let request = ctx.request.clone();
    let updates = ctx.request.updates();
    // run 的取消父级必须挂在 shutdown/会话级：`ctx.cancel` 是请求级 token，
    // create_workflow 所在的 prompt 一返回就会误杀后台 run（与 spawn_agent 同坑）。
    let cancel = ctx.core.shutdown.child_token();
    manager
        .launch(meta, compiled, request, cwd, policy, updates, cancel)
        .await
        .map_err(tool_err)?;
    wf_outcome(json!({
        "run_id": run_id,
        "draft": manager.run_dir(&run_id).join("script.ts").display().to_string(),
        "status": "running",
        "note": "edit the draft and resubmit with `path` to revise",
    }))
}

async fn resume_run(
    ctx: &ToolCtx,
    manager: &Arc<super::WorkflowManager>,
    run_id: &str,
) -> Result<ToolOutcome, ToolError> {
    let dir = manager.run_dir(run_id);
    let meta = super::read_run_meta(&dir).map_err(tool_err)?;
    if meta.status == "running" {
        return Err(ToolError::new("E_STATE", format!("run {run_id} already running")));
    }
    if meta.status == "errored" {
        return Err(ToolError::new("E_STATE", format!("run {run_id} is errored and cannot be resumed")));
    }
    let source = std::fs::read_to_string(dir.join("script.ts"))
        .map_err(|e| ToolError::new("E_IO", format!("draft unreadable: {e}")))?;
    let compiled = crate::workflow::compile::compile(&source, &crate::workflow::compile::CompileOptions::default())
        .map_err(|d| ToolError::new("E_WORKFLOW_COMPILE", format!("script no longer compiles: {}", d.iter().map(|x| x.message.clone()).collect::<Vec<_>>().join("; "))))?;
    let mut meta = meta;
    meta.status = "running".into();
    meta.mode = "resume".into();
    let policy = ctx.sandbox_policy();
    let cancel = ctx.core.shutdown.child_token();
    manager
        .launch(meta, compiled, ctx.request.clone(), ctx.cwd.clone(), policy, ctx.request.updates(), cancel)
        .await
        .map_err(tool_err)?;
    wf_outcome(json!({"run_id": run_id, "status": "running", "resumed": true}))
}

fn read_script(ctx: &ToolCtx, path: &str) -> Result<String, ToolError> {
    let path = crate::tools::resolve_tool_path(&ctx.cwd, path)?;
    std::fs::read_to_string(&path).map_err(|e| ToolError::new("E_IO", format!("script unreadable: {e}")))
}

/// 读取 saved workflow（project → global），并校验/填 args 默认值。
fn load_saved(ctx: &ToolCtx, name: &str) -> Result<String, ToolError> {
    for dir in [
        ctx.cwd.join(".moc").join("workflows"),
        ctx.config.home.join("workflows"),
    ] {
        let script = dir.join(format!("{name}.ts"));
        if script.exists() {
            return std::fs::read_to_string(&script)
                .map_err(|e| ToolError::new("E_IO", e.to_string()));
        }
    }
    Err(ToolError::new("E_NOT_FOUND", format!("saved workflow `{name}` not found")))
}

fn tool_err(e: CoreError) -> ToolError {
    ToolError::new("E_WORKFLOW", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 零泄漏（spec §6 验收）：未激活会话的工具定义 JSON 与「workflow 模块不存在」
    /// 的产物逐字节一致；PTC 签名段同样不含 workflow 工具；`<skills>` 索引未激活
    /// 不含 workflow-authoring。
    #[test]
    fn zero_leak_definitions_and_signatures() {
        let registry = crate::tools::ToolRegistry::new();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let config = crate::config::ResolvedConfig::load(&crate::CoreConfig {
            home: home.path().to_path_buf(),
            cwd: cwd.path().to_path_buf(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::default(),
            ptc_worker: None,
        })
        .unwrap();
        // 未激活 scope：definitions 必须一个 workflow 工具都不含，且与「把 9 个工具
        // 从注册表删掉」的产物逐字节一致。
        let mut scope = crate::tools::ToolScope::top();
        let inactive: Vec<String> = registry
            .definitions(
                moc_protocol::SessionEngine::Standard,
                false,
                &config,
                scope.clone(),
                &[],
            )
            .iter()
            .map(|d| serde_json::to_string(d).unwrap())
            .collect();
        for tool in WORKFLOW_TOOLS {
            assert!(!inactive.iter().any(|s| s.contains(tool)), "leak: {tool}");
        }
        scope.workflows = true;
        let active_names: Vec<String> = registry
            .definitions(
                moc_protocol::SessionEngine::Standard,
                false,
                &config,
                scope,
                &[],
            )
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for tool in WORKFLOW_TOOLS {
            assert!(active_names.iter().any(|n| n == tool), "missing {tool} when active");
        }
        // 子代理永不给 workflow 工具（即使会话已激活）。
        let sub_scope = crate::tools::ToolScope {
            workflows: false,
            ..crate::tools::ToolScope::top()
        };
        let sub: Vec<String> = registry
            .definitions(moc_protocol::SessionEngine::Standard, false, &config, sub_scope, &[])
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for tool in WORKFLOW_TOOLS {
            assert!(!sub.iter().any(|n| n == tool));
        }
        // PTC 签名段：workflows_active=false 的产物不得含任何 workflow 工具名。
        let catalog = crate::tools::catalog::ToolCatalog::new(
            &registry,
            &moc_mcp::McpSnapshot::default(),
            moc_mcp::McpManager::new(Vec::new(), tokio_util::sync::CancellationToken::new()),
            Vec::new(),
            false,
        )
        .unwrap();
        let text = crate::tools::declarations::render_signatures(
            &catalog,
            moc_protocol::SessionEngine::Ptc,
            false,
            false,
            &config,
        )
        .unwrap_or_default();
        for tool in WORKFLOW_TOOLS {
            assert!(!text.contains(tool), "signature leak: {tool}");
        }
    }

    /// `<skills>` 未激活不含 workflow-authoring；激活后注入。
    #[test]
    fn skills_gate_by_activation() {
        let mut index = crate::skills::SkillIndex::empty();
        assert!(index.get(crate::workflow::skill::SKILL_NAME).is_none());
        let home = tempfile::tempdir().unwrap();
        crate::workflow::skill::inject(&mut index, home.path());
        let meta = index.get(crate::workflow::skill::SKILL_NAME).expect("injected");
        assert!(meta.skill_file().exists(), "materialized SKILL.md must exist");
        assert!(meta.base_dir.join("patterns.md").exists());
    }
}
