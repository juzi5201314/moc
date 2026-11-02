//! workflow driver：把引擎派发映射到真实副作用。
//!
//! - `Ask` → 子代理子会话：guest 声明的完整 `persona.system` 作 `context` 片段
//!   叠在 harness 子代理契约上；首个 ask 以 `task` 发起（`output_schema`
//!   结构化交付），后续 ask 走 `message`。交付不满足 schema → 修复重试有上限
//!   （`[workflow].ask_max_repairs`）。上下文续接按 [`ActorContext`]：Fresh 新
//!   会话；Existing 续本会话；SeedFrom 真 fork——把前驱 actor 子会话的
//!   transcript 逐字截到第 imported_asks 个 ask 结束处，复制成新子会话，
//!   前驱文件保持不动（那是前驱 run 的审计记录）。
//! - `world.run` → 固定 argv 直跑（不经 shell），由会话沙箱以 argv 包裹（审批命令集即授权）；
//! - `world.read` → files.* / git.* 只读实现（git 固定白名单子命令）；
//! - `artifact.file/markdown` → 版本化写 `<run>/artifacts/`。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::agents::SpawnModel;
use crate::session::entries::Entry;
use crate::service::CoreInner;
use crate::workflow::engine::{ActorContext, DriverOutcome, DriverRequest};
use moc_llm::Role;

use super::RunHandle;

/// 引擎派发的执行者；每次派发 `tokio::spawn` 一份。
pub(crate) struct WorkflowDriver {
    pub inner: Arc<CoreInner>,
    pub session_id: String,
    pub run: Arc<RunHandle>,
}

impl WorkflowDriver {
    pub async fn execute(&self, request: &DriverRequest) -> DriverOutcome {
        match request {
            DriverRequest::Ask { .. } => self.ask(request).await,
            DriverRequest::WorldRead { op, args, .. } => self.world_read(op, args).await,
            DriverRequest::WorldRun { .. } => self.world_run(request).await,
            DriverRequest::PublishArtifact { member, spec, .. } => {
                self.publish_artifact(member, spec).await
            }
            DriverRequest::CancelAsk { actor_site_id, actor_ordinal, .. } => {
                self.cancel_ask(actor_site_id, *actor_ordinal).await
            }
        }
    }

    /// ask：会话内首次 → spawn（task=指令），之后 → message+wait；schema 校验带修复重试。
    async fn ask(&self, request: &DriverRequest) -> DriverOutcome {
        let DriverRequest::Ask {
            actor_site_id,
            actor_ordinal,
            instructions,
            schema,
            context,
            persona,
            ..
        } = request
        else {
            unreachable!()
        };
        let actor_key = (actor_site_id.clone(), *actor_ordinal);
        let existing = self
            .run
            .view
            .lock()
            .actor_sessions
            .get(&actor_key)
            .cloned();
        // 模型解析：运行中覆盖 > 审批窗覆盖（已并入 role_models）> persona.model >
        // run.subagent_model > 审批时快照的会话模型。
        let model = self.resolve_actor_model(actor_site_id, persona);
        let child_id = match existing {
            Some(id) => id,
            None => match self.spawn_actor(context, instructions, schema, &model, persona).await {
                Ok(id) => {
                    self.run.engine.lock().actor_session_created(
                        actor_site_id,
                        *actor_ordinal,
                        &id,
                    );
                    self.run
                        .view
                        .lock()
                        .actor_sessions
                        .insert(actor_key.clone(), id.clone());
                    id
                }
                Err(outcome) => return outcome,
            },
        };
        // 会话已存在 → message + wait；首 ask 已随 spawn 发出时跳过 message。
        let first = matches!(context, ActorContext::Fresh | ActorContext::SeedFrom { .. })
            && self
                .run
                .view
                .lock()
                .actor_first_sent
                .insert(actor_key.clone());
        let max_repairs = self.inner.config.read().workflow.ask_max_repairs;
        // 用量差分：本 ask（含 schema 修复重试）在子会话累计用量上的增量；费用按
        // actor 解析模型的 per-million 定价折算。
        let pricing = self
            .inner
            .project_config(std::path::Path::new(&self.run.cwd))
            .ok()
            .and_then(|config| config.resolve_model(&model).ok())
            .and_then(|m| m.pricing);
        let usage_before = self.child_usage(&child_id).await;
        let mut text = if first { None } else { Some(instructions.clone()) };
        for attempt in 0..=max_repairs {
            let send_text = text
                .take()
                .or_else(|| (attempt > 0 || !first).then(|| instructions.clone()));
            if let Some(text) = send_text
                && let Err(e) = crate::agents::message(
                    Arc::clone(&self.inner),
                    self.run.request.clone(),
                    self.session_id.clone(),
                    child_id.clone(),
                    text,
                )
                .await
            {
                return DriverOutcome::Failed {
                    code: "E_ACTOR".into(),
                    message: format!("actor 消息投递失败：{e}"),
                };
            }
            let result = crate::agents::wait(
                &self.inner,
                &self.session_id,
                Some(vec![child_id.clone()]),
                None,
                None,
                &self.run.cancel,
            )
            .await;
            let Ok(result) = result else {
                return DriverOutcome::Failed {
                    code: "E_ACTOR".into(),
                    message: format!("actor 等待失败：{}", result.unwrap_err()),
                };
            };
            let Some(entry) = result.agents.first() else {
                return DriverOutcome::Failed {
                    code: "E_ACTOR".into(),
                    message: "actor 会话不可达".into(),
                };
            };
            // 交付值：schema 化的取 structured_result（Null 视为未交付）；自由
            // 文本交付取 yield 报告。
            let value = entry
                .structured_result
                .clone()
                .filter(|v| !v.is_null())
                .or_else(|| entry.message.as_ref().map(|m| Value::String(m.text())));
            let usage = usage_diff(&self.child_usage(&child_id).await, &usage_before, pricing.as_ref());
            match (&schema, &value) {
                (Some(schema), Some(v)) if schema_ok(schema, v) => {
                    return DriverOutcome::Completed(
                        json!({"value": v, "usage": usage}),
                    );
                }
                (Some(schema), _) => {
                    if attempt == max_repairs {
                        return DriverOutcome::Failed {
                            code: "E_SCHEMA".into(),
                            message: "ask 交付不满足 output_schema（修复重试耗尽）".into(),
                        };
                    }
                    text = Some(format!(
                        "Your previous reply did not satisfy the required output schema. Respond ONLY with a JSON value matching:\n{}",
                        serde_json::to_string_pretty(schema).unwrap_or_default()
                    ));
                }
                (None, _) => {
                    return DriverOutcome::Completed(json!({
                        "value": value.unwrap_or(Value::Null),
                        "usage": usage,
                    }));
                }
            }
        }
        unreachable!()
    }

    /// 新 actor：spawn 子会话（persona.system 全文作 context 注入 harness 子代理契约）。
    async fn spawn_actor(
        &self,
        context: &ActorContext,
        instructions: &str,
        schema: &Option<Value>,
        model: &str,
        persona: &Value,
    ) -> Result<String, DriverOutcome> {
        let meta = self.run.meta.lock().clone();
        // SeedFrom：前驱 actor 子会话 transcript 截到第 imported_asks 个 ask 结束处，
        // 逐字 fork 进新子会话（不碰前驱文件——那是前驱 run 的审计记录）。
        let seed_entries = match context {
            ActorContext::SeedFrom { child_session_id, imported_asks, .. } => {
                let source = match self.inner.sessions.get(child_session_id).await {
                    Ok(record) => record,
                    Err(e) => {
                        return Err(DriverOutcome::Failed {
                            code: "E_ACTOR".into(),
                            message: format!("SeedFrom 源子会话不可读：{e}"),
                        })
                    }
                };
                let st = source.state.lock().await;
                transcript_prefix(&st.entries, *imported_asks)
            }
            _ => Vec::new(),
        };
        // persona.system 用 guest 声明的全文（Ask 请求携带），不再从
        // plan.roles 摘 excerpt——那是 200 字截断摘要且可能取到别的 role。
        let context_text: Option<String> = persona
            .get("system")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let spec = crate::agents::SpawnSpec {
            task: instructions.to_string(),
            title: meta.title.is_empty().then_some("workflow actor").map(str::to_string),
            model: SpawnModel::Exact(model.to_string()),
            preset: None,
            context: context_text,
            output_schema: schema.clone(),
            seed_entries,
        };
        drop(meta);
        match crate::agents::spawn(
            Arc::clone(&self.inner),
            self.run.request.clone(),
            self.session_id.clone(),
            spec,
        )
        .await
        {
            Ok(result) => Ok(result.agent.id),
            Err(e) => Err(DriverOutcome::Failed {
                code: "E_ACTOR".into(),
                message: format!("actor spawn 失败：{e}"),
            }),
        }
    }

    /// 运行中覆盖 > 审批覆盖（role_models）> persona.model > role 声明 >
    /// subagent_model > 会话模型快照。
    fn resolve_actor_model(&self, role_id: &str, persona: &Value) -> String {
        let meta = self.run.meta.lock();
        if let Some(m) = meta.role_models.get(role_id) {
            return m.clone();
        }
        // persona.model：guest 声明优先（字面量或路由后的解析值）。
        if let Some(m) = persona.get("model").and_then(|v| v.as_str())
            && !m.is_empty()
        {
            return m.to_string();
        }
        // 静态回退：plan roles 里的字面量解析值。
        if let Some(roles) = meta.plan.get("roles").and_then(|r| r.as_array()) {
            for role in roles {
                if role.get("role_id").and_then(|v| v.as_str()) == Some(role_id) {
                    match role.get("model_source") {
                        Some(Value::String(m)) if !m.is_empty() => return m.clone(),
                        _ => {}
                    }
                }
            }
        }
        if let Some(m) = &meta.subagent_model {
            return m.clone();
        }
        meta.session_model.clone().unwrap_or_else(|| "default".into())
    }

    /// `world.run`：固定 argv 经 sandbox wrap 直跑（不经 shell）；命令集审批时已授权。
    async fn world_run(&self, request: &DriverRequest) -> DriverOutcome {
        let DriverRequest::WorldRun { command, args, timeout_ms, output, .. } = request else {
            unreachable!()
        };
        let policy = self.run.policy.lock().clone();
        let Some(policy) = policy else {
            return DriverOutcome::Failed {
                code: "E_SANDBOX".into(),
                message: "world.run 无沙箱策略快照".into(),
            };
        };
        world_run_impl(&policy, command, args, *timeout_ms, output).await
    }

    /// files.* / git.* 只读实现。
    async fn world_read(&self, op: &str, args: &Value) -> DriverOutcome {
        let cap = self.inner.config.read().workflow.file_read_bytes;
        world_read_impl(&self.run.cwd, cap, op, args).await
    }

    /// artifact.file/markdown → 版本化写 `<run>/artifacts/`。
    async fn publish_artifact(&self, member: &str, spec: &Value) -> DriverOutcome {
        let args = spec.as_array().cloned().unwrap_or_default();
        let Some(id) = args.first().and_then(|a| a.as_str()).map(str::to_string) else {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: format!("artifact.{member} 缺 id"),
            };
        };
        let dir = self.run.dir.join("artifacts").join(&id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return DriverOutcome::Failed { code: "E_ARTIFACT".into(), message: e.to_string() };
        }
        let version = std::fs::read_dir(&dir)
            .map(|rd| rd.filter_map(|e| e.ok()).count() as u32 + 1)
            .unwrap_or(1);
        let (name, body) = match member {
            "file" => {
                let path = args.get(1).and_then(|a| a.as_str()).unwrap_or("out");
                (path.to_string(), args.get(2).cloned().unwrap_or(Value::Null))
            }
            _ => (format!("v{version}.md"), args.get(1).cloned().unwrap_or(Value::Null)),
        };
        let content = match &body {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        if let Err(e) = std::fs::write(dir.join(format!("v{version}_{name}")), &content) {
            return DriverOutcome::Failed { code: "E_ARTIFACT".into(), message: e.to_string() };
        }
        let mut meta = self.run.meta.lock();
        meta.artifacts.push(json!({
            "artifact_id": id,
            "version": version,
            "member": member,
        }));
        DriverOutcome::Completed(json!({"artifact_id": id, "version": version}))
    }

    /// 子会话累计用量快照（模型无关合计；会话读不到时回零）。
    /// 子会话的累计用量：`state.usage` 只是最后一次响应，记账要按 model_usage
    /// 累计表求和（与 `session/usage` 同口径）。
    async fn child_usage(&self, child_id: &str) -> moc_llm::Usage {
        match self.inner.sessions.get(child_id).await {
            Ok(record) => {
                let state = record.state.lock().await;
                let mut total = moc_llm::Usage::default();
                for usage in state.model_usage.values() {
                    crate::session::state::add_usage(&mut total, usage.usage);
                }
                total
            }
            Err(_) => moc_llm::Usage::default(),
        }
    }

    /// 取消 actor 上的在飞 ask（停子会话当前轮）。
    async fn cancel_ask(&self, actor_site_id: &str, actor_ordinal: u32) -> DriverOutcome {
        let key = (actor_site_id.to_string(), actor_ordinal);
        let child = self.run.view.lock().actor_sessions.get(&key).cloned();
        if let Some(child) = child {
            let _ = crate::agents::stop(&self.inner, self.session_id.clone(), child, None).await;
        }
        DriverOutcome::Completed(json!({"cancelled": true}))
    }
}

/// `world.run` 的共享实现：eval 与 run driver 同一份（策略快照 + argv + 超时 + 输出视图）。
pub(crate) async fn world_run_impl(
    policy: &crate::sandbox::SandboxPolicy,
    command: &str,
    args: &[String],
    timeout_ms: u64,
    output: &Value,
) -> DriverOutcome {
    let wrapped = match crate::sandbox::wrap_command_argv(
        command,
        args,
        &policy.workspace,
        policy,
    ) {
        Ok(w) => w,
        Err(e) => {
            return DriverOutcome::Failed { code: "E_SANDBOX".into(), message: e.to_string() };
        }
    };
    let spawn = tokio::process::Command::new(&wrapped.program)
        .args(&wrapped.args)
        .envs(wrapped.envs.iter().cloned())
        .current_dir(&policy.workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), spawn).await;
    let output_json = match result {
        Ok(Ok(out)) => json!({
            "exit_code": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }),
        Ok(Err(e)) => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("world.run 启动失败：{e}"),
            };
        }
        Err(_) => {
            return DriverOutcome::Failed {
                code: "E_WORLD_TIMEOUT".into(),
                message: format!("world.run 超过 {timeout_ms}ms"),
            };
        }
    };
    let filtered = match output {
        Value::Null => output_json,
        spec => project_output(&output_json, spec),
    };
    DriverOutcome::Completed(filtered)
}

/// `world.read` 的共享实现（files/git 白名单）。
pub(crate) async fn world_read_impl(
    cwd: &std::path::Path,
    cap: usize,
    op: &str,
    args: &Value,
) -> DriverOutcome {
    match op {
        "files_glob" => {
            let pattern = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            match glob_files(cwd, pattern) {
                Ok(paths) => DriverOutcome::Completed(json!(paths)),
                Err(e) => DriverOutcome::Failed { code: "E_WORLD".into(), message: e },
            }
        }
        "files_read" => {
            let path = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            match read_capped(&cwd.join(path), cap) {
                Ok(text) => DriverOutcome::Completed(json!(text)),
                Err(e) => DriverOutcome::Failed { code: "E_WORLD".into(), message: e },
            }
        }
        "files_grep" => {
            let pattern = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get(1).and_then(|v| v.as_str()).unwrap_or(".");
            match grep_files(&cwd.join(path), pattern) {
                Ok(matches) => DriverOutcome::Completed(json!(matches)),
                Err(e) => DriverOutcome::Failed { code: "E_WORLD".into(), message: e },
            }
        }
        "git_changed_files" | "git_diff" | "git_status" | "git_log" => {
            git_impl(cwd, op, args).await
        }
        _ => DriverOutcome::Failed {
            code: "E_WORLD".into(),
            message: format!("unknown world.read op {op}"),
        },
    }
}

/// git 白名单子命令（固定参数，不经 shell）。
async fn git_impl(cwd: &std::path::Path, op: &str, args: &Value) -> DriverOutcome {
    let argv: Vec<String> = match op {
        "git_changed_files" => vec!["diff".into(), "--name-only".into(), "HEAD".into()],
        "git_diff" => {
            let mut v = vec!["diff".into()];
            if let Some(path) = args.get(0).and_then(|a| a.as_str()) {
                v.push("--".into());
                v.push(path.to_string());
            }
            v
        }
        "git_status" => vec!["status".into(), "--porcelain".into()],
        "git_log" => {
            let n = args.get(0).and_then(|a| a.as_u64()).unwrap_or(10).min(100);
            vec!["log".into(), format!("-{n}"), "--oneline".into()]
        }
        _ => unreachable!(),
    };
    match tokio::process::Command::new("git")
        .args(&argv)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(out) => DriverOutcome::Completed(json!({
            "exit_code": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout),
        })),
        Err(e) => DriverOutcome::Failed {
            code: "E_WORLD".into(),
            message: format!("git 执行失败：{e}"),
        },
    }
}

fn schema_ok(schema: &Value, value: &Value) -> bool {
    jsonschema::validator_for(schema).map(|v| v.is_valid(value)).unwrap_or(false)
}

/// 会话累计用量差分：引擎按 `spent_tokens` 记账，分项与费用进 run 快照。
fn usage_diff(
    after: &moc_llm::Usage,
    before: &moc_llm::Usage,
    pricing: Option<&moc_protocol::methods::ModelPricing>,
) -> Value {
    let input = after.input_tokens.saturating_sub(before.input_tokens);
    let output = after.output_tokens.saturating_sub(before.output_tokens);
    let cached = after
        .cached_input_tokens
        .unwrap_or(0)
        .saturating_sub(before.cached_input_tokens.unwrap_or(0));
    let mut usage = json!({
        "spent_tokens": after.total_tokens().saturating_sub(before.total_tokens()),
        "input_tokens": input,
        "output_tokens": output,
        "cached_input_tokens": cached,
        "reasoning_tokens": after
            .reasoning_tokens
            .unwrap_or(0)
            .saturating_sub(before.reasoning_tokens.unwrap_or(0)),
    });
    if let Some(p) = pricing {
        // input_tokens 已折入缓存命中部分：未命中按 input 价，命中按 cache_read 价
        //（未配置 cache_read 时按 input 价估算）。
        let uncached = input.saturating_sub(cached);
        let cost = (uncached as f64) * p.input / 1e6
            + (cached as f64) * p.cache_read.unwrap_or(p.input) / 1e6
            + (output as f64) * p.output / 1e6;
        usage["cost_usd"] = json!(cost);
    }
    usage
}

/// world.run output 视图：`{"tail":N}` 否则全量。
fn project_output(full: &Value, spec: &Value) -> Value {
    if let Some(tail) = spec.get("tail").and_then(|v| v.as_u64()) {
        let stdout = full["stdout"].as_str().unwrap_or_default();
        let lines: Vec<&str> = stdout.lines().collect();
        let tail = (tail as usize).min(lines.len());
        return json!({
            "exit_code": full["exit_code"],
            "stdout": lines[lines.len() - tail..].join("\n"),
        });
    }
    full.clone()
}

fn glob_files(cwd: &std::path::Path, pattern: &str) -> Result<Vec<String>, String> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| e.to_string())?
        .compile_matcher();
    let mut out = Vec::new();
    for entry in ignore::Walk::new(cwd).filter_map(|e| e.ok()) {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(cwd)
            && glob.is_match(rel)
        {
            out.push(rel.display().to_string());
        }
    }
    Ok(out)
}

fn read_capped(path: &std::path::Path, cap: usize) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&data[..data.len().min(cap)]).to_string())
}

fn grep_files(path: &std::path::Path, pattern: &str) -> Result<Vec<Value>, String> {
    use grep_matcher::Matcher;
    let re = grep_regex::RegexMatcher::new(pattern).map_err(|e| e.to_string())?;
    let paths: Vec<std::path::PathBuf> = if path.is_dir() {
        ignore::Walk::new(path)
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.path().to_path_buf())
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    let mut out = Vec::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line.as_bytes()).unwrap_or(false) {
                out.push(json!({"path": p.display().to_string(), "line": i + 1, "text": line}));
            }
        }
    }
    Ok(out)
}

/// SeedFrom 截断点：跳过 header，取到第 `asks` 个 user 消息所属回合结束
///（即第 asks+1 个 user 消息之前；不足则取全部）。
fn transcript_prefix(entries: &[Entry], asks: u32) -> Vec<Entry> {
    let mut seen = 0u32;
    let mut end = entries.len();
    for (i, entry) in entries.iter().enumerate().skip(1) {
        if let Entry::Message { message, .. } = entry
            && message.role == Role::User
        {
            seen += 1;
            if seen > asks {
                end = i;
                break;
            }
        }
    }
    entries[1..end].to_vec()
}
