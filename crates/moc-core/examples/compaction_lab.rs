//! 真实上下文压缩实验驱动：只通过公开会话 API 调用现有完整压缩管线。
//!
//! 用法：cargo run -p moc-core --example compaction_lab -- /absolute/spec.json
//! 录制网关、模型配置与语料规范化由外层脚本负责。本例不改写压缩策略。

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use moc_core::session::SessionTable;
use moc_core::session::entries::{Entry, SessionOrigin};
use moc_core::session::store::{SessionFile, SessionPaths, SessionStore};
use moc_core::{Core, CoreConfig};
use moc_llm::Message;
use moc_protocol::methods::*;
use moc_protocol::{ClientBuilder, ClientHandle, Server, transport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    home: PathBuf,
    cwd: PathBuf,
    session_dir: PathBuf,
    output: PathBuf,
    model: String,
    #[serde(default)]
    corpus: Option<PathBuf>,
    #[serde(default)]
    target_tokens: u64,
    #[serde(default)]
    rounds: usize,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    operation: Operation,
    #[serde(default)]
    probe: Option<String>,
    #[serde(default)]
    probe_mode: ProbeMode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeMode {
    #[default]
    Direct,
    Retrieval,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    #[default]
    Run,
    Inspect,
    Compact,
    Probe,
}

#[derive(Debug, Deserialize)]
struct Group {
    source: Value,
    messages: Vec<Message>,
}

/// 逐行读取，不因原始任务增长把全部历史复制到内存。
struct Corpus {
    lines: std::io::Lines<BufReader<File>>,
    cursor: usize,
}

impl Corpus {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            lines: BufReader::new(File::open(path)?).lines(),
            cursor: 0,
        })
    }

    fn next(&mut self) -> Result<Group> {
        let line = self
            .lines
            .next()
            .context("真实语料已耗尽，无法补足目标上下文；没有循环复用历史")??;
        let group: Group = serde_json::from_str(&line)
            .with_context(|| format!("语料第 {} 行不是有效消息组", self.cursor + 1))?;
        ensure!(!group.messages.is_empty(), "语料消息组不能为空");
        self.cursor += 1;
        Ok(group)
    }
}

#[derive(Default)]
struct Events {
    file: Option<File>,
    error: Option<String>,
}

type EventSink = Arc<Mutex<Events>>;

fn event(sink: &EventSink, kind: &str, value: &impl Serialize) {
    let mut sink = sink.lock();
    if let Some(file) = sink.file.as_mut() {
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut *file, &json!({"kind": kind, "event": value}))?;
            file.write_all(b"\n")?;
            file.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            sink.error = Some(error.to_string());
        }
    }
}

fn begin_events(sink: &EventSink, output: &Path) -> Result<()> {
    *sink.lock() = Events {
        file: Some(File::create(output.join("updates.jsonl"))?),
        error: None,
    };
    Ok(())
}

fn check_events(sink: &EventSink) -> Result<()> {
    if let Some(error) = sink.lock().error.take() {
        bail!("写入观察事件失败：{error}");
    }
    Ok(())
}

fn save(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn progress(value: Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

async fn load(client: &ClientHandle, session_id: &str) -> Result<()> {
    client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: true,
            session_id: session_id.into(),
            max_entries: Some(0),
            read_only: false,
        })
        .await?;
    Ok(())
}

async fn unload(client: &ClientHandle, session_id: &str) -> Result<()> {
    client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: session_id.into(),
        })
        .await?;
    Ok(())
}

async fn context(client: &ClientHandle, session_id: &str) -> Result<ContextUsageResult> {
    Ok(client
        .call::<SessionContext>(SessionContextParams {
            session_id: session_id.into(),
        })
        .await?)
}

async fn snapshot(
    client: &ClientHandle,
    session_id: &str,
    paths: &SessionPaths,
    output: &Path,
    stage: &str,
) -> Result<ContextUsageResult> {
    let usage = context(client, session_id).await?;
    let mut content = client
        .call::<SessionContextContent>(SessionContextContentParams {
            session_id: session_id.into(),
            ..Default::default()
        })
        .await?;
    ensure!(
        content.error.is_none(),
        "当前上下文不可编码: {:?}",
        content.error
    );
    while let Some(cursor) = content.next_cursor {
        let mut page = client
            .call::<SessionContextContent>(SessionContextContentParams {
                session_id: session_id.into(),
                view_id: Some(content.view_id.clone()),
                cursor: Some(cursor),
                limit: None,
            })
            .await?;
        content.sections.append(&mut page.sections);
        content.next_cursor = page.next_cursor;
    }
    let mut nodes = content.sections.clone();
    let mut pages = Vec::new();
    while let Some(node) = nodes.pop() {
        let mut cursor = None;
        loop {
            let page = client
                .call::<moc_protocol::methods::SessionContextRead>(
                    moc_protocol::methods::SessionContextReadParams {
                        session_id: session_id.into(),
                        view_id: content.view_id.clone(),
                        node_id: node.id.clone(),
                        cursor,
                        limit: None,
                    },
                )
                .await?;
            nodes.extend(page.children.clone());
            cursor = page.next_cursor;
            pages.push(page);
            if cursor.is_none() {
                break;
            }
        }
    }
    save(
        output.join(format!("{stage}-context.json")),
        &serde_json::json!({"view":content,"pages":pages}),
    )?;
    client
        .call::<moc_protocol::methods::SessionContextClose>(
            moc_protocol::methods::SessionContextCloseParams {
                session_id: session_id.into(),
                view_id: content.view_id,
            },
        )
        .await?;
    save(output.join(format!("{stage}-usage.json")), &usage)?;
    fs::copy(
        &paths.transcript,
        output.join(format!("{stage}-session.jsonl")),
    )?;
    copy_tree(&paths.sidecar, &output.join(format!("{stage}-sidecar")))?;
    Ok(usage)
}

async fn seed(spec: &Spec, session_id: &str) -> Result<SessionPaths> {
    let store = SessionStore::new(&spec.session_dir);
    ensure!(
        store.find(session_id).await?.is_none(),
        "实验会话已存在，请使用新的隔离目录；run 不会覆盖已有会话"
    );
    let paths = store.paths_for(&spec.cwd, session_id, 0);
    let mut header = Entry::header(
        session_id.into(),
        spec.cwd.to_string_lossy().into_owned(),
        spec.model.clone(),
        SessionEngine::Standard,
        SessionPermission::Workspace,
        SessionOrigin::root(),
        "上下文压缩实验".into(),
        0,
    );
    if let Entry::Header { id, .. } = &mut header {
        *id = "lab-header".into();
    }
    SessionFile::create_with_header(&paths.transcript, &header).await?;
    Ok(paths)
}

/// Core 已卸载后才追加；用相同消息索引生成稳定 ID，并持有真正的写租约。
async fn append_until(
    spec: &Spec,
    session_id: &str,
    corpus: &mut Corpus,
    mut tokens: u64,
    sources: &mut Vec<Value>,
) -> Result<()> {
    let table = SessionTable::new(&spec.session_dir, true);
    let record = table.get(session_id).await?;
    table
        .ensure_write_lease(session_id, &record, "compaction-lab-import")
        .await?;
    let mut parent = record.state.lock().await.last_entry_id.clone();
    let store = SessionStore::new(&spec.session_dir);
    let file = store.open(&record.paths).await?;
    loop {
        let group = corpus.next()?;
        let group_tokens = moc_core::compact::messages_tokens(&group.messages);
        sources.push(json!({"group": corpus.cursor - 1, "source": group.source,
            "estimated_tokens": group_tokens, "messages": group.messages.len()}));
        for (index, message) in group.messages.into_iter().enumerate() {
            let id = format!("lab-g{:08}-m{index:05}", corpus.cursor - 1);
            let entry = Entry::Message {
                id: id.clone(),
                parent_id: Some(parent),
                at_ms: (corpus.cursor as u64) * 100_000 + index as u64,
                message,
                tool: None,
                finish_reason: None,
                bash: None,
                agent: None,
                goal: None,
                reminder: None,
            };
            file.append(&entry, None).await?;
            parent = id;
        }
        tokens = tokens.saturating_add(group_tokens);
        if tokens >= spec.target_tokens {
            break;
        }
    }
    drop(file);
    table.unload(session_id).await?;
    Ok(())
}

async fn compact_round(
    spec: &Spec,
    client: &ClientHandle,
    sink: &EventSink,
    session_id: &str,
    paths: &SessionPaths,
    output: &Path,
) -> Result<Value> {
    let before = snapshot(client, session_id, paths, output, "pre").await?;
    progress(json!({"phase":"compacting", "output":output,
        "tokens_before":before.total_tokens, "target_tokens":spec.target_tokens}))?;
    let started = Instant::now();
    let compact = client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session_id.into(),
            // Some("") 也会跳过 shake，完整管线必须传 None。
            instructions: None,
        })
        .await;
    let elapsed_ms = started.elapsed().as_millis();
    // 失败同样保留可能已经完成 shake 的会话状态。
    let after = snapshot(client, session_id, paths, output, "post").await?;
    let entries = SessionStore::new(&spec.session_dir)
        .read_entries(paths)
        .await?;
    let checkpoints: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(entry, Entry::Compaction { .. } | Entry::Usage { .. }))
        .collect();
    save(output.join("checkpoints-and-usage.json"), &checkpoints)?;
    check_events(sink)?;
    let mut result = json!({
        "session_id":session_id, "target_tokens":spec.target_tokens,
        "tokens_before":before.total_tokens, "tokens_after":after.total_tokens,
        "overshoot_tokens":before.total_tokens.saturating_sub(spec.target_tokens),
        "elapsed_ms":elapsed_ms, "transcript":paths.transcript, "sidecar":paths.sidecar,
    });
    match compact {
        Ok(compact) => {
            result["status"] = json!("completed");
            result["compact"] = serde_json::to_value(compact)?;
        }
        Err(error) => {
            result["status"] = json!("failed");
            result["error"] = json!(error.to_string());
        }
    }
    Ok(result)
}

/// 仅作用于调用方准备的探针副本；不设置 parent/preset/prompt，因此不注入子代理系统提示。
async fn restrict_probe(spec: &Spec, session_id: &str) -> Result<()> {
    let table = SessionTable::new(&spec.session_dir, true);
    let record = table.get(session_id).await?;
    table
        .ensure_write_lease(session_id, &record, "compaction-lab-probe")
        .await?;
    let mut entries = record.state.lock().await.log_entries();
    let allow: &[&str] = match spec.probe_mode {
        ProbeMode::Direct => &[],
        ProbeMode::Retrieval => &["read", "grep", "find"],
    };
    let Some(Entry::Header {
        agent,
        parent_session_id,
        ..
    }) = entries.first_mut()
    else {
        bail!("探针副本缺少 header");
    };
    ensure!(parent_session_id.is_none(), "探针只接受顶层会话副本");
    save(
        spec.output.join("probe-policy.json"),
        &json!({
            "original_agent":agent, "allow":allow,
            "description":"仅更换工具白名单；沿用正常 SessionPrompt 的上下文构造"
        }),
    )?;
    *agent = Some(serde_json::from_value(
        json!({"tool_policy":{"allow":allow}}),
    )?);
    table.rewrite(session_id, entries).await?;
    table.unload(session_id).await?;
    Ok(())
}

async fn run(spec: &Spec, client: &ClientHandle, sink: &EventSink) -> Result<()> {
    let session_id = spec.session_id.as_deref().unwrap_or("ses_compaction_lab");
    if spec.operation != Operation::Run {
        let paths = SessionStore::new(&spec.session_dir)
            .find(session_id)
            .await?
            .context("找不到指定隔离会话")?;
        ensure!(!paths.compressed(), "实验操作需要明文 JSONL 会话快照");
        if spec.operation == Operation::Probe {
            restrict_probe(spec, session_id).await?;
        }
        load(client, session_id).await?;
        begin_events(sink, &spec.output)?;
        match spec.operation {
            Operation::Inspect => {
                snapshot(client, session_id, &paths, &spec.output, "inspect").await?;
            }
            Operation::Compact => {
                let result =
                    compact_round(spec, client, sink, session_id, &paths, &spec.output).await?;
                save(spec.output.join("result.json"), &result)?;
                ensure!(
                    result["status"] == "completed",
                    "压缩失败：{}",
                    result["error"]
                );
            }
            Operation::Probe => {
                let question = spec
                    .probe
                    .clone()
                    .context("probe 操作需要 probe 问题正文")?;
                snapshot(client, session_id, &paths, &spec.output, "pre").await?;
                let result = client
                    .call::<SessionPrompt>(PromptParams {
                        session_id: session_id.into(),
                        input: vec![InputPart::Text { text: question }],
                        mode: Some(PromptMode::Ask),
                        agent_message: None,
                        goal_round: None,
                    })
                    .await?;
                save(spec.output.join("reply.json"), &result)?;
                snapshot(client, session_id, &paths, &spec.output, "post").await?;
            }
            Operation::Run => unreachable!(),
        }
        unload(client, session_id).await?;
        check_events(sink)?;
        return Ok(());
    }
    ensure!(
        spec.rounds > 0 && spec.target_tokens > 0,
        "rounds 和 target_tokens 必须大于零"
    );
    let mut corpus = Corpus::open(spec.corpus.as_deref().context("run 操作需要 corpus 路径")?)?;
    let paths = seed(spec, session_id).await?;
    load(client, session_id).await?;
    let mut rounds = Vec::with_capacity(spec.rounds);
    for round in 1..=spec.rounds {
        let output = spec.output.join(format!("round-{round:02}"));
        fs::create_dir(&output)?;
        begin_events(sink, &output)?;
        let cursor_start = corpus.cursor;
        let mut sources = Vec::new();
        let outcome: Result<Value> = async {
            let mut measured = context(client, session_id).await?;
            // 每轮至少追加一组新的真实数据；最后由同一 core 的上下文 API 校准。
            loop {
                unload(client, session_id).await?;
                append_until(
                    spec,
                    session_id,
                    &mut corpus,
                    measured.total_tokens,
                    &mut sources,
                )
                .await?;
                load(client, session_id).await?;
                measured = context(client, session_id).await?;
                if measured.total_tokens >= spec.target_tokens {
                    break;
                }
            }
            compact_round(spec, client, sink, session_id, &paths, &output).await
        }
        .await;
        save(output.join("sources.json"), &sources)?;
        let mut result = match outcome {
            Ok(result) => result,
            Err(error) => json!({"status":"failed", "error":format!("{error:#}")}),
        };
        result["round"] = json!(round);
        result["cursor_start"] = json!(cursor_start);
        result["cursor_end"] = json!(corpus.cursor);
        save(output.join("result.json"), &result)?;
        progress(result.clone())?;
        let failed = result["status"] == "failed";
        rounds.push(result);
        save(
            spec.output.join("report.json"),
            &json!({
                "session_id":session_id, "model":spec.model, "target_tokens":spec.target_tokens,
                "requested_rounds":spec.rounds, "rounds":rounds, "transcript":paths.transcript,
                "sidecar":paths.sidecar, "status":if failed {"failed"} else if round == spec.rounds {"completed"} else {"running"}
            }),
        )?;
        ensure!(
            !failed,
            "第 {round} 轮失败，详见 result.json 与 updates.jsonl"
        );
    }
    unload(client, session_id).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("需要一个绝对 spec.json 路径")?;
    let spec: Spec = serde_json::from_reader(File::open(path)?)?;
    for path in [&spec.home, &spec.cwd, &spec.session_dir, &spec.output] {
        ensure!(
            path.is_absolute(),
            "实验路径必须是绝对路径：{}",
            path.display()
        );
    }
    if let Some(path) = &spec.corpus {
        ensure!(path.is_absolute(), "corpus 必须是绝对路径");
    }
    fs::create_dir_all(&spec.output)?;
    let core = Core::new(CoreConfig {
        home: spec.home.clone(),
        cwd: spec.cwd.clone(),
        overlays: vec![],
        session_dir: Some(spec.session_dir.clone()),
        client: moc_llm::Client::new(),
        ptc_worker: None,
    })?;
    let server = Server::new((*core).clone());
    let (server_side, client_side) = transport::duplex(65536);
    let connection = server.spawn(server_side);
    let sink = Arc::new(Mutex::new(Events::default()));
    let updates = sink.clone();
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "上下文压缩实验".into(),
        version: "1".into(),
        frontend: "compaction-lab".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: false,
        ask_requests: false,
    })
    .on_notification::<SessionUpdateMethod, _, _>(move |params, _| {
        event(&updates, "session", &params);
        std::future::ready(())
    })
    .connect(client_side)
    .await?;
    let outcome = run(&spec, &client, &sink).await;
    if let Err(error) = &outcome {
        save(
            spec.output.join("error.json"),
            &json!({"error":format!("{error:#}")}),
        )?;
    }
    server.shutdown();
    connection.wait().await;
    core.shutdown().await;
    outcome
}
