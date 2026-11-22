use super::DriverOutcome;
use super::DriverRequest;
use super::Engine;
use super::EngineAction;
use super::EngineRequest;
use super::HashMismatch;
use super::Pending;
use super::PendingReq;
use super::RunMode;
use super::SessionEvent;
use super::guest_outcome;
use super::is_preset_member;
use super::outcome_of;
use crate::workflow::engine::JournalRecord;
use crate::workflow::engine::RecordKind;
use crate::workflow::engine::input_hash;
use crate::workflow::facade;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

impl Engine {
    pub(super) fn handle_request(
        &mut self,
        request: EngineRequest,
        actions: &mut Vec<EngineAction>,
    ) {
        let request_id = request.request_id();
        let site_id = request.site_id().to_string();
        let ordinal = self.ordinal_of(&site_id);
        let kind = request.kind();
        let hash = self.request_hash(&request);

        // —— artifact 发布侧运行期门禁（facade：content 类 catchable reject）——
        // 位于重放/导入查找之前：重放命中的 publish 同样登记 id/member，
        // 否则 resume 后 32-id 上限与 member 冲突判定失真。
        if let EngineRequest::PublishArtifact { member, spec, .. } = &request {
            match spec.get(0).and_then(|v| v.as_str()).map(String::from) {
                Some(id) => {
                    match self.artifact_members.get(&id) {
                        Some(prev) if *prev == *member => {}
                        Some(prev) => {
                            if is_preset_member(prev) {
                                // 与已声明 preset 冲突 → 拒绝本次发布（可捕获）。
                                actions.push(EngineAction::Complete {
                                    request_id,
                                    outcome: DriverOutcome::Failed {
                                        code: "E_ARTIFACT".into(),
                                        message: format!(
                                            "artifact id \"{id}\" is already declared as `{prev}`"
                                        ),
                                    },
                                });
                            } else {
                                // 同一 id 换 member（file↔markdown）→ 整 run 失败。
                                self.errored(
                                "ArtifactMemberConflict",
                                format!("artifact \"{id}\" is used with both `{prev}` and `{member}`"),
                                actions,
                            );
                            }
                            return;
                        }
                        None => {
                            if self.artifact_members.len() >= facade::ARTIFACT_MAX_IDS {
                                actions.push(EngineAction::Complete {
                                    request_id,
                                    outcome: DriverOutcome::Failed {
                                        code: "E_ARTIFACT".into(),
                                        message: format!(
                                            "run exceeds {} artifact ids",
                                            facade::ARTIFACT_MAX_IDS
                                        ),
                                    },
                                });
                                return;
                            }
                            self.artifact_members.insert(id, member.clone());
                        }
                    }
                }
                None => {
                    actions.push(EngineAction::Complete {
                        request_id,
                        outcome: DriverOutcome::Failed {
                            code: "E_ARTIFACT".into(),
                            message: format!("artifact.{member} requires an id"),
                        },
                    });
                    return;
                }
            }
        }

        // —— ask 的 amend 导入路径（仅 Amend 模式）——
        if self.config.mode == RunMode::Amend
            && let EngineRequest::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                ..
            } = &request
        {
            let actor_key = (actor_site_id.clone(), *actor_ordinal);
            // 先在借用域内算出结果，再推 actions（避免 self 二次可变借用）。
            enum Import {
                Hit(Value),
                Diverged(Option<String>),
            }
            let result = (|| {
                let actor = self.actors.get_mut(&actor_key)?;
                let import = actor.import.as_mut()?;
                if import.diverged {
                    return None;
                }
                Some(match import.entries.get(import.consumed) {
                    Some(entry)
                        if entry.extra.get("actor_seq").and_then(|v| v.as_u64())
                            == Some(*actor_seq as u64)
                            && entry.extra.get("persona_key").and_then(|v| v.as_str())
                                == Some(actor.persona_key.as_str())
                            && entry.input_hash == hash =>
                    {
                        import.consumed += 1;
                        Import::Hit(entry.outcome.clone())
                    }
                    _ => {
                        import.diverged = true;
                        Import::Diverged(actor.name.clone())
                    }
                })
            })();
            match result {
                Some(Import::Hit(outcome_json)) => {
                    self.account(&outcome_json);
                    actions.push(EngineAction::Complete {
                        request_id,
                        outcome: guest_outcome(kind, outcome_of(&outcome_json)),
                    });
                    return;
                }
                Some(Import::Diverged(Some(name))) => {
                    actions.push(EngineAction::Session(SessionEvent::ActorDiverged { name }));
                }
                _ => {}
            }
        }

        // —— (site_id, ordinal) 重放路径：仅 Resume（同一 run 重启，含 ask 零 token）——
        let record = if self.config.mode == RunMode::Resume
            && !self.consumed.contains(&(site_id.clone(), ordinal))
        {
            self.replay
                .get(&site_id, ordinal)
                .filter(|r| r.kind == kind && !self.consumed.contains(&r.key()))
                .cloned()
        } else {
            None
        };
        if let Some(record) = record {
            if record.input_hash == hash {
                self.consumed.insert(record.key());
                let outcome = guest_outcome(kind, outcome_of(&record.outcome));
                self.account(&record.outcome);
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome,
                });
                return;
            }
            // resume 的哈希不一致说明脚本或输入不可重放 → 整个 run errored，不得重执行。
            self.mismatches.push(HashMismatch {
                site_id: site_id.clone(),
                ordinal,
                expected: record.input_hash.clone(),
                actual: hash.clone(),
            });
            self.consumed.insert(record.key());
            self.errored(
                "InputHashMismatch",
                format!(
                    "site {site_id}#{ordinal} produced a different input than recorded in the journal; the script is non-deterministic and the run cannot be replayed"
                ),
                actions,
            );
            return;
        }

        // —— miss → dispatch（ask 受并发上限钳制）——
        let pending_req = match &request {
            EngineRequest::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                instructions,
                schema,
                ..
            } => PendingReq::Ask {
                actor_site_id: actor_site_id.clone(),
                actor_ordinal: *actor_ordinal,
                actor_seq: *actor_seq,
                instructions: instructions.clone(),
                schema: schema.clone(),
            },
            EngineRequest::WorldRead { op, args, .. } => PendingReq::WorldRead {
                op: op.clone(),
                args: args.clone(),
            },
            EngineRequest::WorldRun {
                command,
                args,
                timeout_ms,
                output,
                ..
            } => PendingReq::WorldRun {
                command: command.clone(),
                args: args.clone(),
                timeout_ms: *timeout_ms,
                output: output.clone(),
            },
            EngineRequest::PublishArtifact { member, spec, .. } => PendingReq::PublishArtifact {
                member: member.clone(),
                spec: spec.clone(),
            },
            EngineRequest::HoleResolve { .. } => PendingReq::HoleResolve,
            EngineRequest::CancelAsk {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                ..
            } => {
                // CancelAsk 不做 journal 重放，直接派发并回执。
                actions.push(EngineAction::Dispatch(DriverRequest::CancelAsk {
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                }));
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome: DriverOutcome::Completed(json!({"cancelled": true})),
                });
                return;
            }
        };
        let actor_key = match &pending_req {
            PendingReq::Ask {
                actor_site_id,
                actor_ordinal,
                ..
            } => Some((actor_site_id.clone(), *actor_ordinal)),
            _ => None,
        };
        // 同 actor FIFO：actor 忙（有 ask 在飞或在全局队列）→ 进 actor 队列等位。
        if let Some(key) = &actor_key {
            if self.busy_actors.contains(key) {
                self.pending.insert(
                    request_id,
                    Pending {
                        site_id: site_id.clone(),
                        ordinal,
                        kind,
                        input_hash: hash,
                        request: pending_req,
                        queued: true,
                    },
                );
                self.actor_queues
                    .entry(key.clone())
                    .or_default()
                    .push_back(request_id);
                return;
            }
            self.busy_actors.insert(key.clone());
        }
        let queued = actor_key.is_some()
            && self.config.max_concurrency > 0
            && self.in_flight_asks >= self.config.max_concurrency;
        self.pending.insert(
            request_id,
            Pending {
                site_id: site_id.clone(),
                ordinal,
                kind,
                input_hash: hash,
                request: pending_req,
                queued,
            },
        );
        if queued {
            self.queued_asks.push_back(request_id);
        } else {
            self.dispatch(request_id, actions);
        }
    }

    /// 一个 ask 走全局派发逻辑：并发满则进全局队列（actor 保持 busy），否则立即派发。
    pub(super) fn offer_ask(&mut self, request_id: u64, actions: &mut Vec<EngineAction>) {
        let full =
            self.config.max_concurrency > 0 && self.in_flight_asks >= self.config.max_concurrency;
        if let Some(p) = self.pending.get_mut(&request_id) {
            p.queued = full;
        }
        if full {
            self.queued_asks.push_back(request_id);
        } else {
            self.dispatch(request_id, actions);
        }
    }

    /// 请求输入哈希：种类 + 决定输出的参数字段（不含 request_id/site_id）。
    pub(super) fn request_hash(&self, request: &EngineRequest) -> String {
        match request {
            EngineRequest::Ask {
                instructions,
                schema,
                actor_seq,
                actor_site_id,
                actor_ordinal,
                ..
            } => {
                // spec：ask 哈希 = 指令 + persona（schema/seq 亦决定交付形态）。
                let persona_key = self
                    .actors
                    .get(&(actor_site_id.clone(), *actor_ordinal))
                    .map_or(Value::Null, |a| json!(a.persona_key));
                input_hash(&json!({
                    "instructions": instructions,
                    "schema": schema,
                    "actor_seq": actor_seq,
                    "persona_key": persona_key,
                }))
            }
            EngineRequest::WorldRead { op, args, .. } => {
                input_hash(&json!({"op": op, "args": args}))
            }
            EngineRequest::WorldRun {
                command,
                args,
                timeout_ms,
                output,
                ..
            } => input_hash(&json!({
                "cmd": command, "args": args, "timeout_ms": timeout_ms, "output": output,
            })),
            EngineRequest::PublishArtifact { member, spec, .. } => {
                input_hash(&json!({"member": member, "spec": spec}))
            }
            EngineRequest::HoleResolve { .. } => input_hash(&json!("hole")),
            EngineRequest::CancelAsk { .. } => String::new(),
        }
    }

    /// 宿主填充 hole（fill_hole 工具 → worker 回执）。
    pub fn fill_hole(&mut self, site_id: &str, ordinal: u32, code: String) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        let Some(request_id) = self.pending.iter().find_map(|(id, p)| {
            (p.site_id == site_id
                && p.ordinal == ordinal
                && matches!(p.request, PendingReq::HoleResolve))
            .then_some(*id)
        }) else {
            return actions;
        };
        if let Some(pending) = self.pending.remove(&request_id) {
            // 交付形状 = `{"body": code}`：guest 的 hole() 读 `answer.body`
            // 再 evalFill。journal outcome 的 `result` 键必须同形——重放经
            // `outcome_of` 取 `result` 交付。
            let record = JournalRecord {
                kind: RecordKind::Hole,
                site_id: pending.site_id.clone(),
                ordinal: pending.ordinal,
                input_hash: input_hash(&json!(code)),
                settle: self.settle,
                outcome: json!({"status": "filled", "code": code, "result": {"body": code}}),
                extra: Map::new(),
            };
            self.settle += 1;
            let _ = self.store.append(&record);
            if let Some(name) = self.open_holes.remove(&(pending.site_id, pending.ordinal)) {
                actions.push(EngineAction::Session(SessionEvent::HoleCancelled { name }));
            }
            actions.push(EngineAction::Complete {
                request_id,
                outcome: DriverOutcome::Completed(json!({"body": code})),
            });
        }
        actions
    }
}
