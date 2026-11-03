//! 编译主流程：解析 → 静态诊断 → WorkflowPlan → 降级。

use super::schema;
use super::{
    CompiledWorkflow, CompileOptions, Diagnostic, EdgeKind, ModelRef, ModelSource, PhaseKind,
    PhaseNode, PlanEdge, Role, Site, SiteKind, WorkflowPlan, line_col,
};
use crate::workflow::facade;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::{SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, Span};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// facade 顶层可调用名（只允许直接调用）。
const CALLABLES: &[&str] = &[
    "agent", "model", "phase", "log", "report", "channel", "future", "hole",
];
/// facade 对象名（只允许 `obj.member(...)` 直接调用）。
const OBJECTS: &[&str] = &["artifact", "files", "git", "world"];
/// facade 值名（可自由读写，不算间接调用）。
const VALUES: &[&str] = &["args", "run"];
/// facade 类型名（只应出现在类型位；列入白名单避免误报全局）。
const TYPE_NAMES: &[&str] = &[
    "Node",
    "Agent",
    "AgentPersona",
    "ModelRef",
    "ArtifactRef",
    "ArtifactOptions",
    "ArtifactFileOptions",
    "ArtifactField",
    "ChartSpec",
    "TableSpec",
    "MetricsSpec",
    "BoardSpec",
    "GrepMatch",
    "GitStatus",
    "GitCommit",
    "WorldRunResult",
    "Channel",
];
const FILES_MEMBERS: &[&str] = &["glob", "read", "grep"];
const GIT_MEMBERS: &[&str] = &["changedFiles", "diff", "status", "log"];
const ARTIFACT_MEMBERS: &[&str] = &["file", "markdown", "chart", "table", "metrics", "board"];

/// ES2022 内建全局；其余未解析引用一律 FORBIDDEN_GLOBAL。
const ES2022_GLOBALS: &[&str] = &[
    "AggregateError", "Array", "ArrayBuffer", "Atomics", "BigInt", "BigInt64Array",
    "BigUint64Array", "Boolean", "DataView", "Date", "decodeURI", "decodeURIComponent",
    "encodeURI", "encodeURIComponent", "Error", "escape", "eval", "EvalError",
    "FinalizationRegistry", "Float32Array", "Float64Array", "Function", "globalThis",
    "Infinity", "Int16Array", "Int32Array", "Int8Array", "Intl", "isFinite", "isNaN",
    "JSON", "Map", "Math", "NaN", "Number", "Object", "parseFloat", "parseInt", "Promise",
    "Proxy", "RangeError", "ReferenceError", "Reflect", "RegExp", "Set",
    "SharedArrayBuffer", "String", "Symbol", "SyntaxError", "TypeError", "Uint16Array",
    "Uint32Array", "Uint8Array", "Uint8ClampedArray", "undefined", "unescape", "URIError",
    "WeakMap", "WeakRef", "WeakSet",
    // 常用 TS 工具类型名，出现在类型位时 fallback 兜底。
    "Partial", "Required", "Readonly", "Pick", "Omit", "Record", "Exclude", "Extract",
    "Awaited", "ReturnType", "Parameters", "NonNullable", "Uppercase", "Lowercase",
    "Capitalize", "Uncapitalize", "InstanceType", "ConstructorParameters", "Iterable",
    "Iterator", "IterableIterator", "AsyncIterable", "AsyncIterator", "PromiseLike",
];
/// 这些全局的特定成员禁止访问（编译期诊断 + worker 运行时双重禁止）。
const BANNED_MEMBERS: &[(&str, &str)] = &[("Date", "now"), ("Math", "random")];

struct SiteRec<'a> {
    id: String,
    kind: SiteKind,
    /// 该调用点表达式在源中的 span。
    span: Span,
    phase: Option<String>,
    label: Option<String>,
    /// 实参表达式（消费依赖追踪用）。
    args: Vec<&'a Expression<'a>>,
    /// ask 站点的 actor receiver 表达式。
    receiver: Option<&'a Expression<'a>>,
    /// ask<T>/hole<T> 的类型实参。
    type_arg: Option<&'a TSType<'a>>,
    fan_out: bool,
    /// 所属函数（call-graph 阶段传播用）；顶层为 None。
    in_fn: Option<SymbolId>,
}

pub(super) fn compile_inner(
    source: &str,
    options: &CompileOptions<'_>,
) -> Result<CompiledWorkflow, Vec<Diagnostic>> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, oxc_span::SourceType::ts())
        .with_options(ParseOptions {
            allow_return_outside_function: true,
            ..ParseOptions::default()
        })
        .parse();
    let diagnostics: Vec<Diagnostic> = parsed
        .errors
        .iter()
        .map(|error| {
            let (line, col) = line_col(
                source,
                error.labels.as_ref().and_then(|l| l.first()).map_or(0, |l| l.offset()) as u32,
            );
            Diagnostic {
                code: super::SYNTAX_ERROR,
                message: error.to_string(),
                line,
                col,
            }
        })
        .collect();
    if parsed.panicked {
        return Err(diagnostics);
    }
    let program = parsed.program;
    let built = SemanticBuilder::new().build(&program);
    let semantic = built.semantic;

    let mut w = Walker {
        source,
        options,
        diagnostics,
        nodes: semantic.nodes(),
        scoping: semantic.scoping(),
        counters: HashMap::new(),
        sites: Vec::new(),
        blank: Vec::new(),
        inserts: Vec::new(),
        phases: Vec::new(),
        phase_order: Vec::new(),
        phase_sites: BTreeMap::new(),
        literal_agents: HashMap::new(),
        hole_names: HashMap::new(),
        roles: Vec::new(),
        role_model_spans: Vec::new(),
        model_refs: Vec::new(),
        commands: BTreeMap::new(),
        binding_init: HashMap::new(),
        param_owner: HashMap::new(),
        iter_source: HashMap::new(),
        fn_decl: HashMap::new(),
        fn_calls: HashMap::new(),
        channel_sends: HashMap::new(),
        fn_stack: Vec::new(),
        phase: None,
        in_loop: false,
        artifact_members: HashMap::new(),
        ask_sites_by_agent: HashMap::new(),
        bad_globals: HashSet::new(),
        block_depth: 0,
        loop_bodies: Vec::new(),
        user_owns_args_run: [false; 2],
        type_decls: HashMap::new(),
    };
    w.program(&program);
    w.check_globals(semantic.scoping());
    if !w.diagnostics.is_empty() {
        return Err(w.diagnostics);
    }
    w.finish_plan(source)
}

struct Walker<'a> {
    source: &'a str,
    options: &'a CompileOptions<'a>,
    diagnostics: Vec<Diagnostic>,
    nodes: &'a oxc_semantic::AstNodes<'a>,
    scoping: &'a oxc_semantic::Scoping,

    counters: HashMap<&'static str, u32>,
    sites: Vec<SiteRec<'a>>,
    /// 待空白化的类型区域（保持行/列不变量）。
    blank: Vec<Span>,
    /// (offset, 文本) 插入点。
    inserts: Vec<(u32, &'static str, String)>,
    /// phase 节点（按名合并）：name → sites。
    phases: Vec<PhaseNode>,
    /// phase 名 → 加入顺序。
    phase_order: Vec<String>,
    phase_sites: BTreeMap<String, Vec<String>>,
    /// 字面量 agent 名 → 首个 site_id。
    literal_agents: HashMap<String, String>,
    /// hole/phase 名占用表（HOLE_NAME_DUP）。
    hole_names: HashMap<String, Span>,
    roles: Vec<Role>,
    /// (role 下标, model() 调用 span)：persona.model 路由，finish_plan 解析 site_id。
    role_model_spans: Vec<(usize, Span)>,
    model_refs: Vec<ModelRef>,
    commands: BTreeMap<String, Vec<String>>,
    /// 绑定 symbol → 初始值表达式（含解构退化到整体 init）。
    binding_init: HashMap<SymbolId, &'a Expression<'a>>,
    /// 函数参数 symbol → (函数 symbol, 参数位)。
    param_owner: HashMap<SymbolId, (SymbolId, usize)>,
    /// for-of/for-await 左值绑定 symbol → 被迭代表达式。
    iter_source: HashMap<SymbolId, &'a Expression<'a>>,
    /// 函数 symbol → Function 节点（内联分析）。
    fn_decl: HashMap<SymbolId, &'a Function<'a>>,
    /// 函数 symbol → (调用点, 调用点相位, 调用方函数, 调用点在循环内)。
    fn_calls: HashMap<SymbolId, Vec<FnCall<'a>>>,
    /// channel 绑定 symbol → send 调用点。
    channel_sends: HashMap<SymbolId, Vec<&'a CallExpression<'a>>>,

    fn_stack: Vec<SymbolId>,
    phase: Option<String>,
    in_loop: bool,
    /// artifact id → 成员名（每个 id 只配一种成员）。
    artifact_members: HashMap<String, &'static str>,
    /// agent 站点 site_id → ask 站点 site_id。
    ask_sites_by_agent: HashMap<String, Vec<String>>,
    /// 已报过的非法全局名（去重）。
    bad_globals: HashSet<&'a str>,
    /// 嵌套块/函数体深度（顶层声明判定）。
    block_depth: u32,
    /// 循环体 span 列表（fan-out 二级传播判定）。
    loop_bodies: Vec<Span>,
    /// 脚本顶层是否自行声明 args / run（声明则不再注入 facade 值）。
    user_owns_args_run: [bool; 2],
    /// 脚本内 interface/type 声明（schema 合成与嵌套引用解析用）。
    type_decls: HashMap<String, TypeDecl<'a>>,
}

/// (调用点, 调用点相位, 调用方函数, 调用点是否在循环内)。
type FnCall<'a> = (&'a CallExpression<'a>, Option<String>, Option<SymbolId>, bool);

#[derive(Clone, Copy)]
pub(super) enum TypeDecl<'a> {
    Interface(&'a TSInterfaceDeclaration<'a>),
    Alias(&'a TSTypeAliasDeclaration<'a>),
}

// —— AST 结构别名 ——
type Stmts<'a> = &'a [Statement<'a>];

impl<'a> Walker<'a> {
    fn program(&mut self, program: &'a Program<'a>) {
        self.walk_stmts(&program.body);
    }

    fn diag(&mut self, code: &'static str, message: impl Into<String>, span: Span) {
        let (line, col) = line_col(self.source, span.start);
        self.diagnostics.push(Diagnostic {
            code,
            message: message.into(),
            line,
            col,
        });
    }

    fn next_site_id(&mut self, kind: SiteKind) -> String {
        let n = self.counters.entry(kind.prefix()).or_default();
        *n += 1;
        format!("{}{}#{}", self.options.site_prefix.as_deref().unwrap_or(""), kind.prefix(), n)
    }

    fn symbol_of(&self, ident: &IdentifierReference<'a>) -> Option<SymbolId> {
        scoping_symbol(self.scoping, ident)
    }

    /// 未解析引用 => 全局名（facade 或 ES2022）。
    fn is_global(&self, ident: &IdentifierReference<'a>) -> bool {
        self.symbol_of(ident).is_none()
    }

    fn walk_stmts(&mut self, stmts: Stmts<'a>) {
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
    fn walk_block(&mut self, block: &'a BlockStatement<'a>) {
        self.block_depth += 1;
        self.walk_stmts(&block.body);
        self.block_depth -= 1;
    }

    /// `phase(name)` 独立语句：校验名字面量并覆盖后续语句。
    fn phase_marker(&mut self, call: &'a CallExpression<'a>) {
        let args = self.arg_exprs(call);
        let name = args.first().and_then(|a| literal_string(a));
        match &name {
            Some(n) if n.is_empty() => {
                self.diag(super::NON_LITERAL_NAME, "phase name must be a non-empty literal", args[0].span());
            }
            Some(n) if n.chars().count() > facade::NAME_MAX_LEN => {
                self.diag(super::NON_LITERAL_NAME, format!("phase name exceeds {} characters", facade::NAME_MAX_LEN), args[0].span());
            }
            Some(n) => {
                if let Some(first) = self.hole_names.get(n) {
                    let (line, col) = line_col(self.source, first.start);
                    self.diag(super::HOLE_NAME_DUP, format!("phase name \"{n}\" conflicts with a hole or phase declared at L{line}:C{col}"), args[0].span());
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
            None => self.diag(super::NON_LITERAL_NAME, "phase name must be a non-empty literal", args.first().map_or(call.span, |a| a.span())),
        }
        let id = self.add_site(SiteKind::Phase, call, name, None, None);
        self.insert_at(self.open_paren(call), &id);
        if let Expression::Identifier(callee) = &call.callee {
            self.rewrite_callee(callee);
        }
        self.walk_exprs(&args);
    }

    fn walk_stmt(&mut self, stmt: &'a Statement<'a>) {
        match stmt {
            Statement::BlockStatement(block) => self.walk_block(block),
            Statement::VariableDeclaration(decl) => {
                if decl.declare {
                    self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", decl.span);
                    return;
                }
                for d in &decl.declarations {
                    self.declarator(d);
                }
            }
            Statement::FunctionDeclaration(func) => {
                if func.r#type == FunctionType::TSDeclareFunction || func.declare {
                    self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", func.span);
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
                    self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", c.span);
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
                self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", stmt.span());
            }
            Statement::WithStatement(_) => {
                self.diag(super::FORBIDDEN_SYNTAX, "with statements are not allowed in workflow scripts", stmt.span());
            }
            Statement::TSTypeAliasDeclaration(d) => {
                self.blank.push(d.span);
                self.type_decls.insert(d.id.name.to_string(), TypeDecl::Alias(d));
            }
            Statement::TSInterfaceDeclaration(d) => {
                self.blank.push(d.span);
                self.type_decls.insert(d.id.name.to_string(), TypeDecl::Interface(d));
            }
            Statement::TSEnumDeclaration(d) => {
                if d.declare {
                    self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", d.span);
                } else {
                    self.diag(super::TS_UNSUPPORTED, "enum is not erasable TypeScript; use literal union types instead", d.span);
                }
            }
            Statement::TSModuleDeclaration(d) => {
                self.diag(super::TS_UNSUPPORTED, "namespace/module declarations are not erasable TypeScript", d.span);
            }
            _ => {}
        }
    }

    fn with_loop(&mut self, body: Span, f: impl FnOnce(&mut Self)) {
        self.loop_bodies.push(body);
        let saved = self.in_loop;
        self.in_loop = true;
        f(self);
        self.in_loop = saved;
    }

    /// for-of/for-await 左值绑定 → 被迭代表达式（channel send→for await 的边）。
    fn for_binding(&mut self, left: &'a ForStatementLeft<'a>, right: &'a Expression<'a>) {
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

    fn declarator(&mut self, d: &'a VariableDeclarator<'a>) {
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
    fn walk_pattern(&mut self, p: &'a BindingPattern<'a>) {
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

    fn register_fn(&mut self, func: &'a Function<'a>) {
        if let Some(id) = &func.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.fn_decl.insert(sym, func);
        }
    }

    /// 形参 → (所属函数 symbol 或宿主绑定 symbol, 位次)。
    fn register_params(&mut self, func: &'a Function<'a>) {
        let owner = func.id.as_ref().and_then(|i| i.symbol_id.get());
        for (index, param) in func.params.items.iter().enumerate() {
            if param.accessibility.is_some() || param.readonly || param.r#override {
                self.diag(super::TS_UNSUPPORTED, "parameter properties are not erasable TypeScript", param.span);
            }
            if !param.decorators.is_empty() {
                self.diag(super::TS_UNSUPPORTED, "decorators are not erasable TypeScript", param.span);
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
    fn blank_char_after(&mut self, pos: u32, target: u8) {
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
    fn walk_function(&mut self, func: &'a Function<'a>) {
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

    fn walk_class(&mut self, class: &'a Class<'a>) {
        for d in &class.decorators {
            self.diag(super::TS_UNSUPPORTED, "decorators are not erasable TypeScript", d.span);
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
    fn blank_modifiers(&mut self, member: Span, key: Span) {
        const MODIFIERS: &[&str] = &[
            "public", "private", "protected", "readonly", "abstract", "override", "declare", "accessor",
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
                    self.blank.push(Span::new(member.start + start as u32, member.start + i as u32));
                }
            } else {
                i += 1;
            }
        }
    }

    fn walk_method(&mut self, m: &'a MethodDefinition<'a>) {
        for d in &m.decorators {
            self.diag(super::TS_UNSUPPORTED, "decorators are not erasable TypeScript", d.span);
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

    fn walk_property(&mut self, p: &'a PropertyDefinition<'a>) {
        for d in &p.decorators {
            self.diag(super::TS_UNSUPPORTED, "decorators are not erasable TypeScript", d.span);
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
            self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", p.span);
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

// —— 辅助：符号解析与绑定名枚举 ——

fn scoping_symbol(
    scoping: &oxc_semantic::Scoping,
    ident: &IdentifierReference<'_>,
) -> Option<SymbolId> {
    scoping.get_reference(ident.reference_id()).symbol_id()
}

/// 收集绑定模式内全部绑定标识符（用于 symbol → init 映射）。
fn bound_names<'a>(pattern: &'a BindingPattern<'a>) -> Vec<&'a BindingIdentifier<'a>> {
    fn collect<'a>(p: &'a BindingPattern<'a>, out: &mut Vec<&'a BindingIdentifier<'a>>) {
        match &p.kind {
            BindingPatternKind::BindingIdentifier(i) => out.push(i),
            BindingPatternKind::ObjectPattern(o) => {
                for prop in &o.properties {
                    collect(&prop.value, out);
                }
                if let Some(r) = &o.rest {
                    collect(&r.argument, out);
                }
            }
            BindingPatternKind::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    collect(el, out);
                }
                if let Some(r) = &a.rest {
                    collect(&r.argument, out);
                }
            }
            BindingPatternKind::AssignmentPattern(ap) => collect(&ap.left, out),
        }
    }
    let mut out = Vec::new();
    collect(pattern, &mut out);
    out
}

/// 独立成句的 `phase(...)` 调用；其他位置的 phase() 是 INDIRECT_FACADE。
fn stmt_as_phase_marker<'a>(stmt: &'a Statement<'a>) -> Option<&'a CallExpression<'a>> {
    if let Statement::ExpressionStatement(expr) = stmt
        && let Expression::CallExpression(call) = &expr.expression
        && let Expression::Identifier(callee) = &call.callee
        && callee.name == "phase"
    {
        return Some(call);
    }
    None
}

impl<'a> Walker<'a> {
    fn walk_expr(&mut self, expr: &'a Expression<'a>) {
        match expr {
            Expression::CallExpression(call) => self.call(call),
            Expression::ParenthesizedExpression(p) => self.walk_expr(&p.expression),
            Expression::AwaitExpression(a) => self.walk_expr(&a.argument),
            Expression::Identifier(ident) => self.ident(ident),
            Expression::StaticMemberExpression(m) => self.member(&m.object, &m.property, m.span, true),
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
                    self.diag(super::FORBIDDEN_GLOBAL, "new Date() without arguments is non-deterministic", n.span);
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
                self.diag(super::FORBIDDEN_SYNTAX, "workflow scripts cannot use import/export/declare", i.span);
            }
            Expression::YieldExpression(y) => {
                if let Some(a) = &y.argument {
                    self.walk_expr(a);
                }
            }
            Expression::TSAsExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSSatisfiesExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSTypeAssertion(e) => {
                self.blank.push(Span::new(e.span.start, e.expression.span().start));
                self.walk_expr(&e.expression);
            }
            Expression::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            Expression::TSInstantiationExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            _ => {}
        }
    }

    fn register_params_farrow(&mut self, f: &'a ArrowFunctionExpression<'a>) {
        for param in f.params.items.iter() {
            if param.accessibility.is_some() || param.readonly || param.r#override {
                self.diag(super::TS_UNSUPPORTED, "parameter properties are not erasable TypeScript", param.span);
            }
            if !param.decorators.is_empty() {
                self.diag(super::TS_UNSUPPORTED, "decorators are not erasable TypeScript", param.span);
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

    fn walk_assignment_target(&mut self, t: &'a SimpleAssignmentTarget<'a>) {
        match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(_) => {}
            SimpleAssignmentTarget::TSAsExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSSatisfiesExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            SimpleAssignmentTarget::TSTypeAssertion(e) => {
                self.blank.push(Span::new(e.span.start, e.expression.span().start));
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

    fn walk_at_maybe(&mut self, t: &'a AssignmentTargetMaybeDefault<'a>) {
        match t {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                self.walk_at(&d.binding);
                self.walk_expr(&d.init);
            }
            _ => self.walk_at(t.to_assignment_target()),
        }
    }

    fn walk_at(&mut self, t: &'a AssignmentTarget<'a>) {
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
    fn ident(&mut self, ident: &'a IdentifierReference<'a>) {
        let name = ident.name.as_str();
        if !self.is_global(ident) {
            return;
        }
        if CALLABLES.contains(&name) || OBJECTS.contains(&name) {
            self.diag(
                super::INDIRECT_FACADE,
                format!("`{name}` is a workflow facade callable and may only be invoked directly"),
                ident.span,
            );
        } else if name == "__wf" {
            self.diag(super::FORBIDDEN_GLOBAL, "`__wf` is reserved for the workflow runtime", ident.span);
        }
    }

    /// member 访问：查 BANNED_MEMBERS；facade 对象的非调用成员访问 → INDIRECT_FACADE。
    fn member(
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
                    super::FORBIDDEN_GLOBAL,
                    format!("`{base_name}.{}` is a non-deterministic source disabled in workflows", property.name),
                    span,
                );
                return;
            }
            if OBJECTS.contains(&base_name) {
                self.diag(
                    super::INDIRECT_FACADE,
                    format!("`{base_name}` is a workflow facade object and may only be invoked directly"),
                    base.span,
                );
                return;
            }
        }
        if walk {
            self.walk_expr(object);
        }
    }

    fn member_object_only(&mut self, object: &'a Expression<'a>, _span: Span) {
        if let Expression::Identifier(base) = object
            && self.is_global(base)
            && OBJECTS.contains(&base.name.as_str())
        {
            self.diag(
                super::INDIRECT_FACADE,
                format!("`{}` is a workflow facade object and may only be invoked directly", base.name),
                base.span,
            );
            return;
        }
        self.walk_expr(object);
    }
}

/// 字面量字符串实参的求值（StringLiteral / 无插值 TemplateLiteral / 简单字符串拼接）。
fn literal_string<'a>(expr: &'a Expression<'a>) -> Option<String> {
    match expr {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::TemplateLiteral(t)
            if t.expressions.is_empty() && t.quasis.len() == 1 =>
        {
            Some(t.quasis[0].value.cooked.as_ref()?.to_string())
        }
        Expression::ParenthesizedExpression(p) => literal_string(&p.expression),
        Expression::BinaryExpression(b)
            if b.operator == BinaryOperator::Addition =>
        {
            Some(literal_string(&b.left)? + &literal_string(&b.right)?)
        }
        _ => None,
    }
}

/// 模板实参的 name_pattern 文本（`reviewer-${i}` 原样保留形）。
fn name_pattern_of<'a>(expr: &'a Expression<'a>) -> Option<String> {
    match expr {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::TemplateLiteral(t) => {
            let mut out = String::new();
            for (i, q) in t.quasis.iter().enumerate() {
                out.push_str(q.value.cooked.as_ref().map_or("", |c| &**c));
                if i < t.expressions.len() {
                    out.push_str("${…}");
                }
            }
            Some(out)
        }
        _ => None,
    }
}

impl<'a> Walker<'a> {
    /// `(` 位置：callee 之后第一个 `(`（跳过 `?.` 与类型实参）。
    fn open_paren(&self, call: &'a CallExpression<'a>) -> Option<u32> {
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

    fn arg_exprs<'b>(&self, call: &'b CallExpression<'a>) -> Vec<&'b Expression<'a>> {
        call.arguments.iter().map(|a| a.to_expression()).collect()
    }

    fn add_site(&mut self, kind: SiteKind, call: &'a CallExpression<'a>, label: Option<String>, receiver: Option<&'a Expression<'a>>, type_arg: Option<&'a TSType<'a>>) -> String {
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
            self.phase_sites.entry(phase.clone()).or_default().push(id.clone());
        }
        id
    }

    fn call(&mut self, call: &'a CallExpression<'a>) {
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
    fn global_call(&mut self, call: &'a CallExpression<'a>, name: &'a str, args: &[&'a Expression<'a>]) {
        let open = self.open_paren(call);
        if let Expression::Identifier(callee) = &call.callee {
            self.rewrite_callee(callee);
        }
        match name {
            "agent" => self.agent_call(call, args, open),
            "model" => self.model_call(call, args, open),
            "phase" => {
                // 到达这里说明 phase() 不在独立语句位（marker 已在 stmt 层拦下）。
                self.diag(super::INDIRECT_FACADE, "`phase(name)` must be a standalone statement", call.span);
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

    fn insert_at(&mut self, open: Option<u32>, site_id: &str) {
        if let Some(open) = open {
            self.inserts.push((open + 1, "site", format!("\"{site_id}\", ")));
        }
    }

    fn walk_exprs(&mut self, args: &[&'a Expression<'a>]) {
        for a in args {
            self.walk_expr(a);
        }
    }

    /// `future(...)`/`hole(...)` 的函数体实参：体内语句继承外层 phase 覆盖
    ///（体在派发点异步开跑，属同一 phase 边界）。
    fn walk_deferred(&mut self, expr: &'a Expression<'a>) {
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

    fn agent_call(&mut self, call: &'a CallExpression<'a>, args: &[&'a Expression<'a>], open: Option<u32>) {
        let name_arg = args.first().copied();
        let persona = args.get(1).copied();
        let literal_name = name_arg.and_then(|a| literal_string(a));
        let id = self.add_site(
            SiteKind::Actor,
            call,
            literal_name.clone().or_else(|| name_arg.and_then(|a| name_pattern_of(a))),
            None,
            None,
        );
        self.insert_at(open, &id);

        // DUP_AGENT_NAME / FANOUT_LITERAL_NAME
        let fan_out = self.sites.last().unwrap().fan_out;
        if let Some(name) = &literal_name {
            if fan_out {
                self.diag(super::FANOUT_LITERAL_NAME, format!("literal agent name \"{name}\" inside a fan-out position collides on every iteration"), args[0].span());
            } else if let Some(first) = self.literal_agents.get(name) {
                self.diag(super::DUP_AGENT_NAME, format!("duplicate agent name \"{name}\" (first declared at {first})"), args[0].span());
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

    fn persona_of(&mut self, persona: Option<&'a Expression<'a>>, name: Option<&'a Expression<'a>>, args: &[&'a Expression<'a>]) -> (ModelSource, Option<String>, Option<Span>) {
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
                        self.diag(super::NON_LITERAL_MODEL, "persona.model must be a non-empty literal or model()", model.span());
                    } else {
                        self.check_model(&s.value, model.span());
                        model_source = ModelSource::Literal { model: s.value.to_string() };
                    }
                }
                // model(...) 或绑到 model() 的标识符 → 路由到 ModelRef；
                // span → site_id 在 finish_plan 统一解析（此时 model() 站点尚未登记）。
                _ if self.model_ref_span(model).is_some() => {
                    model_source = ModelSource::Routed { refs: Vec::new() };
                    routed_span = self.model_ref_span(model);
                }
                _ => {
                    self.diag(super::NON_LITERAL_MODEL, "persona.model must be a literal, a model() ModelRef, or a literal union", model.span());
                }
            }
        }
        let _ = name;
        let _ = args;
        (model_source, excerpt, routed_span)
    }

    /// 表达式若（直接或经变量绑定）是 `model(...)` 调用，返回其 span。
    fn model_ref_span(&self, expr: &'a Expression<'a>) -> Option<Span> {
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
        if matches!(&call.callee, Expression::Identifier(c) if c.name == "model" && self.is_global(c)) {
            Some(call.span)
        } else {
            None
        }
    }

    fn check_model(&mut self, id: &str, span: Span) {
        if let Some(exists) = self.options.model_exists
            && !exists(id)
        {
            self.diag(super::UNKNOWN_MODEL, format!("model \"{id}\" is not in the moc model catalog"), span);
        }
    }

    fn model_call(&mut self, call: &'a CallExpression<'a>, args: &[&'a Expression<'a>], open: Option<u32>) {
        let id = self.add_site(SiteKind::Model, call, args.first().and_then(|a| literal_string(a)), None, None);
        self.insert_at(open, &id);
        match args.first().and_then(|a| literal_string(a)) {
            Some(model) if !model.is_empty() => {
                self.check_model(&model, args[0].span());
                self.model_refs.push(ModelRef { ref_id: id, model_id: model });
            }
            _ => self.diag(super::NON_LITERAL_MODEL, "model() requires a non-empty literal id", args.first().map_or(call.span, |a| a.span())),
        }
        self.walk_exprs(args);
    }

    fn hole_call(&mut self, call: &'a CallExpression<'a>, args: &[&'a Expression<'a>], open: Option<u32>) {
        // hole 名必须非空字面量 ≤128。
        let name = args.first().and_then(|a| literal_string(a));
        match &name {
            Some(n) if n.is_empty() => {
                self.diag(super::NON_LITERAL_NAME, "hole name must be a non-empty literal", args[0].span());
            }
            Some(n) if n.chars().count() > facade::NAME_MAX_LEN => {
                self.diag(super::NON_LITERAL_NAME, format!("hole name exceeds {} characters", facade::NAME_MAX_LEN), args[0].span());
            }
            Some(n) => {
                if let Some(first) = self.hole_names.get(n) {
                    let (line, col) = line_col(self.source, first.start);
                    self.diag(super::HOLE_NAME_DUP, format!("hole name \"{n}\" conflicts with a hole or phase declared at L{line}:C{col}"), args[0].span());
                } else {
                    self.hole_names.insert(n.clone(), args[0].span());
                }
                self.phase_sites.entry(format!("hole:{n}")).or_default();
            }
            None => {
                self.diag(super::NON_LITERAL_NAME, "hole name must be a non-empty literal", args.first().map_or(call.span, |a| a.span()));
            }
        }
        let type_arg = call
            .type_arguments
            .as_ref()
            .and_then(|t| t.params.first());
        let id = self.add_site(SiteKind::Hole, call, name.clone(), None, type_arg);
        self.insert_at(open, &id);
        // 注入 evaluate 实参：(__src)=>eval(__src)，在 prompt 位之后、body 之前。
        let body_arg = args
            .last()
            .filter(|a| matches!(a, Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)));
        if let Some(body) = body_arg {
            if args.len() == 2 {
                self.inserts.push((body.span().start, "hole", "undefined, (__src)=>eval(__src), ".into()));
            } else {
                self.inserts.push((body.span().start, "hole", "(__src)=>eval(__src), ".into()));
            }
        } else {
            let close = call.span.end - 1;
            if args.len() >= 2 {
                self.inserts.push((close, "hole", ", (__src)=>eval(__src)".into()));
            } else {
                self.inserts.push((close, "hole", ", undefined, (__src)=>eval(__src)".into()));
            }
        }
        self.walk_exprs(args);
    }

    fn member_call(&mut self, call: &'a CallExpression<'a>, m: &'a StaticMemberExpression<'a>, args: &[&'a Expression<'a>]) {
        let member = m.property.name.as_str();
        // `x.ask(...)`：receiver 可解析到 agent() 产出 → ask 站点。
        if member == "ask" {
            let type_arg = call
                .type_arguments
                .as_ref()
                .and_then(|t| t.params.first());
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
                    let id = self.add_site(SiteKind::WorldRead, call, Some(format!("files.{member}")), None, None);
                    self.insert_at(self.open_paren(call), &id);
                    self.rewrite_member_base(base);
                    self.walk_exprs(args);
                    return;
                }
                "git" if GIT_MEMBERS.contains(&member) => {
                    let id = self.add_site(SiteKind::WorldRead, call, Some(format!("git.{member}")), None, None);
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
                    self.diag(super::INDIRECT_FACADE, format!("`{base_name}.{member}` is not a workflow facade member"), m.span);
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

    fn computed_member_call(&mut self, call: &'a CallExpression<'a>, m: &'a ComputedMemberExpression<'a>, args: &[&'a Expression<'a>]) {
        // files["read"](…) 等 computed 访问不属直接调用形态 → INDIRECT_FACADE。
        if let Expression::Identifier(base) = &m.object
            && self.is_global(base)
            && OBJECTS.contains(&base.name.as_str())
        {
            self.diag(super::INDIRECT_FACADE, format!("`{}` member calls must use direct dot syntax", base.name), m.span);
            return;
        }
        self.walk_expr(&call.callee);
        self.walk_exprs(args);
    }

    /// `__wf.` 前缀：改基对象文本（files/git/world/artifact → __wf.*）。
    fn rewrite_member_base(&mut self, base: &'a IdentifierReference<'a>) {
        self.inserts.push((base.span.start, "base", "__wf.".into()));
    }

    fn world_run_call(&mut self, call: &'a CallExpression<'a>, args: &[&'a Expression<'a>]) {
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
            _ => self.diag(super::NON_LITERAL_COMMAND, "world.run's first argument must be a compile-time string literal", args.first().map_or(call.span, |a| a.span())),
        }
        self.walk_exprs(args);
    }

    fn artifact_call(&mut self, call: &'a CallExpression<'a>, member: &'a str, args: &[&'a Expression<'a>], base: &'a IdentifierReference<'a>) {
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
            if content { SiteKind::ArtifactPublish } else { SiteKind::ArtifactDecl },
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
                    self.diag(super::ARTIFACT_ID, format!("artifact id \"{id_str}\" must match [A-Za-z0-9_.-]{{1,64}}"), args[0].span());
                } else {
                    let member_name = MEMBER.iter().find(|(m, _)| *m == member).unwrap().1;
                    if let Some(prev) = self.artifact_members.get(&id_str) {
                        if *prev != member_name {
                            self.diag(super::ARTIFACT_ID, format!("artifact id \"{id_str}\" is used with both `{prev}` and `{member_name}`"), args[0].span());
                        }
                    } else {
                        self.artifact_members.insert(id_str, member_name);
                    }
                }
                if !content && !self.is_top_level() {
                    self.diag(super::ARTIFACT_ID, format!("preset artifact `{member}` declarations must be top-level statements"), call.span);
                }
            }
            None => self.diag(super::ARTIFACT_ID, "artifact id must be a compile-time literal", args.first().map_or(call.span, |a| a.span())),
        }
        self.walk_exprs(args);
    }

    /// 预置 artifact 只允许顶层语句声明。
    fn is_top_level(&self) -> bool {
        self.block_depth == 0 && self.fn_stack.is_empty()
    }

    /// 顶层 facade 调用名 → `__wf.<name>(...)`。
    fn rewrite_callee(&mut self, callee: &'a IdentifierReference<'a>) {
        self.inserts.push((callee.span.start, "base", "__wf.".into()));
    }

    /// `x?.y()` 链：callee 或 member。
    fn walk_chain(&mut self, el: &'a ChainElement<'a>) {
        match el {
            ChainElement::CallExpression(call) => self.call(call),
            ChainElement::TSNonNullExpression(e) => {
                self.walk_expr(&e.expression);
                self.blank.push(Span::new(e.expression.span().end, e.span.end));
            }
            other => {
                if let Some(m) = other.as_member_expression() {
                    self.walk_expr(m.object());
                }
            }
        }
    }

    /// `x` 是否解析为 agent() 调用产出（ask receiver 判定）。
    fn resolves_to_agent(&self, expr: &'a Expression<'a>) -> bool {
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

fn receiver_label(expr: &Expression<'_>) -> Option<String> {
    match expr {
        Expression::CallExpression(c) => c
            .arguments
            .first()
            .and_then(|a| literal_string(a.to_expression())),
        Expression::Identifier(i) => Some(i.name.to_string()),
        _ => None,
    }
}

pub(crate) fn valid_artifact_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

fn prop_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(i) => Some(i.name.to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        _ => None,
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// —— 后置：全局检查、相位传播、边推导、降级装配 ——

impl<'a> Walker<'a> {
    /// 未解析引用的全局白名单检查（FORBIDDEN_GLOBAL）。
    fn check_globals(&mut self, scoping: &'a oxc_semantic::Scoping) {
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
                        super::FORBIDDEN_GLOBAL,
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
    fn fn_phases(&self) -> HashMap<SymbolId, BTreeSet<String>> {
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
    fn fanout_fns(&self) -> HashSet<SymbolId> {
        let mut set: HashSet<SymbolId> = HashSet::new();
        for _ in 0..self.fn_calls.len() + 1 {
            let mut changed = false;
            for (callee, calls) in &self.fn_calls {
                if set.contains(callee) {
                    continue;
                }
                if calls.iter().any(|(call, _, caller, in_loop)| {
                    *in_loop
                        || self.loop_bodies.iter().any(|b| b.contains_inclusive(call.span))
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
    fn agent_site_of(&self, site_by_start: &HashMap<u32, usize>, expr: &'a Expression<'a>) -> Option<String> {
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
            Expression::AwaitExpression(a) => return self.agent_site_of(site_by_start, &a.argument),
            Expression::TSAsExpression(e) => return self.agent_site_of(site_by_start, &e.expression),
            Expression::TSSatisfiesExpression(e) => return self.agent_site_of(site_by_start, &e.expression),
            Expression::TSTypeAssertion(e) => return self.agent_site_of(site_by_start, &e.expression),
            Expression::TSNonNullExpression(e) => return self.agent_site_of(site_by_start, &e.expression),
            _ => return None,
        };
        if !matches!(&call.callee, Expression::Identifier(i) if i.name == "agent" && self.is_global(i)) {
            return None;
        }
        site_by_start.get(&call.span.start).map(|i| self.sites[*i].id.clone())
    }

    fn finish_plan(mut self, source: &str) -> Result<CompiledWorkflow, Vec<Diagnostic>> {
        let site_by_start: HashMap<u32, usize> =
            self.sites.iter().enumerate().map(|(i, s)| (s.span.start, i)).collect();
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
                        code: super::PHASE_REQUIRED,
                        message: "this call is not covered by any phase(); every ask and world.run must run inside a phase".into(),
                        line,
                        col,
                    }
                });
            }
            if site.fan_out
                || site.in_fn.is_some_and(|f| fanout.contains(&f))
            {
                site.fan_out = true;
            }
            site.phase = phases.first().cloned();
            for p in &phases {
                self.phase_sites.entry(p.clone()).or_default().push(site.id.clone());
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
                && !self.loop_bodies.iter().any(|b| b.contains_inclusive(site.span))
                && self.literal_agents.contains_key(lit)
            {
                let (line, col) = line_col(source, site.span.start);
                self.diagnostics.push(Diagnostic {
                    code: super::FANOUT_LITERAL_NAME,
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
                self.ask_sites_by_agent.entry(agent_id).or_default().push(site.id.clone());
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
                self.roles[*role_idx].model_source =
                    ModelSource::Routed { refs: vec![self.sites[*site_idx].id.clone()] };
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
                    code: super::PHASE_EMPTY,
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
                SiteKind::Ask | SiteKind::WorldRun | SiteKind::WorldRead | SiteKind::ArtifactPublish | SiteKind::Hole
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
                        if from_phase != *to_phase && seen.insert((from_phase.clone(), to_phase.clone(), EdgeKind::Data)) {
                            edges.push(PlanEdge { from: from_phase.clone(), to: to_phase.clone(), kind: EdgeKind::Data });
                        }
                    }
                }
            }
            if opaque {
                for to_phase in &to_phases {
                    let my_first = phase_first_site.get(to_phase).copied().unwrap_or(0);
                    for (name, first) in &phase_first_site {
                        if name != to_phase && *first < my_first
                            && seen.insert((name.clone(), to_phase.clone(), EdgeKind::Order))
                        {
                            edges.push(PlanEdge { from: name.clone(), to: to_phase.clone(), kind: EdgeKind::Order });
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
                                code: super::SCHEMA_UNREPRESENTABLE,
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
                .map(|(command, sites)| super::CommandSite { command, sites })
                .collect(),
            schemas,
        };
        let lowered = super::lower::lower(source, &self.blank, &self.inserts, &self.user_owns_args_run);
        Ok(CompiledWorkflow { lowered, plan })
    }
}

// —— 依赖追踪：表达式 → 产出站点集合 + 不可追踪标记 ——

struct Trace<'w, 'a> {
    w: &'w Walker<'a>,
    site_by_start: &'w HashMap<u32, usize>,
    depth: usize,
    visiting: HashSet<SymbolId>,
    /// 内联分析时的形参→实参替换。
    sub: Vec<(SymbolId, &'a Expression<'a>)>,
}

type Deps = (BTreeSet<String>, bool);

impl<'w, 'a> Trace<'w, 'a> {
    fn site_at_span(&self, span: Span) -> Option<String> {
        self.site_by_start.get(&span.start).map(|i| self.w.sites[*i].id.clone())
    }

    fn expr(&mut self, expr: &'a Expression<'a>) -> Deps {
        if self.depth > 8 {
            return (BTreeSet::new(), true);
        }
        self.depth += 1;
        let out = self.expr_inner(expr);
        self.depth -= 1;
        out
    }

    fn expr_inner(&mut self, expr: &'a Expression<'a>) -> Deps {
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
            Expression::YieldExpression(y) => y.argument.as_ref().map_or((BTreeSet::new(), false), |a| self.expr(a)),
            // 函数字面量：被调用时经内联分析处理，本身不携带依赖。
            Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_) | Expression::ClassExpression(_) => {
                (BTreeSet::new(), false)
            }
            _ => (BTreeSet::new(), false),
        }
    }

    fn all(&mut self, it: impl Iterator<Item = &'a Expression<'a>>) -> Deps {
        let mut deps = BTreeSet::new();
        let mut opaque = false;
        for e in it {
            let (d, o) = self.expr(e);
            deps.extend(d);
            opaque |= o;
        }
        (deps, opaque)
    }

    fn all2(&mut self, a: &'a Expression<'a>, b: &'a Expression<'a>) -> Deps {
        let (d1, o1) = self.expr(a);
        let (d2, o2) = self.expr(b);
        (d1.union(&d2).cloned().collect(), o1 || o2)
    }

    fn all3(&mut self, a: &'a Expression<'a>, b: &'a Expression<'a>, c: &'a Expression<'a>) -> Deps {
        let (d1, o1) = self.expr(a);
        let (d2, o2) = self.all2(b, c);
        (d1.union(&d2).cloned().collect(), o1 || o2)
    }

    fn ident(&mut self, ident: &'a IdentifierReference<'a>) -> Deps {
        let Some(sym) = scoping_symbol(self.w.scoping, ident) else {
            return (BTreeSet::new(), false); // 全局名（args/run/内建）——值非站点产出
        };
        // 内联形参替换优先。
        if let Some(arg) = self.sub.iter().find(|(s, _)| *s == sym).map(|(_, a)| *a) {
            return self.expr(arg);
        }
        self.symbol(sym)
    }

    fn symbol(&mut self, sym: SymbolId) -> Deps {
        if !self.visiting.insert(sym) {
            return (BTreeSet::new(), false); // 循环引用：断开
        }
        let out = self.symbol_inner(sym);
        self.visiting.remove(&sym);
        out
    }

    fn symbol_inner(&mut self, sym: SymbolId) -> Deps {
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
    fn iter_sym_of(&self, iter: &'a Expression<'a>) -> Option<SymbolId> {
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

    fn call(&mut self, call: &'a CallExpression<'a>) -> Deps {
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

    fn inline_fn(&mut self, func: &'a Function<'a>, call: &'a CallExpression<'a>, sym: SymbolId) -> Deps {
        if !self.visiting.insert(sym) {
            return (BTreeSet::new(), false);
        }
        let out = (|| {
            let Some(body) = &func.body else { return (BTreeSet::new(), true) };
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

    fn inline_arrow(&mut self, f: &'a ArrowFunctionExpression<'a>, call: &'a CallExpression<'a>) -> Deps {
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
fn collect_returns<'a>(stmt: &'a Statement<'a>, f: &mut impl FnMut(&'a Expression<'a>)) {
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
