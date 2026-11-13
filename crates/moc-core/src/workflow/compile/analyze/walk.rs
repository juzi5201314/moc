//! 语法遍历与绑定收集。

use super::super::Diagnostic;
use super::super::PhaseKind;
use super::super::PhaseNode;
use super::super::SiteKind;
use super::super::line_col;
use super::Stmts;
use super::TypeDecl;
use super::Walker;
use super::bound_names;
use super::literal_string;
use super::scoping_symbol;
use super::stmt_as_phase_marker;
use crate::workflow::facade;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;
use oxc_span::Span;

impl<'a> Walker<'a> {
    pub(super) fn program(&mut self, program: &'a Program<'a>) {
        self.walk_stmts(&program.body);
    }

    pub(super) fn diag(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        let (line, col) = line_col(self.source, span.start);
        self.diagnostics.push(Diagnostic {
            code,
            message: message.into(),
            line,
            col,
        });
    }

    pub(super) fn next_site_id(&mut self, kind: SiteKind) -> String {
        let n = self.counters.entry(kind.prefix()).or_default();
        *n += 1;
        format!(
            "{}{}#{}",
            self.options.site_prefix.as_deref().unwrap_or(""),
            kind.prefix(),
            n
        )
    }

    pub(super) fn symbol_of(&self, ident: &IdentifierReference<'a>) -> Option<SymbolId> {
        scoping_symbol(self.scoping, ident)
    }

    /// 未解析引用 => 全局名（facade 或 ES2022）。
    pub(super) fn is_global(&self, ident: &IdentifierReference<'a>) -> bool {
        self.symbol_of(ident).is_none()
    }

    pub(super) fn walk_stmts(&mut self, stmts: Stmts<'a>) {
        let saved_phase = self.phase.clone();
        for stmt in stmts {
            if let Some(call) = stmt_as_phase_marker(stmt) {
                self.phase_marker(call);
                continue;
            }
            self.walk_stmt(stmt);
        }
        self.phase = saved_phase;
    }

    /// 嵌套块：继承当前 phase 进入，退出恢复（marker 不外泄）。
    pub(super) fn walk_block(&mut self, block: &'a BlockStatement<'a>) {
        self.block_depth += 1;
        self.walk_stmts(&block.body);
        self.block_depth -= 1;
    }

    /// `phase(name)` 独立语句：校验名字面量并覆盖后续语句。
    pub(super) fn phase_marker(&mut self, call: &'a CallExpression<'a>) {
        let args = self.arg_exprs(call);
        let name = args.first().and_then(|a| literal_string(a));
        match &name {
            Some(n) if n.is_empty() => {
                self.diag(
                    super::super::NON_LITERAL_NAME,
                    "phase name must be a non-empty literal",
                    args[0].span(),
                );
            }
            Some(n) if n.chars().count() > facade::NAME_MAX_LEN => {
                self.diag(
                    super::super::NON_LITERAL_NAME,
                    format!("phase name exceeds {} characters", facade::NAME_MAX_LEN),
                    args[0].span(),
                );
            }
            Some(n) => {
                if let Some(first) = self.hole_names.get(n) {
                    let (line, col) = line_col(self.source, first.start);
                    self.diag(super::super::HOLE_NAME_DUP, format!("phase name \"{n}\" conflicts with a hole or phase declared at L{line}:C{col}"), args[0].span());
                } else {
                    self.hole_names.insert(n.clone(), args[0].span());
                }
                self.phase = name.clone();
                if !self.phase_order.contains(n) {
                    self.phase_order.push(n.clone());
                    self.phases.push(PhaseNode {
                        id: n.clone(),
                        name: n.clone(),
                        kind: PhaseKind::Phase,
                        sites: Vec::new(),
                    });
                }
            }
            None => self.diag(
                super::super::NON_LITERAL_NAME,
                "phase name must be a non-empty literal",
                args.first().map_or(call.span, |a| a.span()),
            ),
        }
        let id = self.add_site(SiteKind::Phase, call, name, None, None);
        self.insert_at(self.open_paren(call), &id);
        if let Expression::Identifier(callee) = &call.callee {
            self.rewrite_callee(callee);
        }
        self.walk_exprs(&args);
    }

    pub(super) fn walk_stmt(&mut self, stmt: &'a Statement<'a>) {
        match stmt {
            Statement::BlockStatement(block) => self.walk_block(block),
            Statement::VariableDeclaration(decl) => {
                if decl.declare {
                    self.diag(
                        super::super::FORBIDDEN_SYNTAX,
                        "workflow scripts cannot use import/export/declare",
                        decl.span,
                    );
                    return;
                }
                for d in &decl.declarations {
                    self.declarator(d);
                }
            }
            Statement::FunctionDeclaration(func) => {
                if func.r#type == FunctionType::TSDeclareFunction || func.declare {
                    self.diag(
                        super::super::FORBIDDEN_SYNTAX,
                        "workflow scripts cannot use import/export/declare",
                        func.span,
                    );
                    return;
                }
                self.register_fn(func);
                self.walk_function(func);
            }
            Statement::ExpressionStatement(s) => self.walk_expr(&s.expression),
            Statement::IfStatement(s) => {
                self.walk_expr(&s.test);
                self.walk_stmt(&s.consequent);
                if let Some(alt) = &s.alternate {
                    self.walk_stmt(alt);
                }
            }
            Statement::ForStatement(s) => {
                if let Some(init) = &s.init {
                    match init {
                        ForStatementInit::VariableDeclaration(d) => {
                            for d in &d.declarations {
                                self.declarator(d);
                            }
                        }
                        other => self.walk_expr(other.as_expression().unwrap()),
                    }
                }
                if let Some(t) = &s.test {
                    self.walk_expr(t);
                }
                if let Some(u) = &s.update {
                    self.walk_expr(u);
                }
                self.with_loop(s.body.span(), |w| w.walk_stmt(&s.body));
            }
            Statement::ForOfStatement(s) => {
                self.walk_expr(&s.right);
                self.for_binding(&s.left, &s.right);
                // 赋值目标位是写位，不参与依赖。
                if let ForStatementLeft::VariableDeclaration(d) = &s.left {
                    for d in &d.declarations {
                        self.walk_pattern(&d.id);
                    }
                }
                self.with_loop(s.body.span(), |w| w.walk_stmt(&s.body));
            }
            Statement::ForInStatement(s) => {
                self.walk_expr(&s.right);
                if let ForStatementLeft::VariableDeclaration(d) = &s.left {
                    for d in &d.declarations {
                        self.walk_pattern(&d.id);
                    }
                }
                self.with_loop(s.body.span(), |w| w.walk_stmt(&s.body));
            }
            Statement::WhileStatement(s) => {
                self.walk_expr(&s.test);
                self.with_loop(s.body.span(), |w| w.walk_stmt(&s.body));
            }
            Statement::DoWhileStatement(s) => {
                self.with_loop(s.body.span(), |w| w.walk_stmt(&s.body));
                self.walk_expr(&s.test);
            }
            Statement::SwitchStatement(s) => {
                self.walk_expr(&s.discriminant);
                for case in &s.cases {
                    if let Some(t) = &case.test {
                        self.walk_expr(t);
                    }
                    self.walk_stmts(&case.consequent);
                }
            }
            Statement::TryStatement(s) => {
                self.walk_block(&s.block);
                if let Some(h) = &s.handler
                    && let Some(param) = &h.param
                {
                    self.walk_pattern(&param.pattern);
                }
                if let Some(h) = &s.handler {
                    self.walk_block(&h.body);
                }
                if let Some(f) = &s.finalizer {
                    self.walk_block(f);
                }
            }
            Statement::ReturnStatement(s) => {
                if let Some(a) = &s.argument {
                    self.walk_expr(a);
                }
            }
            Statement::ThrowStatement(s) => self.walk_expr(&s.argument),
            Statement::LabeledStatement(s) => self.walk_stmt(&s.body),
            Statement::ClassDeclaration(c) => {
                if c.declare {
                    self.diag(
                        super::super::FORBIDDEN_SYNTAX,
                        "workflow scripts cannot use import/export/declare",
                        c.span,
                    );
                    return;
                }
                self.walk_class(c);
            }
            // —— 脚本体禁止的语法 ——
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_) => {
                self.diag(
                    super::super::FORBIDDEN_SYNTAX,
                    "workflow scripts cannot use import/export/declare",
                    stmt.span(),
                );
            }
            Statement::WithStatement(_) => {
                self.diag(
                    super::super::FORBIDDEN_SYNTAX,
                    "with statements are not allowed in workflow scripts",
                    stmt.span(),
                );
            }
            Statement::TSTypeAliasDeclaration(d) => {
                self.blank.push(d.span);
                self.type_decls
                    .insert(d.id.name.to_string(), TypeDecl::Alias(d));
            }
            Statement::TSInterfaceDeclaration(d) => {
                self.blank.push(d.span);
                self.type_decls
                    .insert(d.id.name.to_string(), TypeDecl::Interface(d));
            }
            Statement::TSEnumDeclaration(d) => {
                if d.declare {
                    self.diag(
                        super::super::FORBIDDEN_SYNTAX,
                        "workflow scripts cannot use import/export/declare",
                        d.span,
                    );
                } else {
                    self.diag(
                        super::super::TS_UNSUPPORTED,
                        "enum is not erasable TypeScript; use literal union types instead",
                        d.span,
                    );
                }
            }
            Statement::TSModuleDeclaration(d) => {
                self.diag(
                    super::super::TS_UNSUPPORTED,
                    "namespace/module declarations are not erasable TypeScript",
                    d.span,
                );
            }
            _ => {}
        }
    }

    pub(super) fn with_loop(&mut self, body: Span, f: impl FnOnce(&mut Self)) {
        self.loop_bodies.push(body);
        let saved = self.in_loop;
        self.in_loop = true;
        f(self);
        self.in_loop = saved;
    }

    /// for-of/for-await 左值绑定 → 被迭代表达式（channel send→for await 的边）。
    pub(super) fn for_binding(
        &mut self,
        left: &'a ForStatementLeft<'a>,
        right: &'a Expression<'a>,
    ) {
        if let ForStatementLeft::VariableDeclaration(decl) = left {
            for d in &decl.declarations {
                for name in bound_names(&d.id) {
                    if let Some(sym) = name.symbol_id.get() {
                        self.iter_source.insert(sym, right);
                    }
                }
            }
        }
    }

    pub(super) fn declarator(&mut self, d: &'a VariableDeclarator<'a>) {
        for name in bound_names(&d.id) {
            if let (Some(sym), Some(init)) = (name.symbol_id.get(), &d.init) {
                self.binding_init.insert(sym, init);
            }
        }
        self.walk_pattern(&d.id);
        if let Some(init) = &d.init {
            self.walk_expr(init);
        }
    }

    /// 收集绑定模式里的类型标注/可选修饰（空白化）并递归子模式。
    pub(super) fn walk_pattern(&mut self, p: &'a BindingPattern<'a>) {
        if p.optional {
            self.blank_char_after(p.span().end, b'?');
        }
        if let Some(a) = &p.type_annotation {
            self.blank.push(a.span);
        }
        match &p.kind {
            BindingPatternKind::ObjectPattern(o) => {
                for prop in &o.properties {
                    self.walk_pattern(&prop.value);
                }
                if let Some(rest) = &o.rest {
                    self.walk_pattern(&rest.argument);
                }
            }
            BindingPatternKind::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    self.walk_pattern(el);
                }
                if let Some(rest) = &a.rest {
                    self.walk_pattern(&rest.argument);
                }
            }
            BindingPatternKind::AssignmentPattern(ap) => {
                self.walk_pattern(&ap.left);
                self.walk_expr(&ap.right);
            }
            BindingPatternKind::BindingIdentifier(_) => {}
        }
    }

    pub(super) fn register_fn(&mut self, func: &'a Function<'a>) {
        if let Some(id) = &func.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.fn_decl.insert(sym, func);
        }
    }

    /// 形参 → (所属函数 symbol 或宿主绑定 symbol, 位次)。
    pub(super) fn register_params(&mut self, func: &'a Function<'a>) {
        let owner = func.id.as_ref().and_then(|i| i.symbol_id.get());
        for (index, param) in func.params.items.iter().enumerate() {
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
                // `a?: T` → `a`；? 在 pattern 文本末尾与 `:` 之间。
                self.blank_char_after(param.pattern.span().end, b'?');
            }
            self.walk_pattern(&param.pattern);
            if let Some(owner) = owner {
                for name in bound_names(&param.pattern) {
                    if let Some(sym) = name.symbol_id.get() {
                        self.param_owner.insert(sym, (owner, index));
                    }
                }
            }
        }
        if let Some(rest) = &func.params.rest {
            self.walk_pattern(&rest.argument);
        }
        if let Some(t) = &func.type_parameters {
            self.blank.push(t.span);
        }
        if let Some(r) = &func.return_type {
            self.blank.push(r.span);
        }
    }

    /// 在 source 中从 pos 起找下一个 target 字节并加入空白集合。
    pub(super) fn blank_char_after(&mut self, pos: u32, target: u8) {
        let bytes = self.source.as_bytes();
        let mut i = pos as usize;
        while i < bytes.len() && bytes[i] != target && bytes[i] != b':' && i - pos as usize <= 4 {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == target {
            self.blank.push(Span::new(i as u32, i as u32 + 1));
        }
    }

    /// 函数体：phase 必须经调用点传播，不随定义处词法继承。
    pub(super) fn walk_function(&mut self, func: &'a Function<'a>) {
        self.register_params(func);
        let saved_phase = self.phase.take();
        self.block_depth += 1;
        if let Some(id) = &func.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.fn_stack.push(sym);
            if let Some(body) = &func.body {
                self.walk_stmts(&body.statements);
            }
            self.fn_stack.pop();
        } else if let Some(body) = &func.body {
            self.walk_stmts(&body.statements);
        }
        self.block_depth -= 1;
        self.phase = saved_phase;
    }

    pub(super) fn walk_class(&mut self, class: &'a Class<'a>) {
        for d in &class.decorators {
            self.diag(
                super::super::TS_UNSUPPORTED,
                "decorators are not erasable TypeScript",
                d.span,
            );
        }
        if let Some(t) = &class.type_parameters {
            self.blank.push(t.span);
        }
        if let Some(s) = &class.super_class {
            self.walk_expr(s);
        }
        for e in &class.implements {
            self.blank.push(e.span);
        }
        if let Some(t) = &class.super_type_arguments {
            self.blank.push(t.span);
        }
        for el in &class.body.body {
            match el {
                ClassElement::MethodDefinition(m) => self.walk_method(m),
                ClassElement::PropertyDefinition(p) => self.walk_property(p),
                ClassElement::AccessorProperty(a) => {
                    self.blank_modifiers(a.span, a.key.span());
                    if let Some(t) = &a.type_annotation {
                        self.blank.push(t.span);
                    }
                    if let Some(v) = &a.value {
                        self.walk_expr(v);
                    }
                }
                ClassElement::TSIndexSignature(i) => self.blank.push(i.span),
                ClassElement::StaticBlock(b) => self.walk_stmts(&b.body),
            }
        }
    }

    /// `private readonly`/`abstract` 等修饰区整体空白化（保留 static——它是真实 JS）。
    pub(super) fn blank_modifiers(&mut self, member: Span, key: Span) {
        const MODIFIERS: &[&str] = &[
            "public",
            "private",
            "protected",
            "readonly",
            "abstract",
            "override",
            "declare",
            "accessor",
        ];
        let text = &self.source[member.start as usize..key.start as usize];
        let bytes = text.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &text[start..i];
                if MODIFIERS.contains(&word) && word != "static" {
                    self.blank.push(Span::new(
                        member.start + start as u32,
                        member.start + i as u32,
                    ));
                }
            } else {
                i += 1;
            }
        }
    }

    pub(super) fn walk_method(&mut self, m: &'a MethodDefinition<'a>) {
        for d in &m.decorators {
            self.diag(
                super::super::TS_UNSUPPORTED,
                "decorators are not erasable TypeScript",
                d.span,
            );
        }
        if m.r#type == MethodDefinitionType::TSAbstractMethodDefinition {
            self.blank.push(m.span);
            return;
        }
        self.blank_modifiers(m.span, m.key.span());
        if m.optional {
            self.blank_char_after(m.key.span().end, b'?');
        }
        if let PropertyKey::Identifier(e) = &m.key {
            self.ident(e);
        }
        self.walk_function(&m.value);
    }

    pub(super) fn walk_property(&mut self, p: &'a PropertyDefinition<'a>) {
        for d in &p.decorators {
            self.diag(
                super::super::TS_UNSUPPORTED,
                "decorators are not erasable TypeScript",
                d.span,
            );
        }
        if p.r#type == PropertyDefinitionType::TSAbstractPropertyDefinition {
            self.blank.push(p.span);
            return;
        }
        self.blank_modifiers(p.span, p.key.span());
        if p.optional {
            self.blank_char_after(p.key.span().end, b'?');
        }
        if p.definite {
            self.blank_char_after(p.key.span().end, b'!');
        }
        if p.declare {
            self.diag(
                super::super::FORBIDDEN_SYNTAX,
                "workflow scripts cannot use import/export/declare",
                p.span,
            );
        }
        if let Some(t) = &p.type_annotation {
            self.blank.push(t.span);
        }
        if let PropertyKey::Identifier(e) = &p.key {
            self.ident(e);
        }
        if let Some(v) = &p.value {
            self.walk_expr(v);
        }
    }
}
