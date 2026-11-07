//! Workflow journal：append-only `journal.jsonl` 回放与重放。
//!
//! 记录身份是 (site_id, ordinal)；`input_hash` 是输入的规范化 JSON 的 xxh3；
//! `settle` 是落地序号，重放严格按 settle 顺序。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// journal 记录种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// actor 子会话建立（extra: actor_name/persona_key/child_session_id；resume/amend 恢复上下文）。
    Actor,
    /// 已完成 ask（含 actor 身份与输入哈希；amend 复用的匹配依据）。
    Ask,
    /// world.read / world.run 的已完成记录。
    World,
    /// artifact.file/markdown 发布记录。
    Artifact,
    /// hole fill 记录（填充体哈希 + 结果）。
    Hole,
    /// run 终态记录（先到者胜只写一次）。
    Run,
}

/// 一条 journal 记录。`extra` 存放种类相关字段（ask 的 actor 身份/指令等）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRecord {
    pub kind: RecordKind,
    pub site_id: String,
    pub ordinal: u32,
    /// xxh3-64 hex；run 记录为空串。
    #[serde(default)]
    pub input_hash: String,
    /// 落地序号（本 run 内单调增）。
    #[serde(default)]
    pub settle: u32,
    /// 结果载荷：`{"status":"completed","result":…,"usage":{…}}` 或 `{"status":"failed","code","message",…}`。
    #[serde(default)]
    pub outcome: Value,
    /// 种类相关附加字段（ask：actor_site_id/actor_ordinal/actor_seq/actor_name/persona_key/instructions）。
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

impl JournalRecord {
    /// (site_id, ordinal) 身份键。
    pub fn key(&self) -> (String, u32) {
        (self.site_id.clone(), self.ordinal)
    }
}

/// journal 存储边界：纯核心可单测；文件实现提供追加 + fsync。
pub trait JournalStore: Send {
    /// 追加一条记录（实现负责 fsync）。
    fn append(&mut self, record: &JournalRecord) -> std::io::Result<()>;
    /// 已加载的历史记录（重放源），按 settle 序。
    fn prior(&self) -> &[JournalRecord];
    /// run 目录（写入 run.json 等用）。
    fn run_dir(&self) -> Option<&Path> {
        None
    }
}

/// 规范化 JSON：对象键排序、数组保序，供输入哈希与 journal 行写入。
pub fn canonical_json(value: &Value) -> String {
    fn write(out: &mut String, v: &Value) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Value::Array(a) => {
                out.push('[');
                for (i, e) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(out, e);
                }
                out.push(']');
            }
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push(':');
                    write(out, &o[*k]);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(&mut out, value);
    out
}

/// 输入哈希：xxh3-64（hex）。
pub fn input_hash(value: &Value) -> String {
    let canonical = canonical_json(value);
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(canonical.as_bytes()))
}

/// 文件 journal：run_dir/journal.jsonl 追加并每条 fsync。
pub struct FileJournal {
    file: File,
    prior: Vec<JournalRecord>,
    dir: PathBuf,
}

impl FileJournal {
    /// 打开（创建）run_dir/journal.jsonl；读取已有记录作重放源。
    pub fn open(run_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let path = run_dir.join("journal.jsonl");
        let prior = if path.exists() {
            let reader = BufReader::new(File::open(&path)?);
            let mut records = Vec::new();
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                records.push(serde_json::from_str::<JournalRecord>(&line)?);
            }
            records
        } else {
            Vec::new()
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            prior,
            dir: run_dir.to_path_buf(),
        })
    }
}

impl JournalStore for FileJournal {
    fn append(&mut self, record: &JournalRecord) -> std::io::Result<()> {
        let line = canonical_json(&serde_json::to_value(record).map_err(std::io::Error::other)?);
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.sync_data()
    }
    fn prior(&self) -> &[JournalRecord] {
        &self.prior
    }
    fn run_dir(&self) -> Option<&Path> {
        Some(&self.dir)
    }
}

/// 内存 journal（测试用）。
#[derive(Default)]
pub struct MemoryJournal {
    pub written: Vec<JournalRecord>,
    pub prior: Vec<JournalRecord>,
}

impl JournalStore for MemoryJournal {
    fn append(&mut self, record: &JournalRecord) -> std::io::Result<()> {
        self.written.push(record.clone());
        Ok(())
    }
    fn prior(&self) -> &[JournalRecord] {
        &self.prior
    }
}

/// settle 序消费队列：按落地顺序回放；同 (site_id, ordinal) 先落地者占位置、
/// 后落地者仅替换内容（spec §4）。
pub struct SettleQueue {
    order: Vec<(String, u32)>,
    records: HashMap<(String, u32), JournalRecord>,
    cursor: usize,
}

impl SettleQueue {
    pub fn new(records: &[JournalRecord]) -> Self {
        let mut queue = Self {
            order: Vec::new(),
            records: HashMap::new(),
            cursor: 0,
        };
        for r in records {
            queue.apply(r.clone());
        }
        queue
    }

    /// 追入一条记录：位置按先落地者，内容取最新。
    pub fn apply(&mut self, record: JournalRecord) {
        let key = record.key();
        if !self.records.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.records.insert(key, record);
    }

    /// 下一条记录（按落地序）。
    pub fn next_record(&mut self) -> Option<&JournalRecord> {
        let key = self.order.get(self.cursor)?.clone();
        self.cursor += 1;
        self.records.get(&key)
    }

    /// 是否还有未消费记录。
    pub fn has_pending(&self) -> bool {
        self.cursor < self.order.len()
    }

    /// 按键查找（不必消费）。
    pub fn get(&self, site_id: &str, ordinal: u32) -> Option<&JournalRecord> {
        self.records.get(&(site_id.to_string(), ordinal))
    }
}
