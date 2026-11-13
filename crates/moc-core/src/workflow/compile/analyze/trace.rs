use super::Deps;
use super::Trace;
use super::bound_names;
use super::scoping_symbol;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;
use oxc_span::Span;
use std::collections::BTreeSet;

impl<'w, 'a> Trace<'w, 'a> {
    pub(super) fn site_at_span(&self, span: Span) -> Option<String> {
        self.site_by_start
            .get(&span.start)
            .map(|i| self.w.sites[*i].id.clone())
    }

    pub(super) fn expr(&mut self, expr: &'a Expression<'a>) -> Deps {
        if self.depth > 8 {
            return (BTreeSet::new(), true);
        }
        self.depth += 1;
        let out = self.expr_inner(expr);
        self.depth -= 1;
        out
    }

    pub(super) fn expr_inner(&mut self, expr: &'a Expression<'a>) -> Deps {
        match expr {
            Expression::Identifier(i) => self.ident(i),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::AwaitExpression(a) => self.expr(&a.argument),
            Expression::TSAsExpression(e) => self.expr(&e.expression),
            Expression::TSSatisfiesExpression(e) => self.expr(&e.expression),
            Expression::TSTypeAssertion(e) => self.expr(&e.expression),
            Expression::TSNonNullExpression(e) => self.expr(&e.expression),
            Expression::TSInstantiationExpression(e) => self.expr(&e.expression),
            Expression::StaticMemberExpression(m) => self.expr(&m.object),
            Expression::ComputedMemberExpression(m) => {
                let (a, oa) = self.expr(&m.object);
                let (b, ob) = self.expr(&m.expression);
                (a.union(&b).cloned().collect(), oa || ob)
            }
            Expression::PrivateFieldExpression(m) => self.expr(&m.object),
            Expression::ChainExpression(c) => match &c.expression {
                ChainElement::CallExpression(call) => self.call(call),
                ChainElement::TSNonNullExpression(e) => self.expr(&e.expression),
                other => other
                    .as_member_expression()
                    .map_or((BTreeSet::new(), false), |m| self.expr(m.object())),
            },
            Expression::CallExpression(call) => self.call(call),
            Expression::NewExpression(n) => {
                let mut deps = BTreeSet::new();
                let mut opaque = false;
                for a in &n.arguments {
                    let (d, o) = self.expr(a.to_expression());
                    deps.extend(d);
                    opaque |= o;
                }
                (deps, opaque)
            }
            Expression::TemplateLiteral(t) => self.all(t.expressions.iter()),
            Expression::TaggedTemplateExpression(t) => self.all(t.quasi.expressions.iter()),
            Expression::ArrayExpression(a) => {
                let mut deps = BTreeSet::new();
                let mut opaque = false;
                for el in &a.elements {
                    let e = match el {
                        ArrayExpressionElement::SpreadElement(s) => Some(&s.argument),
                        ArrayExpressionElement::Elision(_) => None,
                        _ => Some(el.to_expression()),
                    };
                    if let Some(e) = e {
                        let (d, o) = self.expr(e);
                        deps.extend(d);
                        opaque |= o;
                    }
                }
                (deps, opaque)
            }
            Expression::ObjectExpression(o) => {
                let mut deps = BTreeSet::new();
                let mut opaque = false;
                for p in &o.properties {
                    let e = match p {
                        ObjectPropertyKind::ObjectProperty(p) => Some(&p.value),
                        ObjectPropertyKind::SpreadProperty(s) => Some(&s.argument),
                    };
                    if let Some(e) = e {
                        let (d, o) = self.expr(e);
                        deps.extend(d);
                        opaque |= o;
                    }
                }
                (deps, opaque)
            }
            Expression::ConditionalExpression(c) => self.all3(&c.test, &c.consequent, &c.alternate),
            Expression::BinaryExpression(b) => self.all2(&b.left, &b.right),
            Expression::LogicalExpression(b) => self.all2(&b.left, &b.right),
            Expression::UnaryExpression(u) => self.expr(&u.argument),
            Expression::AssignmentExpression(a) => self.expr(&a.right),
            Expression::SequenceExpression(s) => self.all(s.expressions.iter()),
            Expression::UpdateExpression(_) => (BTreeSet::new(), false),
            Expression::YieldExpression(y) => y
                .argument
                .as_ref()
                .map_or((BTreeSet::new(), false), |a| self.expr(a)),
            // 函数字面量：被调用时经内联分析处理，本身不携带依赖。
            Expression::FunctionExpression(_)
            | Expression::ArrowFunctionExpression(_)
            | Expression::ClassExpression(_) => (BTreeSet::new(), false),
            _ => (BTreeSet::new(), false),
        }
    }

    pub(super) fn all(&mut self, it: impl Iterator<Item = &'a Expression<'a>>) -> Deps {
        let mut deps = BTreeSet::new();
        let mut opaque = false;
        for e in it {
            let (d, o) = self.expr(e);
            deps.extend(d);
            opaque |= o;
        }
        (deps, opaque)
    }

    pub(super) fn all2(&mut self, a: &'a Expression<'a>, b: &'a Expression<'a>) -> Deps {
        let (d1, o1) = self.expr(a);
        let (d2, o2) = self.expr(b);
        (d1.union(&d2).cloned().collect(), o1 || o2)
    }

    pub(super) fn all3(
        &mut self,
        a: &'a Expression<'a>,
        b: &'a Expression<'a>,
        c: &'a Expression<'a>,
    ) -> Deps {
        let (d1, o1) = self.expr(a);
        let (d2, o2) = self.all2(b, c);
        (d1.union(&d2).cloned().collect(), o1 || o2)
    }

    pub(super) fn ident(&mut self, ident: &'a IdentifierReference<'a>) -> Deps {
        let Some(sym) = scoping_symbol(self.w.scoping, ident) else {
            return (BTreeSet::new(), false); // 全局名（args/run/内建）——值非站点产出
        };
        // 内联形参替换优先。
        if let Some(arg) = self.sub.iter().find(|(s, _)| *s == sym).map(|(_, a)| *a) {
            return self.expr(arg);
        }
        self.symbol(sym)
    }

    pub(super) fn symbol(&mut self, sym: SymbolId) -> Deps {
        if !self.visiting.insert(sym) {
            return (BTreeSet::new(), false); // 循环引用：断开
        }
        let out = self.symbol_inner(sym);
        self.visiting.remove(&sym);
        out
    }

    pub(super) fn symbol_inner(&mut self, sym: SymbolId) -> Deps {
        // for-of/for-await 绑定 → 被迭代表达式（channel：含 send 依赖）
        if let Some(iter) = self.w.iter_source.get(&sym).copied() {
            let (mut deps, mut opaque) = self.expr(iter);
            if let Some(ch_sym) = self.iter_sym_of(iter)
                && let Some(sends) = self.w.channel_sends.get(&ch_sym)
            {
                for send in sends.clone() {
                    for a in &send.arguments {
                        let (d, o) = self.expr(a.to_expression());
                        deps.extend(d);
                        opaque |= o;
                    }
                }
            }
            return (deps, opaque);
        }
        // 形参 → 宿主函数各调用点对应实参
        if let Some((owner, index)) = self.w.param_owner.get(&sym).copied() {
            let calls = self.w.fn_calls.get(&owner).cloned().unwrap_or_default();
            if calls.is_empty() {
                return (BTreeSet::new(), true);
            }
            let mut deps = BTreeSet::new();
            let mut opaque = false;
            for (call, _, _, _) in calls {
                if let Some(a) = call.arguments.get(index) {
                    let (d, o) = self.expr(a.to_expression());
                    deps.extend(d);
                    opaque |= o;
                }
            }
            return (deps, opaque);
        }
        // 变量/解构绑定 → init 表达式
        if let Some(init) = self.w.binding_init.get(&sym).copied() {
            match init {
                Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
                    return (BTreeSet::new(), false);
                }
                _ => return self.expr(init),
            }
        }
        // catch 参数、函数名等：依赖不可知但无害——视作无依赖
        (BTreeSet::new(), false)
    }

    /// 迭代器表达式若解析到 channel 绑定符号，返回该符号（send 追踪键）。
    pub(super) fn iter_sym_of(&self, iter: &'a Expression<'a>) -> Option<SymbolId> {
        let mut expr = iter;
        loop {
            expr = match expr {
                Expression::ParenthesizedExpression(p) => &p.expression,
                Expression::TSAsExpression(e) => &e.expression,
                Expression::TSSatisfiesExpression(e) => &e.expression,
                Expression::TSNonNullExpression(e) => &e.expression,
                _ => break,
            };
        }
        if let Expression::Identifier(i) = expr {
            return scoping_symbol(self.w.scoping, i);
        }
        None
    }

    pub(super) fn call(&mut self, call: &'a CallExpression<'a>) -> Deps {
        // 站点调用 → 数据依赖
        if let Some(id) = self.site_at_span(call.span) {
            return ([id].into_iter().collect(), false);
        }
        // 本地函数 → 内联返回值分析
        if let Expression::Identifier(callee) = &call.callee
            && let Some(sym) = scoping_symbol(self.w.scoping, callee)
        {
            if let Some(f) = self.w.fn_decl.get(&sym).copied() {
                return self.inline_fn(f, call, sym);
            }
            if let Some(Expression::ArrowFunctionExpression(f)) = self.w.binding_init.get(&sym) {
                return self.inline_arrow(f, call);
            }
            if let Some(Expression::FunctionExpression(f)) = self.w.binding_init.get(&sym) {
                return self.inline_fn(f, call, sym);
            }
        }
        // 其他调用：结果不可追踪——但实参里的依赖仍要带出
        let mut deps = BTreeSet::new();
        for a in &call.arguments {
            let (d, _) = self.expr(a.to_expression());
            deps.extend(d);
        }
        if let Some(m) = call.callee.as_member_expression() {
            let (d, _) = self.expr(m.object());
            deps.extend(d);
        }
        (deps, true)
    }

    pub(super) fn inline_fn(
        &mut self,
        func: &'a Function<'a>,
        call: &'a CallExpression<'a>,
        sym: SymbolId,
    ) -> Deps {
        if !self.visiting.insert(sym) {
            return (BTreeSet::new(), false);
        }
        let out = (|| {
            let Some(body) = &func.body else {
                return (BTreeSet::new(), true);
            };
            // 形参 → 实参替换关系
            let mut sub: Vec<(SymbolId, &'a Expression<'a>)> = Vec::new();
            for (index, param) in func.params.items.iter().enumerate() {
                if let Some(arg) = call.arguments.get(index) {
                    for name in bound_names(&param.pattern) {
                        if let Some(psym) = name.symbol_id.get() {
                            sub.push((psym, arg.to_expression()));
                        }
                    }
                }
            }
            let mut deps = BTreeSet::new();
            let mut opaque = false;
            let saved = std::mem::take(&mut self.sub);
            self.sub = sub;
            for stmt in &body.statements {
                collect_returns(stmt, &mut |e| {
                    let (d, o) = self.expr(e);
                    deps.extend(d);
                    opaque |= o;
                });
            }
            self.sub = saved;
            (deps, opaque)
        })();
        self.visiting.remove(&sym);
        out
    }

    pub(super) fn inline_arrow(
        &mut self,
        f: &'a ArrowFunctionExpression<'a>,
        call: &'a CallExpression<'a>,
    ) -> Deps {
        let mut sub: Vec<(SymbolId, &'a Expression<'a>)> = Vec::new();
        for (index, param) in f.params.items.iter().enumerate() {
            if let Some(arg) = call.arguments.get(index) {
                for name in bound_names(&param.pattern) {
                    if let Some(psym) = name.symbol_id.get() {
                        sub.push((psym, arg.to_expression()));
                    }
                }
            }
        }
        let mut deps = BTreeSet::new();
        let mut opaque = false;
        let saved = std::mem::take(&mut self.sub);
        self.sub = sub;
        for stmt in &f.body.statements {
            collect_returns(stmt, &mut |e| {
                let (d, o) = self.expr(e);
                deps.extend(d);
                opaque |= o;
            });
        }
        self.sub = saved;
        (deps, opaque)
    }
}

/// 收集函数体内的 return 实参表达式（不进嵌套函数）。
pub(super) fn collect_returns<'a>(stmt: &'a Statement<'a>, f: &mut impl FnMut(&'a Expression<'a>)) {
    match stmt {
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                f(a);
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                collect_returns(s, f);
            }
        }
        Statement::IfStatement(s) => {
            collect_returns(&s.consequent, f);
            if let Some(a) = &s.alternate {
                collect_returns(a, f);
            }
        }
        Statement::TryStatement(s) => {
            for st in &s.block.body {
                collect_returns(st, f);
            }
            if let Some(h) = &s.handler {
                for st in &h.body.body {
                    collect_returns(st, f);
                }
            }
            if let Some(fin) = &s.finalizer {
                for st in &fin.body {
                    collect_returns(st, f);
                }
            }
        }
        Statement::SwitchStatement(s) => {
            for c in &s.cases {
                for st in &c.consequent {
                    collect_returns(st, f);
                }
            }
        }
        Statement::ForStatement(s) => collect_returns(&s.body, f),
        Statement::ForInStatement(s) => collect_returns(&s.body, f),
        Statement::ForOfStatement(s) => collect_returns(&s.body, f),
        Statement::WhileStatement(s) => collect_returns(&s.body, f),
        Statement::DoWhileStatement(s) => collect_returns(&s.body, f),
        Statement::LabeledStatement(s) => collect_returns(&s.body, f),
        _ => {}
    }
}
