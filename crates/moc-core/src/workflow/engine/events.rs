use super::ActorInfo;
use super::Engine;
use super::EngineAction;
use super::EngineEvent;
use super::ImportedActor;
use super::RunMode;
use super::ScriptState;
use super::SessionEvent;
use super::Step;
use super::TerminalStatus;
use super::is_preset_member;
use crate::workflow::engine::JournalRecord;
use crate::workflow::engine::RecordKind;
use crate::workflow::engine::input_hash;
use crate::workflow::facade;
use serde_json::Map;
use serde_json::json;

impl Engine {
    /// driver 建立 actor 子会话后回报：写 `actor` 记录并登记上下文。
    /// resume/amend 依赖它恢复 Existing/SeedFrom 决策。
    pub fn actor_session_created(
        &mut self,
        actor_site_id: &str,
        actor_ordinal: u32,
        child_session_id: &str,
    ) {
        let key = (actor_site_id.to_string(), actor_ordinal);
        if self.actor_sessions.contains_key(&key) {
            return;
        }
        self.actor_sessions
            .insert(key, child_session_id.to_string());
        let actor = self.actors.get(&(actor_site_id.to_string(), actor_ordinal));
        let mut extra = Map::new();
        extra.insert("child_session_id".into(), json!(child_session_id));
        if let Some(actor) = actor {
            if let Some(name) = &actor.name {
                extra.insert("actor_name".into(), json!(name));
            }
            extra.insert("persona_key".into(), json!(actor.persona_key));
            // 完整 persona 入 journal：resume/amend 恢复 actor 时用真实值，
            // 不回退 plan.roles 的截断摘要。
            extra.insert("persona".into(), actor.persona.clone());
        }
        let record = JournalRecord {
            kind: RecordKind::Actor,
            site_id: actor_site_id.to_string(),
            ordinal: actor_ordinal,
            input_hash: String::new(),
            settle: self.settle,
            outcome: json!({"status": "completed", "result": null}),
            extra,
        };
        self.settle += 1;
        let _ = self.store.append(&record);
    }

    /// 处理一次 guest step：先事件（含 actor 注册），再请求（重放→回执 / miss→dispatch），
    /// 最后脚本状态（终态/停滞）。
    pub fn handle_step(&mut self, step: Step) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        if self.terminal.is_some() {
            return actions;
        }
        for event in step.events {
            self.handle_event(event, &mut actions);
        }
        for request in step.requests {
            self.handle_request(request, &mut actions);
        }
        match step.state {
            ScriptState::Done { value } => {
                self.finish(
                    TerminalStatus::Completed,
                    Some(value),
                    None,
                    None,
                    &mut actions,
                );
            }
            ScriptState::Failed { code, message } => {
                self.finish(
                    TerminalStatus::Errored,
                    None,
                    Some(code),
                    Some(message),
                    &mut actions,
                );
            }
            ScriptState::Running => {
                // 停滞 = 不可恢复：整个 run errored，附带 channel 名与等待数。
                if let Some(stall) = step.stall {
                    if stall.waiting.is_empty() {
                        self.errored(
                            "ScriptStalled",
                            "the script is suspended with no request in flight and no channel receiver waiting, so nothing can ever resolve what it awaits",
                            &mut actions,
                        );
                    } else {
                        self.errored(
                            "ChannelDeadlock",
                            format!(
                                "deadlock: {} receiver(s) waiting on channel(s) [{}] with no request in flight",
                                stall.waiting.len(),
                                stall.waiting.join(", ")
                            ),
                            &mut actions,
                        );
                    }
                }
            }
        }
        actions
    }

    /// 不可恢复错误 → 终态 errored（先到者胜由 finish 保证）。
    pub(super) fn errored(
        &mut self,
        code: &str,
        message: impl ToString,
        actions: &mut Vec<EngineAction>,
    ) {
        self.finish(
            TerminalStatus::Errored,
            None,
            Some(code.into()),
            Some(message.to_string()),
            actions,
        );
    }

    pub(super) fn handle_event(&mut self, event: EngineEvent, actions: &mut Vec<EngineAction>) {
        match event {
            EngineEvent::CreateActor {
                site_id,
                name,
                persona,
            } => {
                let ordinal = self.ordinal_of(&site_id);
                // 运行期重名（模板/变量名）：整个 run errored。字面量重名编译期已拦。
                if let Some(n) = &name
                    && self.actor_names.contains_key(n)
                {
                    self.errored(
                        "DuplicateActorName",
                        format!("actor name \"{n}\" is already taken by another actor in this run"),
                        actions,
                    );
                    return;
                }
                let persona_key = input_hash(&persona);
                let import = if self.config.mode == RunMode::Amend {
                    name.as_ref().and_then(|n| {
                        self.prior_actor_asks.get(n).map(|entries| ImportedActor {
                            entries: entries.clone(),
                            consumed: 0,
                            diverged: false,
                        })
                    })
                } else {
                    None
                };
                if let Some(n) = &name {
                    self.actor_names
                        .insert(n.clone(), (site_id.clone(), ordinal));
                }
                self.actors.insert(
                    (site_id.clone(), ordinal),
                    ActorInfo {
                        name: name.clone(),
                        persona_key,
                        persona: persona.clone(),
                        import,
                    },
                );
                actions.push(EngineAction::Session(SessionEvent::ActorCreated {
                    site_id,
                    ordinal,
                    name,
                }));
            }
            EngineEvent::Phase { site_id: _, name } => {
                actions.push(EngineAction::Session(SessionEvent::Phase { name }));
            }
            EngineEvent::Log { level, message, .. } => {
                actions.push(EngineAction::Session(SessionEvent::Log { level, message }));
            }
            EngineEvent::Report {
                item, artifact_id, ..
            } => {
                // artifact_id 必须指向已执行的 preset 声明；未声明或指向
                // file/markdown → 整 run 失败（facade：tag naming nothing fails）。
                if let Some(id) = &artifact_id
                    && !self
                        .artifact_members
                        .get(id)
                        .is_some_and(|m| is_preset_member(m))
                {
                    self.errored(
                        "ReportTarget",
                        format!("report artifact_id \"{id}\" is not a declared preset artifact"),
                        actions,
                    );
                    return;
                }
                // caps fail the whole run（facade）：条目数 / 单条 / 累计。
                self.report_count += 1;
                if self.report_count > facade::REPORT_MAX_ITEMS {
                    self.errored(
                        "ReportLimit",
                        format!("run exceeds {} report items", facade::REPORT_MAX_ITEMS),
                        actions,
                    );
                    return;
                }
                let bytes = serde_json::to_vec(&item)
                    .map(|v| v.len() as u64)
                    .unwrap_or(u64::MAX);
                if bytes > facade::REPORT_MAX_ITEM_BYTES as u64 {
                    self.errored(
                        "ReportLimit",
                        format!(
                            "report item exceeds {} bytes serialized",
                            facade::REPORT_MAX_ITEM_BYTES
                        ),
                        actions,
                    );
                    return;
                }
                self.report_bytes += bytes;
                if self.report_bytes > facade::REPORT_MAX_TOTAL_BYTES {
                    self.errored(
                        "ReportLimit",
                        format!(
                            "run exceeds {} bytes of reports",
                            facade::REPORT_MAX_TOTAL_BYTES
                        ),
                        actions,
                    );
                    return;
                }
                actions.push(EngineAction::Session(SessionEvent::Report {
                    item,
                    artifact_id,
                }));
            }
            EngineEvent::DeclareArtifact { member, spec, .. } => {
                // wire 形：[id, specObj]；preset 声明同步落引擎校验。
                let artifact_id = spec.get(0).and_then(|v| v.as_str()).map(String::from);
                let spec_obj = spec.get(1).cloned();
                match (artifact_id, spec_obj) {
                    (Some(id), Some(spec_obj)) if spec_obj.is_object() => {
                        match self.artifact_members.get(&id) {
                            Some(prev) if *prev == member => {
                                // 相同重声明 no-op；不同 spec 或不同 member → errored。
                                if self.artifact_specs.get(&id) != Some(&spec_obj) {
                                    self.errored(
                                        "ArtifactRedeclared",
                                        format!(
                                            "artifact \"{id}\" redeclared with a different spec"
                                        ),
                                        actions,
                                    );
                                }
                            }
                            Some(prev) => {
                                self.errored(
                                    "ArtifactMemberConflict",
                                    format!("artifact \"{id}\" is used with both `{prev}` and `{member}`"),
                                    actions,
                                );
                            }
                            None => {
                                if self.artifact_members.len() >= facade::ARTIFACT_MAX_IDS {
                                    self.errored(
                                        "ArtifactLimit",
                                        format!(
                                            "run exceeds {} artifact ids",
                                            facade::ARTIFACT_MAX_IDS
                                        ),
                                        actions,
                                    );
                                } else {
                                    self.artifact_members.insert(id.clone(), member.clone());
                                    self.artifact_specs.insert(id.clone(), spec_obj.clone());
                                    actions.push(EngineAction::Session(
                                        SessionEvent::DeclareArtifact {
                                            member,
                                            spec: spec_obj,
                                            artifact_id: Some(id),
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    _ => {
                        self.errored(
                            "ArtifactSpec",
                            "artifact declaration must carry an id and a spec object",
                            actions,
                        );
                    }
                }
            }
            EngineEvent::HoleOpened {
                site_id,
                name,
                prompt,
                schema,
            } => {
                // 与同次 HoleResolve 请求共享序号：事件侧 peek，请求侧消耗。
                let ordinal = self.peek_ordinal(&site_id);
                self.open_holes
                    .insert((site_id.clone(), ordinal), name.clone());
                actions.push(EngineAction::Session(SessionEvent::HoleOpened {
                    site_id,
                    ordinal,
                    name,
                    prompt,
                    schema,
                }));
            }
            EngineEvent::HoleCancelled { name, .. } => {
                actions.push(EngineAction::Session(SessionEvent::HoleCancelled { name }));
            }
        }
    }
}
