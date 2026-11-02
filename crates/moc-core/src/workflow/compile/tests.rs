//! 编译器单测：诊断正反例、plan 推导、schema、降级可运行性。

use super::*;

fn compile_ok(source: &str) -> CompiledWorkflow {
    match compile(source, &CompileOptions::default()) {
        Ok(c) => c,
        Err(diags) => panic!("expected compile to succeed, got {diags:?}"),
    }
}

fn compile_err(source: &str) -> Vec<Diagnostic> {
    match compile(source, &CompileOptions::default()) {
        Ok(_) => panic!("expected diagnostics, compiled: {source}"),
        Err(d) => d,
    }
}

fn codes(diags: &[Diagnostic]) -> Vec<&'static str> {
    diags.iter().map(|d| d.code).collect()
}

/// 合法基底：phase 内含一个 ask，避免 PHASE_EMPTY 干扰目标诊断。
const ASK: &str = "const a = agent(); await a.ask(\"hi\");";
const MINIMAL: &str = "phase(\"work\"); const a = agent(\"w\"); const r = await a.ask(\"hi\");";

#[test]
fn minimal_compiles() {
    let c = compile_ok(MINIMAL);
    assert!(c.plan.sites.iter().any(|s| s.kind == SiteKind::Actor));
    assert!(c.plan.sites.iter().any(|s| s.kind == SiteKind::Ask));
    assert!(c.lowered.contains("__wf.agent"));
    assert!(c.lowered.contains("__wf.phase"));
    assert!(c.lowered.contains("\"actor#1\""));
    assert!(c.lowered.contains("\"ask#1\""));
    assert!(c.lowered.starts_with("const {args, run} = __wf;"));
}

#[test]
fn forbidden_import_export() {
    for src in [
        "import x from 'y';",
        "export const a = 1;",
        "declare const x: number;",
        "declare function f(): void;",
        "declare class C {}",
        "with (o) { x = 1; }",
    ] {
        let d = compile_err(src);
        assert!(codes(&d).contains(&FORBIDDEN_SYNTAX), "{src}");
    }
}

#[test]
fn forbidden_globals() {
    for src in [
        "process.env.X;",
        "fetch('http://x');",
        "require('x');",
        "Date.now();",
        "Math.random();",
        "new Date();",
        "performance.now();",
    ] {
        let d = compile_err(src);
        assert!(
            codes(&d).contains(&FORBIDDEN_GLOBAL),
            "expected FORBIDDEN_GLOBAL for {src}, got {d:?}"
        );
    }
    // ES2022 全局可用。
    compile_ok(&format!(
        "phase(\"w\"); const x = Math.floor(1.5) + JSON.stringify([1,2]); {ASK}"
    ));
}

#[test]
fn ts_unsupported() {
    assert!(codes(&compile_err("enum E { A }")).contains(&TS_UNSUPPORTED));
    assert!(codes(&compile_err("namespace N {}")).contains(&TS_UNSUPPORTED));
    assert!(codes(&compile_err("class C { constructor(private x: number) {} }")).contains(&TS_UNSUPPORTED));
    // declare enum 是 declare → FORBIDDEN_SYNTAX
    assert!(codes(&compile_err("declare enum E { A }")).contains(&FORBIDDEN_SYNTAX));
}

#[test]
fn top_level_await_and_return() {
    compile_ok("phase(\"w\"); await world.run(\"true\"); await Promise.resolve(); return 1;");
}

#[test]
fn dup_agent_name() {
    let d = compile_err(&format!("phase(\"w\"); agent(\"x\"); agent(\"x\"); {ASK}"));
    assert!(codes(&d).contains(&DUP_AGENT_NAME));
    compile_ok(&format!("phase(\"w\"); agent(\"x\"); agent(\"y\"); {ASK}"));
}

#[test]
fn fanout_literal_name() {
    let d = compile_err(&format!("phase(\"w\"); for (let i = 0; i < 2; i++) agent(\"x\"); {ASK}"));
    assert!(codes(&d).contains(&FANOUT_LITERAL_NAME));
    compile_ok(&format!("phase(\"w\"); for (let i = 0; i < 2; i++) agent(`w-${{i}}`); {ASK}"));
    // 经函数传播的 fan-out。
    let d = compile_err(&format!(
        "phase(\"w\"); function mk() {{ agent(\"x\"); }} for (let i = 0; i < 2; i++) mk(); {ASK}"
    ));
    assert!(codes(&d).contains(&FANOUT_LITERAL_NAME));
}

#[test]
fn non_literal_command() {
    let d = compile_err("phase(\"w\"); const c = \"ls\"; await world.run(c);");
    assert!(codes(&d).contains(&NON_LITERAL_COMMAND));
    compile_ok("phase(\"w\"); await world.run(\"ls\", [\"-la\"]);");
}

#[test]
fn non_literal_model() {
    let d = compile_err(&format!("phase(\"w\"); const m = \"m\"; agent(\"a\", {{model: m}}); {ASK}"));
    assert!(codes(&d).contains(&NON_LITERAL_MODEL));
    compile_ok(&format!("phase(\"w\"); agent(\"a\", {{model: \"gpt-4\"}}); {ASK}"));
    // model() 产出的 ModelRef（直接或变量绑定）合法。
    let c = compile_ok(&format!("phase(\"w\"); const r = model(\"m1\"); agent(\"a\", {{model: r}}); {ASK}"));
    match &c.plan.roles[0].model_source {
        ModelSource::Routed { refs } => assert_eq!(refs, &["model#1"]),
        other => panic!("expected routed model_source, got {other:?}"),
    }
}

#[test]
fn phase_required() {
    let d = compile_err("const a = agent(); await a.ask(\"x\");");
    assert!(codes(&d).contains(&PHASE_REQUIRED));
    compile_ok("phase(\"w\"); const x = await world.run(\"ls\");");
    // 函数内经调用点相位覆盖。
    compile_ok(&format!(
        "function work() {{ return agent().ask(\"x\"); }} phase(\"w\"); work(); {ASK}"
    ));
}

#[test]
fn phase_empty() {
    let d = compile_err(&format!("phase(\"w\"); phase(\"empty\"); {ASK}"));
    assert!(codes(&d).contains(&PHASE_EMPTY));
}

#[test]
fn hole_name_dup() {
    let d = compile_err(&format!("phase(\"w\"); hole<null>(\"h\"); hole<null>(\"h\"); {ASK}"));
    assert!(codes(&d).contains(&HOLE_NAME_DUP));
}

#[test]
fn indirect_facade() {
    for src in [
        "phase(\"w\"); const f = agent; f();",
        "phase(\"w\"); const r = files.read; r(\"x\");",
        "phase(\"w\"); [agent].map(f => f());",
        "phase(\"w\"); files[\"read\"](\"x\");",
    ] {
        let d = compile_err(src);
        assert!(codes(&d).contains(&INDIRECT_FACADE), "expected INDIRECT_FACADE for {src}");
    }
    // phase 必须是独立语句。
    let d = compile_err("const p = phase(\"w\");");
    assert!(codes(&d).contains(&INDIRECT_FACADE));
    // agent() 直接调用 + 变量别名均合法。
    compile_ok(&format!("phase(\"w\"); const mk = () => agent(); const a2 = mk(); await a2.ask(\"x\"); {ASK}"));
}

#[test]
fn artifact_id_checks() {
    let d = compile_err(&format!("phase(\"w\"); await artifact.file(\"bad id!\", \"x.txt\"); {ASK}"));
    assert!(codes(&d).contains(&ARTIFACT_ID));
    compile_ok(&format!("phase(\"w\"); await artifact.file(\"ok-id\", \"x.txt\"); {ASK}"));
    // 同 id 不同成员。
    let d = compile_err(&format!(
        "phase(\"w\"); await artifact.file(\"dup\", \"x\"); artifact.chart(\"dup\", {{x:{{field:\"a\"}},y:{{field:\"b\"}}}}); {ASK}"
    ));
    assert!(codes(&d).contains(&ARTIFACT_ID));
}

#[test]
fn schema_synthesis() {
    let c = compile_ok(
        r#"
interface Finding {
  /** 文件路径 */
  path: string;
  score?: number;
  kind: "bug" | "nit" | 1;
}
phase("work");
const a = agent();
const f = await a.ask<Finding>("find");
"#,
    );
    let (_, schema) = c.plan.schemas.iter().find(|(id, _)| id.starts_with("ask")).unwrap();
    let f = &schema["$defs"]["Finding"];
    assert_eq!(f["type"], "object");
    assert_eq!(f["properties"]["path"]["type"], "string");
    assert_eq!(f["properties"]["path"]["description"], "文件路径");
    assert_eq!(f["properties"]["kind"]["enum"], serde_json::json!(["bug", "nit", 1]));
    assert_eq!(f["required"], serde_json::json!(["path", "kind"]));
}

#[test]
fn schema_unrepresentable() {
    let d = compile_err(&format!(
        "phase(\"w\"); const a = agent(); await a.ask<Map<string, number>>(\"x\"); {ASK}"
    ));
    assert!(codes(&d).contains(&SCHEMA_UNREPRESENTABLE));
}

#[test]
fn phase_edges_derivation() {
    let c = compile_ok(
        r#"
phase("gather");
const a = agent();
const data = await world.run("cat", ["in.txt"]);
phase("analyze");
const r = await a.ask(`analyze ${data.stdout}`);
"#,
    );
    assert!(
        c.plan
            .edges
            .iter()
            .any(|e| e.from == "gather" && e.to == "analyze" && e.kind == EdgeKind::Data),
        "expected data edge gather→analyze, got {:?}",
        c.plan.edges
    );
    // 同一 phase 内不重复连边；跨 phase 的另一条数据边。
    let c2 = compile_ok(
        r#"
phase("one");
const a = agent("a");
const r1 = await a.ask("q1");
phase("two");
const b = agent("b");
const r2 = await b.ask(`q2 ${r1}`);
"#,
    );
    assert!(c2.plan.edges.iter().any(|e| e.from == "one" && e.to == "two"));
    // actor receiver 也构成数据边（actor 站点的 phase → ask 的 phase）。
    assert!(c2.plan.roles.iter().any(|r| r.ask_sites.len() == 1));
}

#[test]
fn roles_ask_sites() {
    let c = compile_ok("phase(\"w\"); const a = agent(\"x\"); await a.ask(\"one\"); await a.ask(\"two\");");
    let role = &c.plan.roles[0];
    assert_eq!(role.role_id, "actor#1");
    assert_eq!(role.name_pattern.as_deref(), Some("x"));
    assert_eq!(role.ask_sites.len(), 2);
}

#[test]
fn hole_rewriting() {
    let c = compile_ok(&format!(
        "phase(\"w\"); const v = await hole<string>(\"q\", \"need x\"); {ASK}"
    ));
    assert!(c.lowered.contains("(__src)=>eval(__src)"));
    let c = compile_ok(&format!(
        "phase(\"w\"); const v = await hole<number>(\"q\", \"p\", async () => 1); {ASK}"
    ));
    assert!(c.lowered.contains("(__src)=>eval(__src), async () => 1"));
}

#[test]
fn type_erasure_strips_all_constructs() {
    // as / satisfies / 非空断言 / 尖括号断言 / 泛型实参 / 可选参数 / declare 属性全部剥离。
    let c = compile_ok(
        r#"
interface Opt { a?: string; }
type X = { n: number };
phase("w");
const a = agent("a");
const o = {} as Opt;
const s = "x" satisfies string;
const n = (null as X | null)!;
const v = <number>JSON.parse("1");
const g = Array.from<string>(["a"]);
const r = await a.ask<Opt>("q");
function f(p?: number) { return p ?? 0; }
f();
"#,
    );
    for tok in [" as ", "satisfies", "<number>", "p?:", "!", "interface", "type X"] {
        assert!(!c.lowered.contains(tok), "lowered still contains {tok:?}");
    }
    // 类型标注/断言位置被空白化但源码行数不变。
    assert_eq!(c.lowered.lines().count(), 13);
}
