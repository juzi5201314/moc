//! workflow driver：把引擎派发映射到真实副作用。
//!
//! - `Ask` → run 专属子会话：guest 声明的 `persona.system` 与 `persona.tools`
//!   冻结为动态角色契约；首个 ask 以 `task` 发起，后续 ask 走 run 专属消息入口。
//!   `output_schema` 约束结构化交付；不符合 schema 时修复重试有上限
//!   （`[workflow].ask_max_repairs`）。上下文续接按 [`ActorContext`]：Fresh 新
//!   会话；Existing 续本会话；SeedFrom 真 fork——把前驱 actor 子会话的
//!   transcript 逐字截到第 user_turns 个 user 消息之前，复制成新子会话，
//!   前驱文件保持不动（那是前驱 run 的审计记录）。
//! - `world.run` → 固定 argv 直跑（不经 shell），由会话沙箱以 argv 包裹（审批命令集即授权）；
//! - `world.read` → files.* / git.* 只读实现（git 固定白名单子命令）；
//! - `artifact.file/markdown` → 版本化写 `<run>/artifacts/`。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::service::CoreInner;
use crate::session::entries::Entry;
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
            DriverRequest::CancelAsk {
                actor_site_id,
                actor_ordinal,
                ..
            } => self.cancel_ask(actor_site_id, *actor_ordinal).await,
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
        let existing = self.run.view.lock().actor_sessions.get(&actor_key).cloned();
        let model = {
            let meta = self.run.meta.lock();
            resolve_actor_model(&meta, actor_site_id, *actor_ordinal, persona)
        };
        let child_id = match existing {
            Some(id) => id,
            None => match self
                .spawn_actor(context, instructions, schema, &model, persona)
                .await
            {
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
        let mut text = if first {
            None
        } else {
            Some(instructions.clone())
        };
        for attempt in 0..=max_repairs {
            let send_text = text
                .take()
                .or_else(|| (attempt > 0 || !first).then(|| instructions.clone()));
            if let Some(text) = send_text
                && let Err(e) = crate::agents::message_workflow(
                    Arc::clone(&self.inner),
                    self.run.request.clone(),
                    self.session_id.clone(),
                    &self.run.id,
                    child_id.clone(),
                    text,
                )
                .await
            {
                return DriverOutcome::Failed {
                    code: "E_ACTOR".into(),
                    message: format!("failed to deliver message to actor: {e}"),
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
                    message: format!("failed while waiting for actor: {}", result.unwrap_err()),
                };
            };
            let Some(entry) = result.agents.first() else {
                return DriverOutcome::Failed {
                    code: "E_ACTOR".into(),
                    message: "actor session unreachable".into(),
                };
            };
            // 交付值：schema 化的取 structured_result（Null 视为未交付）；自由
            // 文本交付取 yield 报告。
            let value = entry
                .structured_result
                .clone()
                .filter(|v| !v.is_null())
                .or_else(|| entry.message.as_ref().map(|m| Value::String(m.text())));
            let usage = usage_diff(
                &self.child_usage(&child_id).await,
                &usage_before,
                pricing.as_ref(),
            );
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
                            message: "ask output did not match output_schema after repair retries".into(),
                        };
                    }
                    text = Some(format!(
                        "Previous reply failed output_schema validation. Return ONLY JSON matching:\n{}",
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

    /// 仅当前 run 的 driver 可创建动态 actor；提示、工具与所属 run 一并冻结。
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
            ActorContext::SeedFrom {
                child_session_id,
                user_turns,
                ..
            } => {
                let source = match self.inner.sessions.get(child_session_id).await {
                    Ok(record) => record,
                    Err(e) => {
                        return Err(DriverOutcome::Failed {
                            code: "E_ACTOR".into(),
                            message: format!("failed to read predecessor actor session: {e}"),
                        });
                    }
                };
                let st = source.state.lock().await;
                transcript_prefix(&st.log_entries(), *user_turns)
            }
            _ => Vec::new(),
        };
        // 角色提示使用 guest 声明全文，不使用 plan 中截断的展示摘要。
        let context_text: Option<String> = persona
            .get("system")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let tools = match persona.get("tools") {
            None => None,
            Some(value) => match serde_json::from_value::<Vec<String>>(value.clone()) {
                Ok(names) if names.iter().all(|name| !name.trim().is_empty()) => Some(
                    crate::agents::ToolPolicy::Allow(names.into_iter().collect()),
                ),
                _ => {
                    return Err(DriverOutcome::Failed {
                        code: "E_ARGS".into(),
                        message: "persona.tools must be an array of non-empty tool names".into(),
                    });
                }
            },
        };
        let spec = crate::agents::SpawnSpec {
            task: instructions.to_string(),
            title: persona
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| Some("workflow actor".into())),
            profile: crate::agents::SpawnProfile::Workflow {
                run_id: self.run.id.clone(),
                model: model.to_owned(),
                system: context_text,
                tools,
                output_schema: schema.clone(),
            },
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
                message: format!("failed to spawn actor: {e}"),
            }),
        }
    }

    /// `world.run`：固定 argv 经 sandbox wrap 直跑（不经 shell）；命令集审批时已授权。
    async fn world_run(&self, request: &DriverRequest) -> DriverOutcome {
        let DriverRequest::WorldRun {
            command,
            args,
            timeout_ms,
            output,
            ..
        } = request
        else {
            unreachable!()
        };
        let policy = self.run.policy.lock().clone();
        let Some(policy) = policy else {
            return DriverOutcome::Failed {
                code: "E_SANDBOX".into(),
                message: "world.run missing sandbox policy snapshot".into(),
            };
        };
        world_run_impl(
            &policy,
            command,
            args,
            *timeout_ms,
            output,
            &self.run.cancel,
        )
        .await
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
                message: format!("artifact.{member} requires an id"),
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
        if title
            .as_deref()
            .is_some_and(|t| t.chars().count() > facade::ARTIFACT_MAX_TITLE_LEN)
            || description
                .as_deref()
                .is_some_and(|d| d.chars().count() > facade::ARTIFACT_MAX_DESCRIPTION_LEN)
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
            let requested = opts
                .get("primary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
            (
                was_primary || requested,
                prior.iter().filter(|a| a.get("version").is_some()).count() as u32 + 1,
            )
        };
        if version as usize > facade::ARTIFACT_MAX_VERSIONS {
            return DriverOutcome::Failed {
                code: "E_ARTIFACT".into(),
                message: format!(
                    "artifact \"{id}\" exceeds {} versions",
                    facade::ARTIFACT_MAX_VERSIONS
                ),
            };
        }
        let dir = self.run.dir.join("artifacts").join(&id);
        let (stored, content_type) = match member {
            "file" => {
                let Some(path_arg) = args.get(1).and_then(|a| a.as_str()) else {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: "artifact.file requires path".into(),
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
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: e.to_string(),
                    };
                }
                let content_type =
                    opt_str("contentType").unwrap_or_else(|| content_type_of(&source).to_string());
                (stored, Some(content_type))
            }
            _ => {
                let Some(content) = args.get(1).and_then(|a| a.as_str()) else {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: "artifact.markdown requires content string".into(),
                    };
                };
                if content.len() > facade::ARTIFACT_MAX_MARKDOWN_BYTES {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: format!(
                            "markdown exceeds {} bytes",
                            facade::ARTIFACT_MAX_MARKDOWN_BYTES
                        ),
                    };
                }
                let stored = format!("v{version}_content.md");
                if let Err(e) = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(dir.join(&stored), content))
                {
                    return DriverOutcome::Failed {
                        code: "E_ARTIFACT".into(),
                        message: e.to_string(),
                    };
                }
                (
                    stored,
                    opt_str("contentType").or_else(|| Some("text/markdown".into())),
                )
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
                    .filter(|e| {
                        e.message_entry()
                            .is_some_and(|entry| entry.message.role == Role::User)
                    })
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

mod world;
use world::workspace_file;
pub(crate) use world::world_read_impl;
pub(crate) use world::world_run_impl;

fn schema_ok(schema: &Value, value: &Value) -> bool {
    jsonschema::validator_for(schema)
        .map(|v| v.is_valid(value))
        .unwrap_or(false)
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
mod tests;
