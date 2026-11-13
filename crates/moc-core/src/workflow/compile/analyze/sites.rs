//! 调用站点与相位分析。

use super::BANNED_MEMBERS;
use super::CALLABLES;
use super::OBJECTS;
use super::Walker;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use oxc_span::Span;

impl<'a> Walker<'a> {
    pub(super) fn walk_expr(&mut self, expr: &'a Expression<'a>) {
        match expr {
            Expression::CallExpression(call) => self.call(call),
            Expression::ParenthesizedExpression(p) => self.walk_expr(&p.expression),
            Expression::AwaitExpression(a) => self.walk_expr(&a.argument),
            Expression::Identifier(ident) => self.ident(ident),
            Expression::StaticMemberExpression(m) => {
                self.member(&m.object, &m.property, m.span, true)
            }
            Expression::ComputedMemberExpression(m) => {
                self.member_object_only(&m.object, m.span);
                self.walk_expr(&m.expression);
            }
            Expression::PrivateFieldExpression(m) => self.walk_expr(&m.object),
            Expression::AssignmentExpression(a) => {
                self.walk_at(&a.left);
                self.walk_expr(&a.right);
            }
            Expression::BinaryExpression(b) => {
                self.walk_expr(&b.left);
                self.walk_expr(&b.right);
            }
            Expression::LogicalExpression(b) => {
                self.walk_expr(&b.left);
                self.walk_expr(&b.right);
            }
            Expression::UnaryExpression(u) => self.walk_expr(&u.argument),
            Expression::UpdateExpression(u) => {
                self.walk_assignment_target(&u.argument);
            }
            Expression::ConditionalExpression(c) => {
                self.walk_expr(&c.test);
                self.walk_expr(&c.consequent);
                self.walk_expr(&c.alternate);
            }
            Expression::SequenceExpression(s) => {
                for e in &s.expressions {
                    self.walk_expr(e);
                }
            }
            Expression::ArrayExpression(a) => {
                for el in &a.elements {
                    match el {
                        ArrayExpressionElement::SpreadElement(s) => self.walk_expr(&s.argument),
                        ArrayExpressionElement::Elision(_) => {}
                        _ => self.walk_expr(el.to_expression()),
                    }
                }
            }
            Expression::ObjectExpression(o) => {
                for prop in &o.properties {
                    match prop {
                        ObjectPropertyKind::ObjectProperty(p) => {
                            if let PropertyKey::Identifier(e) = &p.key {
                                self.ident(e);
                            }
                            self.walk_expr(&p.value);
                        }
                        ObjectPropertyKind::SpreadProperty(s) => self.walk_expr(&s.argument),
                    }
                }
            }
            Expression::TemplateLiteral(t) => {
                for e in &t.expressions {
                    self.walk_expr(e);
                }
            }
            Expression::TaggedTemplateExpression(t) => {
                self.walk_expr(&t.tag);
                for e in &t.quasi.expressions {
                    self.walk_expr(e);
                }
            }
            Expression::NewExpression(n) => {
                // `new Date()` 无参也是非确定源。
                if let Expression::Identifier(callee) = &n.callee
                    && callee.name == "Date"
                    && self.is_global(callee)
                    && n.arguments.is_empty()
                {
                    self.diag(
                        super::super::FORBIDDEN_GLOBAL,
                        "new Date() without arguments is non-deterministic",
                        n.span,
                    );
                }
                if let Some(t) = &n.type_arguments {
                    self.blank.push(t.span);
                }
                self.walk_expr(&n.callee);
                for a in &n.arguments {
                    self.walk_expr(a.to_expression());
                }
            }
            Expression::FunctionExpression(f) => self.walk_function(f),
            Expression::ArrowFunctionExpression(f) => {
                self.register_params_farrow(f);
                let saved_phase = self.phase.take();
                self.block_depth += 1;
                self.walk_stmts(&f.body.statements);
                self.block_depth -= 1;
                self.phase = saved_phase;
            }
            Expression::ClassExpression(c) => self.walk_class(c),
            Expression::ChainExpression(c) => self.walk_chain(&c.expression),
            Expression::ImportExpression(i) => {
                self.diag(
                    super::super::FORBIDDEN_SYNTAX,
                    "workflow scripts cannot use import/export/declare",
                    i.span,
                );
            }
            Expression::YieldExpression(y) => {
                if let Some(a) = &y.argument {
                    self.walk_expr(a);
                }
            }
            Expression::TSAsExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSSatisfiesExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSTypeAssertion(e) => {
                self.blank
                    .push(Span::new(e.span.start, e.expression.span().start));
                self.walk_expr(&e.expression);
            }
            Expression::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSInstantiationExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            _ => {}
        }
    }

    pub(super) fn register_params_farrow(&mut self, f: &'a ArrowFunctionExpression<'a>) {
        for param in f.params.items.iter() {
            if param.accessibility.is_some() || param.readonly || param.r#override {
                self.diag(
                    super::super::TS_UNSUPPORTED,
                    "parameter properties are not erasable TypeScript",
                    param.span,
                );
            }
            if !param.decorators.is_empty() {
                self.diag(
                    super::super::TS_UNSUPPORTED,
                    "decorators are not erasable TypeScript",
                    param.span,
                );
            }
            if param.pattern.optional {
                self.blank_char_after(param.pattern.span().end, b'?');
            }
            self.walk_pattern(&param.pattern);
        }
        if let Some(rest) = &f.params.rest {
            self.walk_pattern(&rest.argument);
        }
        if let Some(t) = &f.type_parameters {
            self.blank.push(t.span);
        }
        if let Some(r) = &f.return_type {
            self.blank.push(r.span);
        }
    }

    pub(super) fn walk_assignment_target(&mut self, t: &'a SimpleAssignmentTarget<'a>) {
        match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(_) => {}
            SimpleAssignmentTarget::TSAsExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSSatisfiesExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank
                    .push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSTypeAssertion(e) => {
                self.blank
                    .push(Span::new(e.span.start, e.expression.span().start));
                self.walk_expr(&e.expression);
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                self.walk_expr(&m.object);
                self.walk_expr(&m.expression);
            }
            SimpleAssignmentTarget::StaticMemberExpression(m) => self.walk_expr(&m.object),
            SimpleAssignmentTarget::PrivateFieldExpression(m) => self.walk_expr(&m.object),
        }
    }

    pub(super) fn walk_at_maybe(&mut self, t: &'a AssignmentTargetMaybeDefault<'a>) {
        match t {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                self.walk_at(&d.binding);
                self.walk_expr(&d.init);
            }
            _ => self.walk_at(t.to_assignment_target()),
        }
    }

    pub(super) fn walk_at(&mut self, t: &'a AssignmentTarget<'a>) {
        match t {
            AssignmentTarget::ArrayAssignmentTarget(a) => {
                for el in a.elements.iter().flatten() {
                    self.walk_at_maybe(el);
                }
                if let Some(r) = &a.rest {
                    self.walk_at(&r.target);
                }
            }
            AssignmentTarget::ObjectAssignmentTarget(o) => {
                for prop in &o.properties {
                    match prop {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                            if let Some(init) = &p.init {
                                self.walk_expr(init);
                            }
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            if p.computed
                                && let Some(e) = p.name.as_expression()
                            {
                                self.walk_expr(e);
                            }
                            self.walk_at_maybe(&p.binding);
                        }
                    }
                }
                if let Some(r) = &o.rest {
                    self.walk_at(&r.target);
                }
            }
            _ => self.walk_assignment_target(t.to_simple_assignment_target()),
        }
    }

    /// 未解析 ident：facade 可调用名的非直接调用 → INDIRECT_FACADE。
    pub(super) fn ident(&mut self, ident: &'a IdentifierReference<'a>) {
        let name = ident.name.as_str();
        if !self.is_global(ident) {
            return;
        }
        if CALLABLES.contains(&name) || OBJECTS.contains(&name) {
            self.diag(
                super::super::INDIRECT_FACADE,
                format!("`{name}` is a workflow facade callable and may only be invoked directly"),
                ident.span,
            );
        } else if name == "__wf" {
            self.diag(
                super::super::FORBIDDEN_GLOBAL,
                "`__wf` is reserved for the workflow runtime",
                ident.span,
            );
        }
    }

    /// member 访问：查 BANNED_MEMBERS；facade 对象的非调用成员访问 → INDIRECT_FACADE。
    pub(super) fn member(
        &mut self,
        object: &'a Expression<'a>,
        property: &'a IdentifierName<'a>,
        span: Span,
        walk: bool,
    ) {
        if let Expression::Identifier(base) = object
            && self.is_global(base)
        {
            let base_name = base.name.as_str();
            if BANNED_MEMBERS.contains(&(base_name, property.name.as_str())) {
                self.diag(
                    super::super::FORBIDDEN_GLOBAL,
                    format!(
                        "`{base_name}.{}` is a non-deterministic source disabled in workflows",
                        property.name
                    ),
                    span,
                );
                return;
            }
            if OBJECTS.contains(&base_name) {
                self.diag(
                    super::super::INDIRECT_FACADE,
                    format!(
                        "`{base_name}` is a workflow facade object and may only be invoked directly"
                    ),
                    base.span,
                );
                return;
            }
        }
        if walk {
            self.walk_expr(object);
        }
    }

    pub(super) fn member_object_only(&mut self, object: &'a Expression<'a>, _span: Span) {
        if let Expression::Identifier(base) = object
            && self.is_global(base)
            && OBJECTS.contains(&base.name.as_str())
        {
            self.diag(
                super::super::INDIRECT_FACADE,
                format!(
                    "`{}` is a workflow facade object and may only be invoked directly",
                    base.name
                ),
                base.span,
            );
            return;
        }
        self.walk_expr(object);
    }
}
