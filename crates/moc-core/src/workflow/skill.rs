//! 内置 `workflow-authoring` skill：仅激活会话的 `<skills>` 目录出现。
//!
//! 正文=判断层（自己的话）+ verbatim `FACADE_DTS` + 工具字段说明 + 诊断码表；
//! `patterns.md` 存放 ≥10 个可编译范式（测试保证全部通过编译器无诊断）。

use crate::workflow::facade::FACADE_DTS;

pub const SKILL_NAME: &str = "workflow-authoring";

/// skill frontmatter 描述（`<skills>` 目录里的单行说明）。
pub const SKILL_DESCRIPTION: &str =
    "Author and run multi-actor TypeScript workflows (agents, phases, asks, world.run gates, artifacts, holes, resume/amend)";

/// SKILL.md 正文（模型可见，英文）。
pub fn skill_body() -> String {
    format!(
        r#"# workflow-authoring

Dynamic workflows are TypeScript scripts that orchestrate named LLM actors with phases,
structured asks, sandboxed world commands, artifacts, and resumable journals.

## When to use workflows

- The user explicitly asks for a workflow, pipeline, or multi-agent orchestration.
- The task needs multiple named actors, staged phases, or gated shell commands.
- Otherwise prefer `spawn_agent` or do the task yourself — workflows have real cost.

## Discipline

- Always load THIS skill before authoring a script (`create_workflow`/`amend_workflow`
  refuse scripts without it).
- `create_workflow`/`amend_workflow` submit for human approval; the run then executes
  in the background — poll `workflow_status` or wait for the completion notice.
- Scripts are deterministic: no `Date.now`, `Math.random`, `performance`, `new Date`,
  no imports/exports, no node/bun globals. Values you need must come from `args`,
  `ask`, or `world`.
- `world.run` commands must be literal; the approver sees the command set up front.
- `hole()` pauses the run until a human supplies a function body; use for points
  where only a person can decide the algorithm.

## Facade reference (verbatim)

```ts
{FACADE_DTS}
```

## Tool fields

- `create_workflow(script|saved|path, args?, max_concurrency?, subagent_model?)` —
  compile, request approval, then launch in background; returns `run_id` + draft path.
- `amend_workflow(run_id, script?|path?, subagent_model?)` — new run importing matching
  prior asks (actor name + persona + identical instructions, prefix only).
- `resume_workflow_run(run_id)` — replay journaled asks/ops at zero cost; a script
  change that shifts replay hashes errors the run (amend instead).
- `stop_workflow_run(run_id)` — user stop (resumable).
- `fill_workflow_hole(run_id, hole, body)` — fill an open hole; new `world.run`
  commands trigger an incremental approval.
- `save_workflow(name, run_id?|path?, description, args?, scope?)` — save under
  `.moc/workflows/` (project) or `~/.moc/workflows/` (global).
- `list_saved_workflows()` — list saved workflows.
- `workflow_status(run_id?)` — run status, phases, roles, usage.
- `eval_workflow_snippet(code|path, timeout_secs?)` — synchronous eval without
  agents (`ask`/`agent`/`hole` fail); `world/files/git` run for real. Default 60s,
  max 600s.

## Diagnostics

Every diagnostic is `LINE:COL CODE message`. Codes: forbidden syntax/globals
(`E_FORBIDDEN_SYNTAX`, `E_FORBIDDEN_GLOBAL`), structure (`E_NO_PHASE`, `E_PHASE_EMPTY`,
`E_DUPLICATE_NAME`, `E_DUPLICATE_ROLE`), artifact/contract (`E_ARTIFACT`,
`E_NONLITERAL_MODEL`, `E_NONLITERAL_COMMAND`, `E_SCHEMA`), unsupported TS with runtime
semantics (`E_TS_RUNTIME`), facade misuse (`E_FACADE`).

See `patterns.md` next to this skill for ready-made compilable patterns.
"#,
        FACADE_DTS = FACADE_DTS
    )
}

/// 把内置 skill 物化到 `<home>/builtin-skills/workflow-authoring/`（幂等），
/// 返回目录路径。模型经 `skill` 工具读真实文件；`dir=` 指向含 patterns.md 的目录。
pub fn materialize(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home.join("builtin-skills").join(SKILL_NAME);
    let skill_md = format!(
        "---\nname: {SKILL_NAME}\ndescription: |\n  {SKILL_DESCRIPTION}\nmodel_invocable: true\n---\n{}",
        skill_body()
    );
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("SKILL.md"), skill_md);
        let _ = std::fs::write(dir.join("patterns.md"), PATTERNS_MD);
    }
    dir
}

/// 给已激活会话的索引注入内置 skill（不激活则索引原样——零泄漏）。
pub fn inject(index: &mut crate::skills::SkillIndex, home: &std::path::Path) {
    index.insert_builtin(crate::skills::SkillMeta {
        name: SKILL_NAME.into(),
        description: SKILL_DESCRIPTION.into(),
        when_to_use: Some(
            "when the user asks for a workflow, multi-agent orchestration, or you need create/amend/resume_workflow tools".into(),
        ),
        globs: Vec::new(),
        model_invocable: true,
        base_dir: materialize(home),
        source: "builtin".into(),
    });
}

/// patterns.md：≥10 个范式；测试逐段编译保证无诊断。
pub const PATTERNS_MD: &str = r#"# Workflow patterns

## 1. Single actor, single ask

```ts
phase("draft");
const w = agent("writer", { system: "You write terse release notes." });
const notes = await w.ask("Summarize the milestone in three bullets.");
return notes;
```

## 2. Two actors, sequential phases

```ts
phase("draft");
const w = agent("writer", { system: "Write carefully." });
const draft = await w.ask("Draft the paragraph.");
phase("review");
const r = agent("reviewer", { system: "Review harshly." });
const verdict = await r.ask(`Review: ${draft}`);
return verdict;
```

## 3. Structured ask with schema

```ts
interface Score { points: number; label: string; }
phase("score");
const j = agent("judge", { system: "Score strictly." });
const s = await j.ask<Score>("Score this patch 0-10.");
return s.points;
```

## 4. args gate + conditional phases

```ts
phase("plan");
const p = agent("planner", { system: "Plan." });
const plan = await p.ask(`Plan for: ${args.topic}`);
if (args.execute) {
  phase("do");
  const x = agent("doer", { system: "Execute." });
  await x.ask(`Execute: ${plan}`);
}
return plan;
```

## 5. world.run gate

```ts
phase("test");
const out = await world.run("cargo", ["test"], { output: { tail: 40 } });
const t = agent("fixer", { system: "Fix tests." });
if (out.exit_code !== 0) {
  await t.ask(`Fix these failures: ${out.stdout}`);
}
return out.exit_code;
```

## 6. files + git reads

```ts
phase("inspect");
const files = await files.glob("src/**/*.rs");
const diff = await git.diff();
const a = agent("auditor", { system: "Audit." });
return a.ask(`Audit ${files.length} files; diff: ${diff.stdout}`);
```

## 7. Model routing per role

```ts
phase("work");
const fast = model("fast");
const deep = model("deep");
const a = agent("quick", { system: "Fast answers.", model: fast });
const b = agent("slow", { system: "Deep answers.", model: deep });
const px = future(() => a.ask("quick?"));
const py = future(() => b.ask("deep?"));
const [x, y] = await Promise.all([px, py]);
return { x, y };
```

## 8. Parallel asks within a phase

```ts
phase("vote");
const v = agent("voter", { system: "Vote." });
const votes = await Promise.all([
  future(() => v.ask("Vote A or B? Case 1")),
  future(() => v.ask("Vote A or B? Case 2")),
  future(() => v.ask("Vote A or B? Case 3")),
]);
return votes;
```

## 9. Reports and artifacts

```ts
phase("publish");
const w = agent("writer", { system: "Write docs." });
const doc = await w.ask("Write the guide.");
report({ title: "Guide ready" });
artifact.markdown("guide", doc);
return doc;
```

## 10. Channel fan-out/fan-in

```ts
phase("fan");
const results = channel<string>("results");
const w = agent("worker", { system: "Work." });
future(async () => {
  const r = await w.ask("Do piece one");
  results.send(r);
});
future(async () => {
  const r = await w.ask("Do piece two");
  results.send(r);
});
const a = await results.recv();
const b = await results.recv();
return [a, b];
```

## 11. Hole for human-in-the-loop algorithms

```ts
phase("decide");
const s = agent("strategist", { system: "Propose options." });
const options = await s.ask("Give two candidate strategies.");
const pick = await hole<string>("pickWinner", `Choose the winning strategy from: ${options}`);
return pick;
```
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// patterns.md 里的每个 ```ts 代码块都必须通过编译器且无诊断（规格 §7 要求 ≥10 个）。
    #[test]
    fn all_patterns_compile_clean() {
        let mut count = 0;
        let mut rest = PATTERNS_MD;
        while let Some(start) = rest.find("```ts") {
            rest = &rest[start + 5..];
            let Some(end) = rest.find("```") else {
                panic!("unterminated ```ts block");
            };
            let script = &rest[..end];
            rest = &rest[end..];
            count += 1;
            let result = crate::workflow::compile::compile(
                script,
                &crate::workflow::compile::CompileOptions::default(),
            );
            match result {
                Ok(_) => {}
                Err(diags) => panic!(
                    "pattern #{count} has diagnostics:\n{}",
                    diags
                        .iter()
                        .map(|d| format!("L{}:C{} {} {}", d.line, d.col, d.code, d.message))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
        assert!(count >= 10, "patterns.md must carry ≥10 patterns, got {count}");
    }

    /// skill 正文含 verbatim FACADE_DTS 与工具字段说明、诊断码表（规格 §7）。
    #[test]
    fn skill_body_has_contracts() {
        let body = skill_body();
        assert!(body.contains(crate::workflow::facade::FACADE_DTS));
        for tool in super::super::runtime::WORKFLOW_TOOLS {
            assert!(body.contains(tool), "skill body must document `{tool}`");
        }
        for code in ["E_FORBIDDEN_SYNTAX", "E_PHASE_EMPTY", "E_SCHEMA", "E_FACADE"] {
            assert!(body.contains(code), "skill body must list diagnostic `{code}`");
        }
    }
}
