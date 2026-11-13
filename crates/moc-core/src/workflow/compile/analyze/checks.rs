//! 工作流 facade 静态检查。

use super::super::ModelRef;
use super::super::ModelSource;
use super::super::Role;
use super::super::SiteKind;
use super::super::line_col;
use super::ARTIFACT_MEMBERS;
use super::CALLABLES;
use super::FILES_MEMBERS;
use super::GIT_MEMBERS;
use super::OBJECTS;
use super::SiteRec;
use super::Walker;
use super::literal_string;
use super::name_pattern_of;
use super::prop_key_name;
use super::receiver_label;
use super::truncate_chars;
use super::valid_artifact_id;
use crate::workflow::facade;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use oxc_span::Span;

impl<'a> Walker<'a> {
    /// `(` 位置：callee 之后第一个 `(`（跳过 `?.` 与类型实参）。
    pub(super) fn open_paren(&self, call: &'a CallExpression<'a>) -> Option<u32> {
        let bytes = self.source.as_bytes();
        let mut i = call.callee.span().end as usize;
        let end = call
            .arguments
            .first()
            .map_or(call.span.end as usize, |a| a.span().start as usize);
        while i < end {
            if bytes[i] == b'(' {
                return Some(i as u32);
            }
            i += 1;
        }
        None
    }

    pub(super) fn arg_exprs<'b>(&self, call: &'b CallExpression<'a>) -> Vec<&'b Expression<'a>> {
        call.arguments.iter().map(|a| a.to_expression()).collect()
    }

    pub(super) fn add_site(
        &mut self,
        kind: SiteKind,
        call: &'a CallExpression<'a>,
        label: Option<String>,
        receiver: Option<&'a Expression<'a>>,
        type_arg: Option<&'a TSType<'a>>,
    ) -> String {
        let id = self.next_site_id(kind);
        self.sites.push(SiteRec {
            id: id.clone(),
            kind,
            span: call.span,
            phase: self.phase.clone(),
            label,
            args: self.arg_exprs(call),
            receiver,
            type_arg,
            fan_out: self.in_loop,
            in_fn: self.fn_stack.last().copied(),
        });
        if let Some(phase) = &self.phase {
            self.phase_sites
                .entry(phase.clone())
                .or_default()
                .push(id.clone());
        }
        id
    }

    pub(super) fn call(&mut self, call: &'a CallExpression<'a>) {
        if let Some(t) = &call.type_arguments {
            self.blank.push(t.span);
        }
        let args = self.arg_exprs(call);
        // —— callee 分类 ——
        match &call.callee {
            Expression::Identifier(callee) => {
                let name = callee.name.as_str();
                if self.is_global(callee) && CALLABLES.contains(&name) {
                    self.global_call(call, name, &args);
                } else {
                    // 本地函数调用：记录 call site（内联分析/fan-out/相位传播用）。
                    if let Some(sym) = self.symbol_of(callee) {
                        self.fn_calls.entry(sym).or_default().push((
                            call,
                            self.phase.clone(),
                            self.fn_stack.last().copied(),
                            self.in_loop,
                        ));
                    }
                    self.walk_expr(&call.callee);
                    self.walk_exprs(&args);
                }
            }
            Expression::StaticMemberExpression(m) => self.member_call(call, m, &args),
            Expression::ComputedMemberExpression(m) => {
                self.computed_member_call(call, m, &args);
            }
            _ => {
                self.walk_expr(&call.callee);
                self.walk_exprs(&args);
            }
        }
    }

    /// 顶层 facade 可调用名的直接调用。
    pub(super) fn global_call(
        &mut self,
        call: &'a CallExpression<'a>,
        name: &'a str,
        args: &[&'a Expression<'a>],
    ) {
        let open = self.open_paren(call);
        if let Expression::Identifier(callee) = &call.callee {
            self.rewrite_callee(callee);
        }
        match name {
            "agent" => self.agent_call(call, args, open),
            "model" => self.model_call(call, args, open),
            "phase" => {
                // 到达这里说明 phase() 不在独立语句位（marker 已在 stmt 层拦下）。
                self.diag(
                    super::super::INDIRECT_FACADE,
                    "`phase(name)` must be a standalone statement",
                    call.span,
                );
                self.walk_exprs(args);
            }
            "log" => {
                let id = self.add_site(SiteKind::Log, call, None, None, None);
                self.insert_at(open, &id);
                self.walk_exprs(args);
            }
            "report" => {
                let label = args.get(1).and_then(|a| literal_string(a));
                let id = self.add_site(SiteKind::Report, call, label, None, None);
                self.insert_at(open, &id);
                self.walk_exprs(args);
            }
            "channel" => {
                let label = args.first().and_then(|a| literal_string(a));
                let id = self.add_site(SiteKind::Channel, call, label, None, None);
                self.insert_at(open, &id);
                self.walk_exprs(args);
            }
            "future" => {
                let id = self.add_site(SiteKind::Future, call, None, None, None);
                self.insert_at(open, &id);
                // future 体立即在当前 phase 下开跑：函数体继承外层 phase 覆盖。
                for a in args {
                    self.walk_deferred(a);
                }
            }
            "hole" => self.hole_call(call, args, open),
            _ => self.walk_exprs(args),
        }
    }

    pub(super) fn insert_at(&mut self, open: Option<u32>, site_id: &str) {
        if let Some(open) = open {
            self.inserts
                .push((open + 1, "site", format!("\"{site_id}\", ")));
        }
    }

    pub(super) fn walk_exprs(&mut self, args: &[&'a Expression<'a>]) {
        for a in args {
            self.walk_expr(a);
        }
    }

    /// `future(...)`/`hole(...)` 的函数体实参：体内语句继承外层 phase 覆盖
    ///（体在派发点异步开跑，属同一 phase 边界）。
    pub(super) fn walk_deferred(&mut self, expr: &'a Expression<'a>) {
        match expr {
            Expression::ArrowFunctionExpression(f) => {
                self.register_params_farrow(f);
                self.block_depth += 1;
                self.walk_stmts(&f.body.statements);
                self.block_depth -= 1;
            }
            Expression::FunctionExpression(f) => self.walk_function(f),
            other => self.walk_expr(other),
        }
    }

    pub(super) fn agent_call(
        &mut self,
        call: &'a CallExpression<'a>,
        args: &[&'a Expression<'a>],
        open: Option<u32>,
    ) {
        let name_arg = args.first().copied();
        let persona = args.get(1).copied();
        let literal_name = name_arg.and_then(|a| literal_string(a));
        let id = self.add_site(
            SiteKind::Actor,
            call,
            literal_name
                .clone()
                .or_else(|| name_arg.and_then(|a| name_pattern_of(a))),
            None,
            None,
        );
        self.insert_at(open, &id);

        // DUP_AGENT_NAME / FANOUT_LITERAL_NAME
        let fan_out = self.sites.last().unwrap().fan_out;
        if let Some(name) = &literal_name {
            if fan_out {
                self.diag(super::super::FANOUT_LITERAL_NAME, format!("literal agent name \"{name}\" inside a fan-out position collides on every iteration"), args[0].span());
            } else if let Some(first) = self.literal_agents.get(name) {
                self.diag(
                    super::super::DUP_AGENT_NAME,
                    format!("duplicate agent name \"{name}\" (first declared at {first})"),
                    args[0].span(),
                );
            } else {
                self.literal_agents.insert(name.clone(), id.clone());
            }
        }

        // persona → model_source / persona_excerpt / DUP 之后的 role
        let (model_source, persona_excerpt, routed_span) = self.persona_of(persona, name_arg, args);
        self.roles.push(Role {
            role_id: id.clone(),
            name_pattern: name_arg.and_then(|a| name_pattern_of(a)),
            fan_out,
            persona_excerpt,
            model_source,
            ask_sites: Vec::new(),
        });
        if let Some(span) = routed_span {
            self.role_model_spans.push((self.roles.len() - 1, span));
        }
        self.walk_exprs(args);
    }

    pub(super) fn persona_of(
        &mut self,
        persona: Option<&'a Expression<'a>>,
        name: Option<&'a Expression<'a>>,
        args: &[&'a Expression<'a>],
    ) -> (ModelSource, Option<String>, Option<Span>) {
        let mut excerpt = None;
        let mut model_source = ModelSource::Inherit;
        let mut routed_span = None;
        let model_expr = match persona {
            Some(Expression::StringLiteral(s)) => {
                excerpt = Some(truncate_chars(&s.value, 200));
                None
            }
            Some(Expression::ObjectExpression(o)) => {
                let mut model = None;
                for prop in &o.properties {
                    if let ObjectPropertyKind::ObjectProperty(p) = prop {
                        let key = prop_key_name(&p.key);
                        match key.as_deref() {
                            Some("system") => {
                                if let Some(s) = literal_string(&p.value) {
                                    excerpt = Some(truncate_chars(&s, 200));
                                }
                            }
                            Some("model") => model = Some(&p.value),
                            _ => {}
                        }
                    }
                }
                model
            }
            _ => None,
        };
        if let Some(model) = model_expr {
            match model {
                Expression::StringLiteral(s) => {
                    if s.value.is_empty() {
                        self.diag(
                            super::super::NON_LITERAL_MODEL,
                            "persona.model must be a non-empty literal or model()",
                            model.span(),
                        );
                    } else {
                        self.check_model(&s.value, model.span());
                        model_source = ModelSource::Literal {
                            model: s.value.to_string(),
                        };
                    }
                }
                // model(...) 或绑到 model() 的标识符 → 路由到 ModelRef；
                // span → site_id 在 finish_plan 统一解析（此时 model() 站点尚未登记）。
                _ if self.model_ref_span(model).is_some() => {
                    model_source = ModelSource::Routed { refs: Vec::new() };
                    routed_span = self.model_ref_span(model);
                }
                _ => {
                    self.diag(
                        super::super::NON_LITERAL_MODEL,
                        "persona.model must be a literal, a model() ModelRef, or a literal union",
                        model.span(),
                    );
                }
            }
        }
        let _ = name;
        let _ = args;
        (model_source, excerpt, routed_span)
    }

    /// 表达式若（直接或经变量绑定）是 `model(...)` 调用，返回其 span。
    pub(super) fn model_ref_span(&self, expr: &'a Expression<'a>) -> Option<Span> {
        let call = match expr {
            Expression::CallExpression(c) => Some(c),
            Expression::ParenthesizedExpression(p) => match &p.expression {
                Expression::CallExpression(c) => Some(c),
                _ => None,
            },
            Expression::Identifier(i) => match self
                .symbol_of(i)
                .and_then(|sym| self.binding_init.get(&sym).copied())
            {
                Some(Expression::CallExpression(c)) => Some(c),
                _ => None,
            },
            _ => None,
        }?;
        if matches!(&call.callee, Expression::Identifier(c) if c.name == "model" && self.is_global(c))
        {
            Some(call.span)
        } else {
            None
        }
    }

    pub(super) fn check_model(&mut self, id: &str, span: Span) {
        if let Some(exists) = self.options.model_exists
            && !exists(id)
        {
            self.diag(
                super::super::UNKNOWN_MODEL,
                format!("model \"{id}\" is not in the moc model catalog"),
                span,
            );
        }
    }

    pub(super) fn model_call(
        &mut self,
        call: &'a CallExpression<'a>,
        args: &[&'a Expression<'a>],
        open: Option<u32>,
    ) {
        let id = self.add_site(
            SiteKind::Model,
            call,
            args.first().and_then(|a| literal_string(a)),
            None,
            None,
        );
        self.insert_at(open, &id);
        match args.first().and_then(|a| literal_string(a)) {
            Some(model) if !model.is_empty() => {
                self.check_model(&model, args[0].span());
                self.model_refs.push(ModelRef {
                    ref_id: id,
                    model_id: model,
                });
            }
            _ => self.diag(
                super::super::NON_LITERAL_MODEL,
                "model() requires a non-empty literal id",
                args.first().map_or(call.span, |a| a.span()),
            ),
        }
        self.walk_exprs(args);
    }

    pub(super) fn hole_call(
        &mut self,
        call: &'a CallExpression<'a>,
        args: &[&'a Expression<'a>],
        open: Option<u32>,
    ) {
        // hole 名必须非空字面量 ≤128。
        let name = args.first().and_then(|a| literal_string(a));
        match &name {
            Some(n) if n.is_empty() => {
                self.diag(
                    super::super::NON_LITERAL_NAME,
                    "hole name must be a non-empty literal",
                    args[0].span(),
                );
            }
            Some(n) if n.chars().count() > facade::NAME_MAX_LEN => {
                self.diag(
                    super::super::NON_LITERAL_NAME,
                    format!("hole name exceeds {} characters", facade::NAME_MAX_LEN),
                    args[0].span(),
                );
            }
            Some(n) => {
                if let Some(first) = self.hole_names.get(n) {
                    let (line, col) = line_col(self.source, first.start);
                    self.diag(super::super::HOLE_NAME_DUP, format!("hole name \"{n}\" conflicts with a hole or phase declared at L{line}:C{col}"), args[0].span());
                } else {
                    self.hole_names.insert(n.clone(), args[0].span());
                }
                self.phase_sites.entry(format!("hole:{n}")).or_default();
            }
            None => {
                self.diag(
                    super::super::NON_LITERAL_NAME,
                    "hole name must be a non-empty literal",
                    args.first().map_or(call.span, |a| a.span()),
                );
            }
        }
        let type_arg = call.type_arguments.as_ref().and_then(|t| t.params.first());
        let id = self.add_site(SiteKind::Hole, call, name.clone(), None, type_arg);
        self.insert_at(open, &id);
        // 注入 evaluate 实参：(__src)=>eval(__src)，在 prompt 位之后、body 之前。
        let body_arg = args.last().filter(|a| {
            matches!(
                a,
                Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
            )
        });
        if let Some(body) = body_arg {
            if args.len() == 2 {
                self.inserts.push((
                    body.span().start,
                    "hole",
                    "undefined, (__src)=>eval(__src), ".into(),
                ));
            } else {
                self.inserts
                    .push((body.span().start, "hole", "(__src)=>eval(__src), ".into()));
            }
        } else {
            let close = call.span.end - 1;
            if args.len() >= 2 {
                self.inserts
                    .push((close, "hole", ", (__src)=>eval(__src)".into()));
            } else {
                self.inserts
                    .push((close, "hole", ", undefined, (__src)=>eval(__src)".into()));
            }
        }
        self.walk_exprs(args);
    }

    pub(super) fn member_call(
        &mut self,
        call: &'a CallExpression<'a>,
        m: &'a StaticMemberExpression<'a>,
        args: &[&'a Expression<'a>],
    ) {
        let member = m.property.name.as_str();
        // `x.ask(...)`：receiver 可解析到 agent() 产出 → ask 站点。
        if member == "ask" {
            let type_arg = call.type_arguments.as_ref().and_then(|t| t.params.first());
            if self.resolves_to_agent(&m.object) {
                let label = receiver_label(&m.object);
                let id = self.add_site(SiteKind::Ask, call, label, Some(&m.object), type_arg);
                self.insert_at(self.open_paren(call), &id);
                self.walk_expr(&m.object);
                self.walk_exprs(args);
                return;
            }
            // 普通方法调用：非 facade。
            self.walk_expr(&call.callee);
            self.walk_exprs(args);
            return;
        }
        // facade 对象成员调用。
        if let Expression::Identifier(base) = &m.object
            && self.is_global(base)
        {
            let base_name = base.name.as_str();
            match base_name {
                "world" if member == "run" => {
                    self.world_run_call(call, args);
                    return;
                }
                "files" if FILES_MEMBERS.contains(&member) => {
                    let id = self.add_site(
                        SiteKind::WorldRead,
                        call,
                        Some(format!("files.{member}")),
                        None,
                        None,
                    );
                    self.insert_at(self.open_paren(call), &id);
                    self.rewrite_member_base(base);
                    self.walk_exprs(args);
                    return;
                }
                "git" if GIT_MEMBERS.contains(&member) => {
                    let id = self.add_site(
                        SiteKind::WorldRead,
                        call,
                        Some(format!("git.{member}")),
                        None,
                        None,
                    );
                    self.insert_at(self.open_paren(call), &id);
                    self.rewrite_member_base(base);
                    self.walk_exprs(args);
                    return;
                }
                "artifact" if ARTIFACT_MEMBERS.contains(&member) => {
                    self.artifact_call(call, member, args, base);
                    return;
                }
                "world" | "files" | "git" | "artifact" => {
                    self.diag(
                        super::super::INDIRECT_FACADE,
                        format!("`{base_name}.{member}` is not a workflow facade member"),
                        m.span,
                    );
                    return;
                }
                _ => {}
            }
        }
        // 普通成员调用；channel send 记录（send→for-await 边）。
        if member == "send"
            && let Expression::Identifier(obj) = &m.object
            && let Some(sym) = self.symbol_of(obj)
        {
            self.channel_sends.entry(sym).or_default().push(call);
        }
        self.walk_expr(&call.callee);
        self.walk_exprs(args);
    }

    pub(super) fn computed_member_call(
        &mut self,
        call: &'a CallExpression<'a>,
        m: &'a ComputedMemberExpression<'a>,
        args: &[&'a Expression<'a>],
    ) {
        // files["read"](…) 等 computed 访问不属直接调用形态 → INDIRECT_FACADE。
        if let Expression::Identifier(base) = &m.object
            && self.is_global(base)
            && OBJECTS.contains(&base.name.as_str())
        {
            self.diag(
                super::super::INDIRECT_FACADE,
                format!("`{}` member calls must use direct dot syntax", base.name),
                m.span,
            );
            return;
        }
        self.walk_expr(&call.callee);
        self.walk_exprs(args);
    }

    /// `__wf.` 前缀：改基对象文本（files/git/world/artifact → __wf.*）。
    pub(super) fn rewrite_member_base(&mut self, base: &'a IdentifierReference<'a>) {
        self.inserts.push((base.span.start, "base", "__wf.".into()));
    }

    pub(super) fn world_run_call(
        &mut self,
        call: &'a CallExpression<'a>,
        args: &[&'a Expression<'a>],
    ) {
        let cmd = args.first().and_then(|a| literal_string(a));
        let id = self.add_site(SiteKind::WorldRun, call, cmd.clone(), None, None);
        self.insert_at(self.open_paren(call), &id);
        if let Expression::StaticMemberExpression(m) = &call.callee
            && let Expression::Identifier(base) = &m.object
        {
            self.rewrite_member_base(base);
        }
        match cmd {
            Some(cmd) if !cmd.is_empty() => {
                self.commands.entry(cmd).or_default().push(id);
            }
            _ => self.diag(
                super::super::NON_LITERAL_COMMAND,
                "world.run's first argument must be a compile-time string literal",
                args.first().map_or(call.span, |a| a.span()),
            ),
        }
        self.walk_exprs(args);
    }

    pub(super) fn artifact_call(
        &mut self,
        call: &'a CallExpression<'a>,
        member: &'a str,
        args: &[&'a Expression<'a>],
        base: &'a IdentifierReference<'a>,
    ) {
        static MEMBER: &[(&str, &str)] = &[
            ("file", "file"),
            ("markdown", "markdown"),
            ("chart", "chart"),
            ("table", "table"),
            ("metrics", "metrics"),
            ("board", "board"),
        ];
        let content = matches!(member, "file" | "markdown");
        let id = self.add_site(
            if content {
                SiteKind::ArtifactPublish
            } else {
                SiteKind::ArtifactDecl
            },
            call,
            args.first().and_then(|a| literal_string(a)),
            None,
            None,
        );
        self.insert_at(self.open_paren(call), &id);
        self.rewrite_member_base(base);
        // ARTIFACT_ID：id 字面量 + 每 id 单成员 + 预置类顶层声明。
        match args.first().and_then(|a| literal_string(a)) {
            Some(id_str) => {
                if !valid_artifact_id(&id_str) {
                    self.diag(
                        super::super::ARTIFACT_ID,
                        format!("artifact id \"{id_str}\" must match [A-Za-z0-9_.-]{{1,64}}"),
                        args[0].span(),
                    );
                } else {
                    let member_name = MEMBER.iter().find(|(m, _)| *m == member).unwrap().1;
                    if let Some(prev) = self.artifact_members.get(&id_str) {
                        if *prev != member_name {
                            self.diag(super::super::ARTIFACT_ID, format!("artifact id \"{id_str}\" is used with both `{prev}` and `{member_name}`"), args[0].span());
                        }
                    } else {
                        self.artifact_members.insert(id_str, member_name);
                    }
                }
                if !content && !self.is_top_level() {
                    self.diag(
                        super::super::ARTIFACT_ID,
                        format!(
                            "preset artifact `{member}` declarations must be top-level statements"
                        ),
                        call.span,
                    );
                }
            }
            None => self.diag(
                super::super::ARTIFACT_ID,
                "artifact id must be a compile-time literal",
                args.first().map_or(call.span, |a| a.span()),
            ),
        }
        self.walk_exprs(args);
    }

    /// 预置 artifact 只允许顶层语句声明。
    pub(super) fn is_top_level(&self) -> bool {
        self.block_depth == 0 && self.fn_stack.is_empty()
    }

    /// 顶层 facade 调用名 → `__wf.<name>(...)`。
    pub(super) fn rewrite_callee(&mut self, callee: &'a IdentifierReference<'a>) {
        self.inserts
            .push((callee.span.start, "base", "__wf.".into()));
    }

    /// `x?.y()` 链：callee 或 member。
    pub(super) fn walk_chain(&mut self, el: &'a ChainElement<'a>) {
        match el {
            ChainElement::CallExpression(call) => self.call(call),
            ChainElement::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            other => {
                if let Some(m) = other.as_member_expression() {
                    self.walk_expr(m.object());
                }
            }
        }
    }

    /// `x` 是否解析为 agent() 调用产出（ask receiver 判定）。
    pub(super) fn resolves_to_agent(&self, expr: &'a Expression<'a>) -> bool {
        match expr {
            Expression::CallExpression(c) => {
                matches!(&c.callee, Expression::Identifier(i) if i.name == "agent" && self.is_global(i))
            }
            Expression::ParenthesizedExpression(p) => self.resolves_to_agent(&p.expression),
            Expression::Identifier(i) => {
                if let Some(sym) = self.symbol_of(i) {
                    matches!(self.binding_init.get(&sym), Some(Expression::CallExpression(c)) if matches!(&c.callee, Expression::Identifier(callee) if callee.name == "agent"))
                } else {
                    false
                }
            }
            Expression::AwaitExpression(a) => self.resolves_to_agent(&a.argument),
            Expression::TSAsExpression(e) => self.resolves_to_agent(&e.expression),
            Expression::TSSatisfiesExpression(e) => self.resolves_to_agent(&e.expression),
            Expression::TSTypeAssertion(e) => self.resolves_to_agent(&e.expression),
            Expression::TSNonNullExpression(e) => self.resolves_to_agent(&e.expression),
            _ => false,
        }
    }
}
