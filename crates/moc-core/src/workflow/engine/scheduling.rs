use super::ActorContext;
use super::DriverOutcome;
use super::DriverRequest;
use super::Engine;
use super::EngineAction;
use super::PendingReq;
use super::SessionEvent;
use super::StopReason;
use super::Terminal;
use super::TerminalStatus;
use super::guest_outcome;
use crate::workflow::engine::JournalRecord;
use crate::workflow::engine::RecordKind;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

impl Engine {
    /// 派发一个 pending 请求（ask 计数在飞额度）。
    pub(super) fn dispatch(&mut self, request_id: u64, actions: &mut Vec<EngineAction>) {
        let Some(p) = self.pending.get(&request_id) else {
            return;
        };
        let (site_id, ordinal) = (p.site_id.clone(), p.ordinal);
        let drv = match &p.request {
            PendingReq::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                instructions,
                schema,
            } => {
                let actor_key = (actor_site_id.clone(), *actor_ordinal);
                let context = if let Some(session) = self.actor_sessions.get(&actor_key) {
                    ActorContext::Existing {
                        child_session_id: session.clone(),
                    }
                } else {
                    // amend：具名 actor 已导入过 ask → 用前驱子会话 transcript 播种。
                    self.actors
                        .get(&actor_key)
                        .and_then(|a| a.name.as_ref().zip(a.import.as_ref()))
                        .and_then(|(name, import)| {
                            let session = self.prior_actor_sessions_by_name.get(name)?.clone();
                            // 截断点 = 最后一条已导入 ask 的 user_turns（driver 交付时
                            // 记入 outcome.result；缺省 0 表示不导入任何 user 回合）。
                            let user_turns = import
                                .consumed
                                .checked_sub(1)
                                .and_then(|last| import.entries.get(last))
                                .and_then(|e| e.outcome.pointer("/result/user_turns"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                            Some(ActorContext::SeedFrom {
                                run_id: self.config.resumed_from.clone().unwrap_or_default(),
                                child_session_id: session,
                                user_turns,
                            })
                        })
                        .unwrap_or(ActorContext::Fresh)
                };
                DriverRequest::Ask {
                    request_id,
                    site_id,
                    ordinal,
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                    instructions: instructions.clone(),
                    schema: schema.clone(),
                    context,
                    persona: self
                        .actors
                        .get(&(actor_site_id.clone(), *actor_ordinal))
                        .map(|a| a.persona.clone())
                        .unwrap_or(Value::Null),
                }
            }
            PendingReq::WorldRead { op, args } => DriverRequest::WorldRead {
                request_id,
                site_id,
                ordinal,
                op: op.clone(),
                args: args.clone(),
            },
            PendingReq::WorldRun {
                command,
                args,
                timeout_ms,
                output,
            } => DriverRequest::WorldRun {
                request_id,
                site_id,
                ordinal,
                command: command.clone(),
                args: args.clone(),
                timeout_ms: *timeout_ms,
                output: output.clone(),
            },
            PendingReq::PublishArtifact { member, spec } => DriverRequest::PublishArtifact {
                request_id,
                site_id,
                ordinal,
                member: member.clone(),
                spec: spec.clone(),
            },
            PendingReq::HoleResolve => return, // hole 由 fill_hole 回执，无需 dispatch
        };
        if let Some(key) = drv.key() {
            self.in_flight.insert(key, request_id);
        }
        if matches!(drv, DriverRequest::Ask { .. }) {
            self.in_flight_asks += 1;
        }
        actions.push(EngineAction::Dispatch(drv));
    }

    /// driver 完成一个派发：写 journal（settle 序）、回执 worker、释放额度。
    /// `request_id` 对应 pending；重复 settle 以首个落地为准。
    pub fn settle(&mut self, request_id: u64, outcome: DriverOutcome) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        let Some(pending) = self.pending.remove(&request_id) else {
            return actions;
        };
        if matches!(pending.request, PendingReq::Ask { .. }) && self.in_flight_asks > 0 {
            self.in_flight_asks -= 1;
        }
        self.in_flight
            .remove(&(pending.site_id.clone(), pending.ordinal));
        // 写 journal（ask 记录带 actor 身份供 amend 匹配）。
        let mut extra = Map::new();
        if let PendingReq::Ask {
            actor_site_id,
            actor_ordinal,
            actor_seq,
            instructions,
            schema,
        } = &pending.request
        {
            extra.insert("actor_site_id".into(), json!(actor_site_id));
            extra.insert("actor_ordinal".into(), json!(actor_ordinal));
            extra.insert("actor_seq".into(), json!(actor_seq));
            extra.insert("instructions".into(), json!(instructions));
            if let Some(schema) = schema {
                extra.insert("schema".into(), schema.clone());
            }
            if let Some(actor) = self.actors.get(&(actor_site_id.clone(), *actor_ordinal)) {
                if let Some(name) = &actor.name {
                    extra.insert("actor_name".into(), json!(name));
                }
                extra.insert("persona_key".into(), json!(actor.persona_key));
            }
        }
        let record = JournalRecord {
            kind: pending.kind,
            site_id: pending.site_id.clone(),
            ordinal: pending.ordinal,
            input_hash: pending.input_hash.clone(),
            settle: self.settle,
            outcome: outcome.into_outcome_json(),
            extra,
        };
        self.settle += 1;
        if let Err(e) = self.store.append(&record) {
            actions.push(EngineAction::Session(SessionEvent::Log {
                level: "error".into(),
                message: format!("journal append failed: {e}"),
            }));
        }
        // 记账
        if let DriverOutcome::Completed(v) = &outcome
            && let Some(u) = v.get("usage")
        {
            self.usage.spent_tokens += u.get("spent_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        }
        match (&pending.kind, &outcome) {
            (RecordKind::Ask, DriverOutcome::Completed(_)) => self.usage.asks_completed += 1,
            (RecordKind::Ask, DriverOutcome::Failed { .. }) => self.usage.asks_failed += 1,
            (RecordKind::World, _) => self.usage.world_calls += 1,
            (RecordKind::Artifact, DriverOutcome::Completed(_)) => {
                self.usage.artifacts_published += 1;
            }
            _ => {}
        }
        actions.push(EngineAction::Complete {
            request_id,
            outcome: guest_outcome(pending.kind, outcome),
        });
        // 同 actor FIFO：释放 actor；其队列有下一个 → 走同样的全局逻辑（保持 busy）。
        if let PendingReq::Ask {
            actor_site_id,
            actor_ordinal,
            ..
        } = &pending.request
        {
            let actor_key = (actor_site_id.clone(), *actor_ordinal);
            match self
                .actor_queues
                .get_mut(&actor_key)
                .and_then(|q| q.pop_front())
            {
                Some(next) => {
                    if self
                        .actor_queues
                        .get(&actor_key)
                        .is_some_and(|q| q.is_empty())
                    {
                        self.actor_queues.remove(&actor_key);
                    }
                    self.offer_ask(next, &mut actions);
                }
                None => {
                    self.actor_queues.remove(&actor_key);
                    self.busy_actors.remove(&actor_key);
                }
            }
        }
        // 释放额度后派发排队的 ask（FIFO）。
        while let Some(next) = self.queued_asks.pop_front() {
            let slot_free = self.config.max_concurrency == 0
                || self.in_flight_asks < self.config.max_concurrency;
            if !slot_free {
                self.queued_asks.push_front(next);
                break;
            }
            if let Some(p) = self.pending.get_mut(&next) {
                p.queued = false;
                self.dispatch(next, &mut actions);
            }
        }
        actions
    }

    /// run 终态（先到者胜）。
    pub fn finish(
        &mut self,
        status: TerminalStatus,
        result: Option<Value>,
        code: Option<String>,
        message: Option<String>,
        actions: &mut Vec<EngineAction>,
    ) {
        if self.terminal.is_some() {
            return;
        }
        let terminal = Terminal {
            status,
            result: result.clone(),
            error_code: code.clone(),
            error_message: message.clone(),
        };
        self.terminal = Some(terminal.clone());
        // 脚本返回/终态 → 取消在飞 ask。
        let in_flight: Vec<(u64, String, u32)> = self
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.request, PendingReq::Ask { .. }))
            .map(|(id, p)| (*id, p.site_id.clone(), p.ordinal))
            .collect();
        for (request_id, site_id, ordinal) in in_flight {
            if let Some(p) = self.pending.get(&request_id)
                && let PendingReq::Ask {
                    actor_site_id,
                    actor_ordinal,
                    actor_seq,
                    ..
                } = &p.request
            {
                actions.push(EngineAction::Dispatch(DriverRequest::CancelAsk {
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                }));
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome: DriverOutcome::Failed {
                        code: "CANCELLED".into(),
                        message: "run finished while the ask was in flight".into(),
                    },
                });
            }
            self.in_flight.remove(&(site_id, ordinal));
            // 从 pending 摘除：迟到的 settle 不得二次回执或追加 journal。
            self.pending.remove(&request_id);
        }
        self.queued_asks.clear();
        self.actor_queues.clear();
        self.busy_actors.clear();
        // run 记录
        let record = JournalRecord {
            kind: RecordKind::Run,
            site_id: "run".into(),
            ordinal: 0,
            input_hash: String::new(),
            settle: self.settle,
            outcome: json!({
                "status": match status {
                    TerminalStatus::Completed => "completed",
                    TerminalStatus::Errored => "errored",
                    TerminalStatus::Stopped { .. } => "stopped",
                },
                "stop_reason": match status {
                    TerminalStatus::Stopped { reason } => match reason {
                        StopReason::User => "user",
                        StopReason::Interrupted => "interrupted",
                        StopReason::Provider => "provider",
                    },
                    _ => "",
                },
                "result": result,
                "error_code": code,
                "error_message": message,
                "usage": serde_json::to_value(&self.usage).unwrap_or_default(),
                "resumed_from": self.config.resumed_from,
            }),
            extra: Map::new(),
        };
        self.settle += 1;
        let _ = self.store.append(&record);
        actions.push(EngineAction::Finalized(Box::new(terminal)));
    }

    /// 取消整个 run。
    pub fn cancel(&mut self, code: impl Into<String>) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        self.finish(
            TerminalStatus::Stopped {
                reason: StopReason::User,
            },
            None,
            Some(code.into()),
            Some("run cancelled".into()),
            &mut actions,
        );
        actions
    }

    pub(super) fn account(&mut self, outcome: &Value) {
        if let Some(u) = outcome.get("usage") {
            self.usage.spent_tokens += u.get("spent_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        }
    }
}
