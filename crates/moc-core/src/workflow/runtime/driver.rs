//! workflow driver：把引擎派发映射到真实副作用。
//!
//! - `Ask` → 子代理子会话：guest 声明的完整 `persona.system` 作 `context` 片段
//!   叠在 harness 子代理契约上；首个 ask 以 `task` 发起（`output_schema`
//!   结构化交付），后续 ask 走 `message`。交付不满足 schema → 修复重试有上限
//!   （`[workflow].ask_max_repairs`）。上下文续接按 [`ActorContext`]：Fresh 新
//!   会话；Existing 续本会话；SeedFrom 真 fork——把前驱 actor 子会话的
//!   transcript 逐字截到第 user_turns 个 user 消息之前，复制成新子会话，
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
use crate::workflow::facade;
use moc_llm::Role;

use super::RunHandle;

/// wire 枚举 → serde 名（`files_glob`/`git_diff` 等；`{kind:?}` 小写化会把
/// `FilesGlob` 错拼成 `filesglob`，这里统一走 serde 序列化）。
pub(crate) fn serde_op_name<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// actor 模型解析链（facade 顺序）：运行中/审批窗覆盖（role_models，
/// `site#ord` 优先）> persona.model（字面量直接用；{model_id, ref_id}
/// 先查 model_ref_map 重映射）> plan.roles 的 model_source（literal 直接；
/// routed 取首 ref 经 model_ref_map 或 plan.model_refs）> subagent_model >
/// 审批时快照的 session_model。
pub(crate) fn resolve_actor_model(
    meta: &super::RunMeta,
    actor_site_id: &str,
    actor_ordinal: u32,
    persona: &Value,
) -> String {
    if actor_ordinal > 0
        && let Some(m) = meta
            .role_models
            .get(&format!("{actor_site_id}#{actor_ordinal}"))
    {
        return m.clone();
    }
    if let Some(m) = meta.role_models.get(actor_site_id) {
        return m.clone();
    }
    match persona.get("model") {
        Some(Value::String(m)) if !m.is_empty() => return m.clone(),
        Some(model @ Value::Object(_)) => {
            if let Some(m) = model
                .get("ref_id")
                .and_then(|v| v.as_str())
                .and_then(|r| meta.model_ref_map.get(r))
            {
                return m.clone();
            }
            if let Some(m) = model
                .get("model_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                return m.to_string();
            }
        }
        _ => {}
    }
    if let Some(source) = meta
        .plan
        .get("roles")
        .and_then(|r| r.as_array())
        .and_then(|roles| {
            roles.iter().find_map(|r| {
                (r.get("role_id").and_then(|v| v.as_str()) == Some(actor_site_id))
                    .then(|| r.get("model_source").cloned())
                    .flatten()
            })
        })
    {
        match source.get("kind").and_then(|k| k.as_str()) {
            Some("literal") => {
                if let Some(m) = source
                    .get("model")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    return m.to_string();
                }
            }
            Some("routed") => {
                if let Some(first) = source
                    .get("refs")
                    .and_then(|r| r.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    if let Some(m) = meta.model_ref_map.get(first) {
                        return m.clone();
                    }
                    if let Some(m) = meta
                        .plan
                        .get("model_refs")
                        .and_then(|r| r.as_array())
                        .and_then(|refs| {
                            refs.iter().find_map(|r| {
                                (r.get("ref_id").and_then(|v| v.as_str()) == Some(first))
                                    .then(|| r.get("model_id").and_then(|v| v.as_str()))
                                    .flatten()
                            })
                        })
                        .filter(|s| !s.is_empty())
                    {
                        return m.to_string();
                    }
                }
            }
            _ => {}
        }
    }
    meta.subagent_model
        .clone()
        .or_else(|| meta.session_model.clone())
        .unwrap_or_else(|| "default".into())
}

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
        let model = {
            let meta = self.run.meta.lock();
            resolve_actor_model(&meta, actor_site_id, *actor_ordinal, persona)
        };
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
            // 交付统计子会话 transcript 的 User 消息数（SeedFrom 截断点；
            // schema 修复重试也占 user 位）。
            let user_turns = self.child_user_turns(&child_id).await;
            match (&schema, &value) {
                (Some(schema), Some(v)) if schema_ok(schema, v) => {
                    return DriverOutcome::Completed(
                        json!({"value": v, "usage": usage, "user_turns": user_turns}),
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
                        "user_turns": user_turns,
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
        // SeedFrom：前驱 actor 子会话 transcript 截到第 user_turns 个 user 消息之前，
        // 逐字 fork 进新子会话（不碰前驱文件——那是前驱 run 的审计记录）。
        let seed_entries = match context {
            ActorContext::SeedFrom { child_session_id, user_turns, .. } => {
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
                transcript_prefix(&st.log_entries(), *user_turns)
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
            title: persona
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| Some("workflow actor".into())),
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
    /// wire 参数 `[id, path|content, opts]`；meta.artifacts 条目与
    /// `WorkflowEvent::Artifact` 载荷同形。
    async fn publish_artifact(&self, member: &str, spec: &Value) -> DriverOutcome {
        // 版本号分配 → 写文件 → push 条目串行化（函数体内无 await，
        // 并发同 id publish 不会算出同一版本）。
        let _publish_guard = self.run.publish_lock.lock();
        let args = spec.as_array().cloned().unwrap_or_default();
        let Some(id) = args.first().and_then(|a| a.as_str()).map(str::to_string) else {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: format!("artifact.{member} 缺 id"),
            };
        };
        // id 参与拼路径：运行期也按编译期字符集校验。
        if !crate::workflow::compile::valid_artifact_id(&id) {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: format!("artifact id \"{id}\" must match [A-Za-z0-9_.-]{{1,64}}"),
            };
        }
        let opts = args.get(2).cloned().unwrap_or(Value::Null);
        let opt_str = |key: &str| opts.get(key).and_then(|v| v.as_str()).map(String::from);
        let title = opt_str("title");
        let description = opt_str("description");
        if title.as_deref().is_some_and(|t| t.chars().count() > facade::ARTIFACT_MAX_TITLE_LEN)
            || description.as_deref().is_some_and(|d| d.chars().count() > facade::ARTIFACT_MAX_DESCRIPTION_LEN)
        {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: "artifact title/description exceeds the cap".into(),
            };
        }
        // primary：本 run 已有其他 id 为 primary → 拒；同 id 一旦 primary 后续版本保持。
        let (primary, version) = {
            let meta = self.run.meta.lock();
            let prior: Vec<&Value> = meta
                .artifacts
                .iter()
                .filter(|a| a.get("artifact_id").and_then(|v| v.as_str()) == Some(id.as_str()))
                .collect();
            let was_primary = prior
                .iter()
                .any(|a| a.get("primary").and_then(|v| v.as_bool()).unwrap_or(false));
            let requested = opts.get("primary").and_then(|v| v.as_bool()).unwrap_or(false);
            if requested
                && !was_primary
                && meta.artifacts.iter().any(|a| {
                    a.get("artifact_id").and_then(|v| v.as_str()) != Some(id.as_str())
                        && a.get("primary").and_then(|v| v.as_bool()).unwrap_or(false)
                })
            {
                return DriverOutcome::Failed {
                    code: "E_ARTIFACT".into(),
                    message: "another artifact is already primary in this run".into(),
                };
            }
            (was_primary || requested, prior.iter().filter(|a| a.get("version").is_some()).count() as u32 + 1)
        };
        if version as usize > facade::ARTIFACT_MAX_VERSIONS {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: format!("artifact \"{id}\" exceeds {} versions", facade::ARTIFACT_MAX_VERSIONS),
            };
        }
        let dir = self.run.dir.join("artifacts").join(&id);
        let (stored, content_type) = match member {
            "file" => {
                let Some(path_arg) = args.get(1).and_then(|a| a.as_str()) else {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: "artifact.file 缺 path".into(),
                    };
                };
                let source = match workspace_file(&self.run.cwd, path_arg) {
                    Ok(p) => p,
                    Err(o) => return o,
                };
                let meta_fs = match std::fs::metadata(&source) {
                    Ok(m) if m.is_file() => m,
                    _ => {
                        return DriverOutcome::Failed {
                            code: "E_ARTIFACT".into(),
                            message: format!("{path_arg} is not a regular file"),
                        };
                    }
                };
                if meta_fs.len() > facade::ARTIFACT_MAX_FILE_BYTES {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: format!("file exceeds {} bytes", facade::ARTIFACT_MAX_FILE_BYTES),
                    };
                }
                let basename = source
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                let stored = format!("v{version}_{basename}");
                if let Err(e) = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::copy(&source, dir.join(&stored)).map(|_| ()))
                {
                    return DriverOutcome::Failed { code: "E_ARTIFACT".into(), message: e.to_string() };
                }
                let content_type = opt_str("contentType")
                    .unwrap_or_else(|| content_type_of(&source).to_string());
                (stored, Some(content_type))
            }
            _ => {
                let Some(content) = args.get(1).and_then(|a| a.as_str()) else {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: "artifact.markdown 缺 content 字符串".into(),
                    };
                };
                if content.len() > facade::ARTIFACT_MAX_MARKDOWN_BYTES {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: format!("markdown exceeds {} bytes", facade::ARTIFACT_MAX_MARKDOWN_BYTES),
                    };
                }
                let stored = format!("v{version}_content.md");
                if let Err(e) = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(dir.join(&stored), content))
                {
                    return DriverOutcome::Failed { code: "E_ARTIFACT".into(), message: e.to_string() };
                }
                (stored, opt_str("contentType").or_else(|| Some("text/markdown".into())))
            }
        };
        let mut meta = self.run.meta.lock();
        let mut entry = json!({
            "artifact_id": id,
            "member": member,
            "version": version,
            "file": stored,
        });
        if let Some(ct) = content_type {
            entry["content_type"] = json!(ct);
        }
        if let Some(t) = title {
            entry["title"] = json!(t);
        }
        if let Some(d) = description {
            entry["description"] = json!(d);
        }
        if primary {
            entry["primary"] = json!(true);
        }
        meta.artifacts.push(entry);
        DriverOutcome::Completed(json!({"id": id, "version": version}))
    }

    /// 子会话 transcript 的 User 消息数（ask 交付的 user_turns）。
    async fn child_user_turns(&self, child_id: &str) -> u32 {
        match self.inner.sessions.get(child_id).await {
            Ok(record) => {
                let state = record.state.lock().await;
                state
                    .entries
                    .iter()
                    .filter(|e| e.message_entry().is_some_and(|entry| entry.message.role == Role::User))
                    .count() as u32
            }
            Err(_) => 0,
        }
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
    run_capped(
        &wrapped.program,
        &wrapped.args,
        &wrapped.envs,
        &policy.workspace,
        timeout_ms,
        output,
        facade::WORLD_RUN_STREAM_MAX_BYTES,
    )
    .await
}

/// world.run 进程执行：固定 argv 直跑。cap 注入供测试（生产路径 = facade 上限）。
/// 超时覆盖「等待退出 + 两路流读完」整体：后台孙进程持有管道也不会挂死；
/// 单流超限立即 kill 子进程。
async fn run_capped(
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
    envs: &[(std::ffi::OsString, std::ffi::OsString)],
    cwd: &std::path::Path,
    timeout_ms: u64,
    output: &Value,
    cap: usize,
) -> DriverOutcome {
    // opts.output："full"（默认，单流 cap 封顶）/ "discard" / {tail:n}（每流保留尾 n 字节）。
    let mode = OutputMode::of(output);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .envs(envs.iter().cloned())
        .current_dir(cwd)
        .kill_on_drop(true);
    if matches!(mode, OutputMode::Discard) {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    } else {
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("world.run 启动失败：{e}"),
            };
        }
    };
    let read_stream = |stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
                       mode: OutputMode| {
        tokio::spawn(async move { capture_stream(stream, mode, cap).await })
    };
    let mut out_task = read_stream(
        child.stdout.take().map(|s| {
            Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>
        }),
        mode,
    );
    let mut err_task = read_stream(
        child.stderr.take().map(|s| {
            Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>
        }),
        mode,
    );
    let mut status = None;
    let mut out_res = None;
    let mut err_res = None;
    let collect = async {
        loop {
            if status.is_some() && out_res.is_some() && err_res.is_some() {
                break;
            }
            tokio::select! {
                s = child.wait(), if status.is_none() => status = Some(s),
                r = &mut out_task, if out_res.is_none() => out_res = Some(r),
                r = &mut err_task, if err_res.is_none() => err_res = Some(r),
            }
            // 任一流超限 → 立即 kill：管道写端不会阻塞，其余读取收尾；
            // 退出状态仍由 wait 臂补取（kill 不保证已回收）。
            let overflowed = matches!(out_res, Some(Ok(Err(_))))
                || matches!(err_res, Some(Ok(Err(_))));
            if overflowed && status.is_none() {
                let _ = child.start_kill();
            }
        }
        (status, out_res, err_res)
    };
    let (status, out_res, err_res) = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        collect,
    )
    .await
    {
        Ok(t) => t,
        Err(_) => {
            let _ = child.start_kill();
            return DriverOutcome::Failed {
                code: "E_WORLD_TIMEOUT".into(),
                message: format!("world.run 超过 {timeout_ms}ms"),
            };
        }
    };
    let status = match status {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("world.run 等待失败：{e}"),
            };
        }
        None => unreachable!("collect 循环退出时三项必齐"),
    };
    let (stdout, stderr) = match (out_res, err_res) {
        (Some(Ok(Ok(o))), Some(Ok(Ok(e)))) => (o, e),
        _ => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("world.run 输出流超过 {cap} 字节"),
            };
        }
    };
    DriverOutcome::Completed(json!({
        "exitCode": status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    }))
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
            match workspace_file(cwd, path).and_then(|p| read_capped(&p, cap).map_err(fail_world)) {
                Ok(text) => DriverOutcome::Completed(json!(text)),
                Err(o) => o,
            }
        }
        "files_grep" => {
            // 第二参数是路径 glob 过滤（"*.ts"、"src/**"），不是路径。
            let pattern = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let glob = args.get(1).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            match grep_files(cwd, pattern, glob) {
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

fn fail_world(message: String) -> DriverOutcome {
    DriverOutcome::Failed { code: "E_WORLD".into(), message }
}

/// 工作区路径围栏：一律工作区相对；逃逸/绝对路径出界 → Failed。
fn workspace_file(
    cwd: &std::path::Path,
    value: &str,
) -> Result<std::path::PathBuf, DriverOutcome> {
    let resolved = crate::tools::resolve_tool_path(cwd, value)
        .map_err(|e| fail_world(e.to_string()))?;
    if crate::tools::workspace_escape(cwd, &resolved).is_some() {
        return Err(fail_world(format!("{value} is outside the workspace")));
    }
    Ok(resolved)
}

/// git base 校验：非空、不含 ".."、不得以 '-' 开头（防 `--output=` 选项注入）。
fn valid_base(base: &str) -> bool {
    !base.is_empty() && !base.contains("..") && !base.starts_with('-')
}

/// 执行 git（固定 argv，不经 shell）；非零退出 → Failed（catchable）。
async fn git_stdout(
    cwd: &std::path::Path,
    argv: &[&str],
) -> Result<String, DriverOutcome> {
    let out = tokio::process::Command::new("git")
        .args(argv)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| DriverOutcome::Failed {
            code: "E_GIT".into(),
            message: format!("git 执行失败：{e}"),
        })?;
    if !out.status.success() {
        return Err(DriverOutcome::Failed {
            code: "E_GIT".into(),
            message: format!(
                "git {} 退出 {}：{}",
                argv.first().unwrap_or(&""),
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// git 白名单子命令（固定参数，不经 shell）。
async fn git_impl(cwd: &std::path::Path, op: &str, args: &Value) -> DriverOutcome {
    let base = args.get(0).and_then(|a| a.as_str());
    if let Some(b) = base
        && !valid_base(b)
    {
        return fail_world(format!("invalid git base \"{b}\""));
    }
    match op {
        "git_changed_files" => {
            let mut files: Vec<String> = Vec::new();
            match base {
                Some(b) => match git_stdout(cwd, &["diff", "--name-only", "--relative", b, "--", "."]).await {
                    Ok(out) => files.extend(out.lines().map(str::to_string)),
                    Err(o) => return o,
                },
                None => {
                    match git_stdout(cwd, &["diff", "--name-only", "--relative", "HEAD", "--", "."]).await {
                        Ok(out) => files.extend(out.lines().map(str::to_string)),
                        Err(o) => return o,
                    }
                    match git_stdout(cwd, &["ls-files", "--others", "--exclude-standard", "--", "."]).await {
                        Ok(out) => files.extend(out.lines().map(str::to_string)),
                        Err(o) => return o,
                    }
                }
            }
            files.retain(|f| !f.is_empty());
            files.sort();
            files.dedup();
            DriverOutcome::Completed(json!(files))
        }
        "git_diff" => {
            let root = match std::fs::canonicalize(cwd) {
                Ok(r) => r,
                Err(e) => return fail_world(e.to_string()),
            };
            let mut argv = vec!["diff", "--no-ext-diff", "--relative", base.unwrap_or("HEAD")];
            let mut rel = None;
            if let Some(p) = args.get(1).and_then(|a| a.as_str()).filter(|s| !s.is_empty()) {
                match workspace_file(cwd, p) {
                    Ok(resolved) => {
                        rel = Some(
                            resolved
                                .strip_prefix(&root)
                                .map(|r| r.display().to_string())
                                .unwrap_or_else(|_| p.to_string()),
                        );
                    }
                    Err(o) => return o,
                }
            }
            if let Some(rel) = &rel {
                argv.push("--");
                argv.push(rel);
            }
            match git_stdout(cwd, &argv).await {
                Ok(out) if out.len() > facade::GIT_DIFF_MAX_BYTES => fail_world(format!(
                    "git diff exceeds {} bytes",
                    facade::GIT_DIFF_MAX_BYTES
                )),
                Ok(out) => DriverOutcome::Completed(json!(out)),
                Err(o) => o,
            }
        }
        "git_status" => {
            let status = match git_stdout(cwd, &["status", "--porcelain=v1", "-b", "--", "."]).await {
                Ok(s) => s,
                Err(o) => return o,
            };
            let prefix = match git_stdout(cwd, &["rev-parse", "--show-prefix"]).await {
                Ok(p) => p.trim_end().to_string(),
                Err(o) => return o,
            };
            DriverOutcome::Completed(parse_porcelain(&status, &prefix))
        }
        "git_log" => {
            let n = args.get(0).and_then(|a| a.as_u64()).unwrap_or(facade::GIT_LOG_DEFAULT as u64);
            if n > facade::GIT_LOG_MAX as u64 {
                return fail_world(format!("git log count exceeds {}", facade::GIT_LOG_MAX));
            }
            match git_stdout(cwd, &["log", "-n", &n.to_string(), "--format=%H%x1f%s%x1f%an%x1f%aI"]).await {
                Ok(out) => {
                    let commits: Vec<Value> = out
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| {
                            let mut parts = l.split('\x1f');
                            json!({
                                "hash": parts.next().unwrap_or_default(),
                                "subject": parts.next().unwrap_or_default(),
                                "author": parts.next().unwrap_or_default(),
                                "date": parts.next().unwrap_or_default(),
                            })
                        })
                        .collect();
                    DriverOutcome::Completed(json!(commits))
                }
                Err(o) => o,
            }
        }
        _ => unreachable!(),
    }
}

/// `git status --porcelain=v1 -b` 输出 → facade GitStatus。
/// porcelain 路径是仓库根相对，`prefix`（rev-parse --show-prefix）去前缀成工作区相对。
fn parse_porcelain(text: &str, prefix: &str) -> Value {
    let mut branch = None;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    for line in text.lines() {
        if let Some(b) = line.strip_prefix("## ") {
            let name = b.split("...").next().unwrap_or(b).trim();
            // 未生成分支（尚无提交）仍是分支；仅 detached（HEAD 行）缺省。
            let name = name.strip_prefix("No commits yet on ").unwrap_or(name);
            if !name.is_empty() && !name.starts_with("HEAD") {
                branch = Some(name.to_string());
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let (x, y) = (line.as_bytes()[0], line.as_bytes()[1]);
        let raw = &line[3..];
        // rename "a -> b" 取 b；路径去工作区前缀，不在工作区内的跳过。
        let path = raw.rsplit(" -> ").next().unwrap_or(raw).trim_matches('"');
        let Some(path) = path.strip_prefix(prefix).or(prefix.is_empty().then_some(path)) else {
            continue;
        };
        let path = path.trim_matches('"').to_string();
        if x == b'?' && y == b'?' {
            untracked.push(json!(path));
        } else {
            if x != b' ' && x != b'?' {
                staged.push(json!(path));
            }
            if y != b' ' && y != b'?' {
                unstaged.push(json!(path));
            }
        }
    }
    let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();
    let mut status = json!({
        "clean": clean,
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
    });
    if let Some(b) = branch {
        status["branch"] = json!(b);
    }
    status
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

/// world.run 输出模式（facade opts.output）。
#[derive(Debug, Clone, Copy)]
enum OutputMode {
    /// 默认：全量捕获，单流超 WORLD_RUN_STREAM_MAX_BYTES 拒绝。
    Full,
    /// stdout/stderr 丢弃。
    Discard,
    /// 每流只保留尾 n 字节。
    Tail(usize),
}

impl OutputMode {
    fn of(output: &Value) -> Self {
        match output {
            Value::String(s) if s == "discard" => Self::Discard,
            Value::Object(_) => output
                .get("tail")
                .and_then(|v| v.as_u64())
                .map(|n| Self::Tail(n as usize))
                .unwrap_or(Self::Full),
            _ => Self::Full,
        }
    }
}

/// 流式捕获一条输出流；Full 超 cap → Err（调用方 kill + Failed）。
async fn capture_stream(
    stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    mode: OutputMode,
    cap: usize,
) -> Result<Vec<u8>, ()> {
    use tokio::io::AsyncReadExt;
    let Some(mut stream) = stream else {
        return Ok(Vec::new());
    };
    let mut buf: std::collections::VecDeque<u8> = Default::default();
    let mut chunk = [0u8; 8192];
    let mut len = 0usize;
    loop {
        let n = stream.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            break;
        }
        match mode {
            OutputMode::Full => {
                len += n;
                if len > cap {
                    return Err(());
                }
                buf.extend(&chunk[..n]);
            }
            OutputMode::Tail(limit) => {
                buf.extend(&chunk[..n]);
                let excess = buf.len().saturating_sub(limit);
                buf.drain(..excess);
            }
            OutputMode::Discard => {}
        }
    }
    Ok(buf.into_iter().collect())
}

/// files.glob：工作区相对路径，排序；超 FILES_GLOB_MAX 拒绝（不截断）。
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
    if out.len() > facade::FILES_GLOB_MAX {
        return Err(format!("files.glob matched more than {} files", facade::FILES_GLOB_MAX));
    }
    out.sort();
    Ok(out)
}

/// files.read：只读前 cap 字节。
fn read_capped(path: &std::path::Path, cap: usize) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.take(cap as u64)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// files.grep：遍历工作区，glob 过滤相对路径；匹配/字节超限拒绝。
fn grep_files(cwd: &std::path::Path, pattern: &str, glob: Option<&str>) -> Result<Vec<Value>, String> {
    use grep_matcher::Matcher;
    let re = grep_regex::RegexMatcher::new(pattern).map_err(|e| e.to_string())?;
    let filter = glob
        .map(|g| {
            globset::GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .map(|b| b.compile_matcher())
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for entry in ignore::Walk::new(cwd).filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        if let Some(f) = &filter
            && !f.is_match(rel)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line.as_bytes()).unwrap_or(false) {
                let item = json!({
                    "path": rel.display().to_string(),
                    "line": i + 1,
                    "text": line,
                });
                bytes += serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
                out.push(item);
                if out.len() > facade::FILES_GREP_MAX_MATCHES
                    || bytes > facade::FILES_GREP_MAX_BYTES
                {
                    return Err(format!(
                        "files.grep exceeded the cap ({} matches / {} bytes)",
                        facade::FILES_GREP_MAX_MATCHES,
                        facade::FILES_GREP_MAX_BYTES
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// artifact.file 的扩展名 → content_type 兜底表（无 mime_guess 依赖）。
fn content_type_of(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" | "mts" | "cts" => "text/typescript",
        "json" | "jsonl" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// SeedFrom 截断点：跳过 header，保留到第 `user_turns` 个 user 消息所属回合结束
///（即第 user_turns+1 个 user 消息之前；不足则取全部）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use moc_ptc_protocol::wire::WorldReadKind;
    use tokio::io::AsyncWriteExt;

    /// WorldReadKind 的 serde 名必须被 world_read_impl 识别（防 Debug 小写化回归）。
    #[tokio::test]
    async fn world_read_wire_names_are_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let kinds = [
            WorldReadKind::FilesGlob,
            WorldReadKind::FilesRead,
            WorldReadKind::FilesGrep,
            WorldReadKind::GitChangedFiles,
            WorldReadKind::GitDiff,
            WorldReadKind::GitStatus,
            WorldReadKind::GitLog,
        ];
        for kind in kinds {
            let op = serde_op_name(&kind);
            let outcome = world_read_impl(dir.path(), 1024, &op, &json!([])).await;
            if let DriverOutcome::Failed { message, .. } = &outcome {
                assert!(
                    !message.contains("unknown world.read op"),
                    "{op} 未被识别"
                );
            }
        }
        assert_eq!(serde_op_name(&WorldReadKind::FilesGlob), "files_glob");
        assert_eq!(serde_op_name(&WorldReadKind::GitStatus), "git_status");
    }

    #[test]
    fn workspace_file_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(workspace_file(dir.path(), "../x").is_err(), "../ 逃逸应拒");
        let outside = tempfile::tempdir().unwrap();
        let abs = outside.path().join("f.txt");
        std::fs::write(&abs, "x").unwrap();
        assert!(
            workspace_file(dir.path(), abs.to_str().unwrap()).is_err(),
            "绝对路径出界应拒"
        );
    }

    #[test]
    fn glob_files_sorted_relative() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "c").unwrap();
        let out = glob_files(dir.path(), "*.txt").unwrap();
        // `*` 跨分隔符（literal_separator=false），递归匹配且排序。
        assert_eq!(out, vec!["a.txt", "b.txt", "sub/c.txt"]);
        let out = glob_files(dir.path(), "b.*").unwrap();
        assert_eq!(out, vec!["b.txt"]);
    }

    #[test]
    fn grep_files_glob_filter_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.ts"), "needle here\n").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "needle too\n").unwrap();
        let out = grep_files(dir.path(), "needle", Some("*.ts")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["path"], "src/a.ts", "glob 过滤且相对路径");
        let out = grep_files(dir.path(), "needle", None).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn porcelain_parses_into_facade_status() {
        let text = "## main...origin/main\n M src/a.rs\nA  src/b.rs\n?? src/new.rs\nR  old.rs -> src/moved.rs\n";
        let status = parse_porcelain(text, "");
        assert_eq!(status["branch"], "main");
        assert_eq!(status["clean"], false);
        assert_eq!(status["staged"], json!(["src/b.rs", "src/moved.rs"]));
        assert_eq!(status["unstaged"], json!(["src/a.rs"]));
        assert_eq!(status["untracked"], json!(["src/new.rs"]));
        // 子目录工作区：prefix 去前缀。
        let status = parse_porcelain("## main\n?? sub/f.rs\n", "sub/");
        assert_eq!(status["untracked"], json!(["f.rs"]));
    }

    #[tokio::test]
    async fn capture_stream_tail_and_discard() {
        // tail：按字节保留尾部。
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&data).await.unwrap();
        let buf = capture_stream(
            child.stdout.take().map(|s| {
                Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>
            }),
            OutputMode::Tail(64),
            usize::MAX,
        )
        .await
        .unwrap();
        assert_eq!(buf.len(), 64);
        assert_eq!(buf.as_slice(), &data[data.len() - 64..]);
        // discard：无管道 → 空。
        let buf = capture_stream(None, OutputMode::Discard, usize::MAX).await.unwrap();
        assert!(buf.is_empty());
        assert!(matches!(OutputMode::of(&json!({"tail": 7})), OutputMode::Tail(7)));
        assert!(matches!(OutputMode::of(&json!("discard")), OutputMode::Discard));
        assert!(matches!(OutputMode::of(&Value::Null), OutputMode::Full));
    }

    /// Full 模式单流超限：立即 kill 返回超限 Failed，而不是干等到 E_WORLD_TIMEOUT。
    #[tokio::test]
    async fn world_run_full_overflow_kills_fast() {
        let start = std::time::Instant::now();
        let outcome = run_capped(
            std::ffi::OsStr::new("sh"),
            &[
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("yes | head -c 1048576"),
            ],
            &[],
            std::path::Path::new("/tmp"),
            60_000,
            &Value::Null,
            64,
        )
        .await;
        let elapsed = start.elapsed();
        match outcome {
            DriverOutcome::Failed { code, message } => {
                assert_eq!(code, "E_WORLD", "超限不该是超时：{message}");
                assert!(message.contains("64"), "{message}");
            }
            other => panic!("超限应 Failed，实际 {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "超限应立即返回，实际耗时 {elapsed:?}"
        );
    }

    fn user_entry(id: &str) -> Entry {
        Entry::Message {
            id: id.into(),
            parent_id: None,
            at_ms: 0,
            message: moc_llm::Message::user_text("q"),
            tool: None,
            finish_reason: None,
            bash: None,
            agent: None,
            goal: None,
            reminder: None,
        }
    }

    fn assistant_entry(id: &str) -> Entry {
        Entry::Message {
            id: id.into(),
            parent_id: None,
            at_ms: 0,
            message: moc_llm::Message::assistant_text("a"),
            tool: None,
            finish_reason: None,
            bash: None,
            agent: None,
            goal: None,
            reminder: None,
        }
    }

    #[test]
    fn transcript_prefix_cuts_at_user_turns() {
        // transcript：header + (user, assistant) × N。
        let mut entries = vec![user_entry("h")];
        for i in 0..3 {
            entries.push(user_entry(&format!("u{i}")));
            entries.push(assistant_entry(&format!("a{i}")));
        }
        // user_turns=2 → 保留到第 2 个 user 回合结束（header 始终跳过）。
        let out = transcript_prefix(&entries, 2);
        let users = out
            .iter()
            .filter(|e| matches!(e, Entry::Message { message, .. } if message.role == Role::User))
            .count();
        assert_eq!(users, 2, "只保留前 2 个 user 回合");
        assert_eq!(out.len(), 4);
        // user_turns=0 → 空前缀。
        assert!(transcript_prefix(&entries, 0).is_empty());
    }

    #[test]
    fn resolve_actor_model_ordering() {
        let meta = |role_models: &[(&str, &str)], ref_map: &[(&str, &str)]| {
            super::super::RunMeta {
                run_id: "r".into(),
                status: "running".into(),
                mode: "launch".into(),
                title: String::new(),
                created_at_ms: 0,
                finished_at_ms: None,
                resumed_from: None,
                args: Value::Null,
                commands: vec![],
                role_models: role_models
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                model_ref_map: ref_map
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                subagent_model: Some("sub-m".into()),
                session_model: Some("sess-m".into()),
                result: None,
                error: None,
                stop_reason: None,
                reports: vec![],
                artifacts: vec![],
                holes: vec![],
                roles: vec![],
                phases: vec![],
                usage: Value::Null,
                plan: json!({
                    "roles": [
                        {"role_id": "routed", "model_source": {"kind": "routed", "refs": ["model#0"]}},
                        {"role_id": "lit", "model_source": {"kind": "literal", "model": "lit-m"}},
                    ],
                    "model_refs": [{"ref_id": "model#0", "model_id": "ref-m"}],
                }),
            }
        };
        // site#ord > site > persona.literal > plan > subagent > session。
        let m = meta(&[("a#2", "ord-m"), ("a", "site-m")], &[]);
        assert_eq!(resolve_actor_model(&m, "a", 2, &json!({})), "ord-m");
        assert_eq!(resolve_actor_model(&m, "a", 0, &json!({})), "site-m");
        // persona.model 字面量。
        assert_eq!(
            resolve_actor_model(&m, "b", 0, &json!({"model": "per-m"})),
            "per-m"
        );
        // persona.model ref → model_ref_map 重映射。
        let m2 = meta(&[], &[("ref-x", "mapped-m")]);
        assert_eq!(
            resolve_actor_model(&m2, "b", 0, &json!({"model": {"model_id": "raw-m", "ref_id": "ref-x"}})),
            "mapped-m"
        );
        // plan routed → model_ref_map；未映射 → plan.model_refs。
        let m3 = meta(&[], &[("model#0", "mapped2")]);
        assert_eq!(resolve_actor_model(&m3, "routed", 0, &json!({})), "mapped2");
        assert_eq!(resolve_actor_model(&m2, "routed", 0, &json!({})), "ref-m");
        // plan literal。
        assert_eq!(resolve_actor_model(&m2, "lit", 0, &json!({})), "lit-m");
        // inherit → subagent_model → session_model。
        assert_eq!(resolve_actor_model(&m2, "unknown", 0, &json!({})), "sub-m");
        let m4 = meta(&[], &[]);
        let mut m5 = m4.clone();
        m5.subagent_model = None;
        assert_eq!(resolve_actor_model(&m5, "unknown", 0, &json!({})), "sess-m");
    }
}
