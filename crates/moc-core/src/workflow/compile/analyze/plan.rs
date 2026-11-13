//! 全局校验、依赖边与降级装配。

use super::super::CompiledWorkflow;
use super::super::Diagnostic;
use super::super::EdgeKind;
use super::super::ModelSource;
use super::super::PhaseKind;
use super::super::PhaseNode;
use super::super::PlanEdge;
use super::super::Site;
use super::super::SiteKind;
use super::super::WorkflowPlan;
use super::super::line_col;
use super::super::schema;
use super::CALLABLES;
use super::ES2022_GLOBALS;
use super::OBJECTS;
use super::TYPE_NAMES;
use super::Trace;
use super::VALUES;
use super::Walker;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;
use oxc_span::Span;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

// —— 后置：全局检查、相位传播、边推导、降级装配 ——

impl<'a> Walker<'a> {
    /// 未解析引用的全局白名单检查（FORBIDDEN_GLOBAL）。
    pub(super) fn check_globals(&mut self, scoping: &'a oxc_semantic::Scoping) {
        let mut whitelist: HashSet<&str> = ES2022_GLOBALS.iter().copied().collect();
        whitelist.extend(CALLABLES);
        whitelist.extend(OBJECTS);
        whitelist.extend(VALUES);
        whitelist.extend(TYPE_NAMES);
        whitelist.insert("eval"); // hole fill 注入 (__src)=>eval(__src)
        for (name, refs) in scoping.root_unresolved_references() {
            if whitelist.contains(*name) {
                continue;
            }
            for id in refs {
                let reference = scoping.get_reference(*id);
                if !reference.flags().is_value() {
                    continue;
                }
                let span = self.nodes.get_node(reference.node_id()).kind().span();
                if self.bad_globals.insert(*name) {
                    self.diag(
                        super::super::FORBIDDEN_GLOBAL,
                        format!("`{name}` is not an ES2022 builtin or a workflow facade global"),
                        span,
                    );
                }
                break;
            }
        }
        // 顶层自行声明 args/run 时注入对应 facade 值。
        self.user_owns_args_run = [
            scoping.get_root_binding("args").is_some(),
            scoping.get_root_binding("run").is_some(),
        ];
    }

    /// 相位经调用图传播（fixpoint）：fn_phases[f] = ∪ callsite 相位 ∪ fn_phases[caller]。
    pub(super) fn fn_phases(&self) -> HashMap<SymbolId, BTreeSet<String>> {
        let mut map: HashMap<SymbolId, BTreeSet<String>> = HashMap::new();
        for _ in 0..self.fn_calls.len() + 1 {
            let mut changed = false;
            for (callee, calls) in &self.fn_calls {
                let mut adds: BTreeSet<String> = BTreeSet::new();
                for (_, phase, caller, _) in calls {
                    if let Some(p) = phase {
                        adds.insert(p.clone());
                    }
                    if let Some(caller) = caller
                        && let Some(inherited) = map.get(caller)
                    {
                        adds.extend(inherited.iter().cloned());
                    }
                }
                let entry = map.entry(*callee).or_default();
                for p in adds {
                    if entry.insert(p) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        map
    }

    /// fan-out 经调用图传播：调点在循环内或调用方函数本身 fan-out。
    pub(super) fn fanout_fns(&self) -> HashSet<SymbolId> {
        let mut set: HashSet<SymbolId> = HashSet::new();
        for _ in 0..self.fn_calls.len() + 1 {
            let mut changed = false;
            for (callee, calls) in &self.fn_calls {
                if set.contains(callee) {
                    continue;
                }
                if calls.iter().any(|(call, _, caller, in_loop)| {
                    *in_loop
                        || self
                            .loop_bodies
                            .iter()
                            .any(|b| b.contains_inclusive(call.span))
                        || caller.is_some_and(|c| set.contains(&c))
                }) {
                    set.insert(*callee);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        set
    }

    /// ask receiver 表达式 → 产出它的 agent() 站点 id。
    pub(super) fn agent_site_of(
        &self,
        site_by_start: &HashMap<u32, usize>,
        expr: &'a Expression<'a>,
    ) -> Option<String> {
        let call: &CallExpression = match expr {
            Expression::CallExpression(c) => c,
            Expression::ParenthesizedExpression(p) => match &p.expression {
                Expression::CallExpression(c) => c,
                _ => return None,
            },
            Expression::Identifier(i) => {
                let sym = self.symbol_of(i)?;
                match *self.binding_init.get(&sym)? {
                    Expression::CallExpression(c) => c,
                    _ => return None,
                }
            }
            Expression::AwaitExpression(a) => {
                return self.agent_site_of(site_by_start, &a.argument);
            }
            Expression::TSAsExpression(e) => {
                return self.agent_site_of(site_by_start, &e.expression);
            }
            Expression::TSSatisfiesExpression(e) => {
                return self.agent_site_of(site_by_start, &e.expression);
            }
            Expression::TSTypeAssertion(e) => {
                return self.agent_site_of(site_by_start, &e.expression);
            }
            Expression::TSNonNullExpression(e) => {
                return self.agent_site_of(site_by_start, &e.expression);
            }
            _ => return None,
        };
        if !matches!(&call.callee, Expression::Identifier(i) if i.name == "agent" && self.is_global(i))
        {
            return None;
        }
        site_by_start
            .get(&call.span.start)
            .map(|i| self.sites[*i].id.clone())
    }

    pub(super) fn finish_plan(mut self, source: &str) -> Result<CompiledWorkflow, Vec<Diagnostic>> {
        let site_by_start: HashMap<u32, usize> = self
            .sites
            .iter()
            .enumerate()
            .map(|(i, s)| (s.span.start, i))
            .collect();
        let fn_phases = self.fn_phases();
        let fanout = self.fanout_fns();

        // 相位合成 + PHASE_REQUIRED + fan-out 修正 + phase_sites 重建
        self.phase_sites.clear();
        for site in &mut self.sites {
            let mut phases: BTreeSet<String> = site.phase.iter().cloned().collect();
            if let Some(f) = site.in_fn
                && let Some(inherited) = fn_phases.get(&f)
            {
                phases.extend(inherited.iter().cloned());
            }
            if matches!(site.kind, SiteKind::Ask | SiteKind::WorldRun) && phases.is_empty() {
                self.diagnostics.push({
                    let (line, col) = line_col(source, site.span.start);
                    Diagnostic {
                        code: super::super::PHASE_REQUIRED,
                        message: "this call is not covered by any phase(); every ask and world.run must run inside a phase".into(),
                        line,
                        col,
                    }
                });
            }
            if site.fan_out || site.in_fn.is_some_and(|f| fanout.contains(&f)) {
                site.fan_out = true;
            }
            site.phase = phases.first().cloned();
            for p in &phases {
                self.phase_sites
                    .entry(p.clone())
                    .or_default()
                    .push(site.id.clone());
            }
        }
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        // fan-out 字面量名：经函数传播命中而 agent_call 时未报的补一条。
        // （词法循环内的已在 agent_call 报过。）
        for site in &self.sites {
            if site.kind == SiteKind::Actor
                && site.fan_out
                && let Some(lit) = &site.label
                && !self
                    .loop_bodies
                    .iter()
                    .any(|b| b.contains_inclusive(site.span))
                && self.literal_agents.contains_key(lit)
            {
                let (line, col) = line_col(source, site.span.start);
                self.diagnostics.push(Diagnostic {
                    code: super::super::FANOUT_LITERAL_NAME,
                    message: format!("literal agent name \"{lit}\" inside a fan-out position collides on every iteration"),
                    line,
                    col,
                });
            }
        }
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        // roles 的 fan_out/ask_sites 回填
        for role in &mut self.roles {
            if let Some(site) = self.sites.iter().find(|s| s.id == role.role_id) {
                role.fan_out = site.fan_out;
            }
        }
        for site in &self.sites {
            if site.kind == SiteKind::Ask
                && let Some(recv) = site.receiver
                && let Some(agent_id) = self.agent_site_of(&site_by_start, recv)
            {
                self.ask_sites_by_agent
                    .entry(agent_id)
                    .or_default()
                    .push(site.id.clone());
            }
        }
        for role in &mut self.roles {
            if let Some(asks) = self.ask_sites_by_agent.get(&role.role_id) {
                role.ask_sites = asks.clone();
            }
        }
        // persona.model 的 model() 路由：span → site_id。
        for (role_idx, span) in &self.role_model_spans {
            if let Some(site_idx) = site_by_start.get(&span.start) {
                self.roles[*role_idx].model_source = ModelSource::Routed {
                    refs: vec![self.sites[*site_idx].id.clone()],
                };
            }
        }

        // PHASE_EMPTY：phase 节点须含 ask 或 world.run
        for phase in &self.phases {
            let sites = self.phase_sites.get(&phase.id).cloned().unwrap_or_default();
            let covered = sites.iter().any(|id| {
                self.sites
                    .iter()
                    .find(|s| &s.id == id)
                    .is_some_and(|s| matches!(s.kind, SiteKind::Ask | SiteKind::WorldRun))
            });
            if !covered {
                let span = self
                    .sites
                    .iter()
                    .find(|s| s.kind == SiteKind::Phase && s.label.as_deref() == Some(&phase.name))
                    .map_or(Span::new(0, 0), |s| s.span);
                let (line, col) = line_col(source, span.start);
                self.diagnostics.push(Diagnostic {
                    code: super::super::PHASE_EMPTY,
                    message: format!("phase \"{}\" contains no ask or world.run", phase.name),
                    line,
                    col,
                });
            }
        }
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        // —— 边：def-use 数据边 + 保守 order 边 ——
        let mut edges: Vec<PlanEdge> = Vec::new();
        let mut seen: HashSet<(String, String, EdgeKind)> = HashSet::new();
        let mut tracer = Trace {
            w: &self,
            site_by_start: &site_by_start,
            depth: 0,
            visiting: HashSet::new(),
            sub: Vec::new(),
        };
        let site_phases: HashMap<String, BTreeSet<String>> = self
            .sites
            .iter()
            .map(|s| (s.id.clone(), s.phase.iter().cloned().collect()))
            .collect();
        let phase_first_site: HashMap<String, u32> = {
            let mut m = HashMap::new();
            for (name, sites) in &self.phase_sites {
                let first = sites
                    .iter()
                    .filter_map(|id| self.sites.iter().find(|s| &s.id == id))
                    .map(|s| s.span.start)
                    .min()
                    .unwrap_or(0);
                m.insert(name.clone(), first);
            }
            m
        };
        let mut dep_out: Vec<(BTreeSet<String>, bool)> = Vec::new();
        for site in &self.sites {
            if !matches!(
                site.kind,
                SiteKind::Ask
                    | SiteKind::WorldRun
                    | SiteKind::WorldRead
                    | SiteKind::ArtifactPublish
                    | SiteKind::Hole
            ) {
                dep_out.push((BTreeSet::new(), false));
                continue;
            }
            tracer.visiting.clear();
            let (mut deps, mut opaque) = (BTreeSet::new(), false);
            if let Some(recv) = site.receiver {
                let (d, o) = tracer.expr(recv);
                deps.extend(d);
                opaque |= o;
            }
            for arg in &site.args {
                let (d, o) = tracer.expr(arg);
                deps.extend(d);
                opaque |= o;
            }
            dep_out.push((deps, opaque));
        }
        for (site, (deps, opaque)) in self.sites.iter().zip(dep_out) {
            let to_phases = site_phases.get(&site.id).cloned().unwrap_or_default();
            for dep in deps {
                if dep == site.id {
                    continue;
                }
                for from_phase in site_phases.get(&dep).cloned().unwrap_or_default() {
                    for to_phase in &to_phases {
                        if from_phase != *to_phase
                            && seen.insert((from_phase.clone(), to_phase.clone(), EdgeKind::Data))
                        {
                            edges.push(PlanEdge {
                                from: from_phase.clone(),
                                to: to_phase.clone(),
                                kind: EdgeKind::Data,
                            });
                        }
                    }
                }
            }
            if opaque {
                for to_phase in &to_phases {
                    let my_first = phase_first_site.get(to_phase).copied().unwrap_or(0);
                    for (name, first) in &phase_first_site {
                        if name != to_phase
                            && *first < my_first
                            && seen.insert((name.clone(), to_phase.clone(), EdgeKind::Order))
                        {
                            edges.push(PlanEdge {
                                from: name.clone(),
                                to: to_phase.clone(),
                                kind: EdgeKind::Order,
                            });
                        }
                    }
                }
            }
        }

        // —— schema 合成：ask<T>/hole<T> ——
        let mut schemas: Vec<(String, Value)> = Vec::new();
        for site in &self.sites {
            if matches!(site.kind, SiteKind::Ask | SiteKind::Hole) {
                match site.type_arg {
                    Some(ty) => match schema::synthesize(ty, &self.type_decls, source) {
                        Ok(s) => schemas.push((site.id.clone(), s)),
                        Err(message) => {
                            let (line, col) = line_col(source, ty.span().start);
                            self.diagnostics.push(Diagnostic {
                                code: super::super::SCHEMA_UNREPRESENTABLE,
                                message,
                                line,
                                col,
                            });
                        }
                    },
                    None => {
                        if site.kind == SiteKind::Ask {
                            schemas.push((site.id.clone(), json!({"type": "string"})));
                        }
                    }
                }
            }
        }
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        // —— hole 也作图节点 ——
        let mut phases = std::mem::take(&mut self.phases);
        for phase in &mut phases {
            phase.sites = self.phase_sites.get(&phase.id).cloned().unwrap_or_default();
        }
        for site in &self.sites {
            if site.kind == SiteKind::Hole
                && let Some(name) = &site.label
            {
                phases.push(PhaseNode {
                    id: site.id.clone(),
                    name: name.clone(),
                    kind: PhaseKind::Hole,
                    sites: vec![site.id.clone()],
                });
            }
        }

        let plan = WorkflowPlan {
            sites: self
                .sites
                .iter()
                .map(|s| {
                    let (line, col) = line_col(source, s.span.start);
                    Site {
                        site_id: s.id.clone(),
                        kind: s.kind,
                        phase: s.phase.clone(),
                        line,
                        col,
                        label: s.label.clone(),
                    }
                })
                .collect(),
            phases,
            edges,
            roles: self.roles,
            model_refs: self.model_refs,
            commands: self
                .commands
                .into_iter()
                .map(|(command, sites)| super::super::CommandSite { command, sites })
                .collect(),
            schemas,
        };
        let lowered = super::super::lower::lower(
            source,
            &self.blank,
            &self.inserts,
            &self.user_owns_args_run,
        );
        Ok(CompiledWorkflow { lowered, plan })
    }
}
