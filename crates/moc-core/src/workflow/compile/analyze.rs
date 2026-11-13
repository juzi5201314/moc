use super::{
    CompileOptions, CompiledWorkflow, Diagnostic, ModelRef, PhaseNode, Role, SiteKind, line_col,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::{SemanticBuilder, SymbolId};
use oxc_span::Span;
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
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "Atomics",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "Error",
    "escape",
    "eval",
    "EvalError",
    "FinalizationRegistry",
    "Float32Array",
    "Float64Array",
    "Function",
    "globalThis",
    "Infinity",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "Intl",
    "isFinite",
    "isNaN",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "parseFloat",
    "parseInt",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "SharedArrayBuffer",
    "String",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "undefined",
    "unescape",
    "URIError",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    // 常用 TS 工具类型名，出现在类型位时 fallback 兜底。
    "Partial",
    "Required",
    "Readonly",
    "Pick",
    "Omit",
    "Record",
    "Exclude",
    "Extract",
    "Awaited",
    "ReturnType",
    "Parameters",
    "NonNullable",
    "Uppercase",
    "Lowercase",
    "Capitalize",
    "Uncapitalize",
    "InstanceType",
    "ConstructorParameters",
    "Iterable",
    "Iterator",
    "IterableIterator",
    "AsyncIterable",
    "AsyncIterator",
    "PromiseLike",
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
                error
                    .labels
                    .as_ref()
                    .and_then(|l| l.first())
                    .map_or(0, |l| l.offset()) as u32,
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
type FnCall<'a> = (
    &'a CallExpression<'a>,
    Option<String>,
    Option<SymbolId>,
    bool,
);

#[derive(Clone, Copy)]
pub(super) enum TypeDecl<'a> {
    Interface(&'a TSInterfaceDeclaration<'a>),
    Alias(&'a TSTypeAliasDeclaration<'a>),
}

// —— AST 结构别名 ——
type Stmts<'a> = &'a [Statement<'a>];

mod checks;
mod plan;
mod sites;
mod walk;

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

/// 字面量字符串实参的求值（StringLiteral / 无插值 TemplateLiteral / 简单字符串拼接）。
fn literal_string<'a>(expr: &'a Expression<'a>) -> Option<String> {
    match expr {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            Some(t.quasis[0].value.cooked.as_ref()?.to_string())
        }
        Expression::ParenthesizedExpression(p) => literal_string(&p.expression),
        Expression::BinaryExpression(b) if b.operator == BinaryOperator::Addition => {
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
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
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

mod trace;
