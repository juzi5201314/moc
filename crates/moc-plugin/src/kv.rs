//! 插件键值存储：JSON 值，每次写入原子落盘。
//!
//! 存储按插件名共享（不区分项目），路径为 `<home>/plugin-data/<name>/kv.json`。
//! 写入走临时文件 + rename，进程崩溃不会留下半个 JSON。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{PluginError, Result};

/// 单个 key 的最大字节数。
const MAX_KEY_BYTES: usize = 128;

pub struct Kv {
    file: PathBuf,
    max_bytes: usize,
    inner: parking_lot::Mutex<Inner>,
}

struct Inner {
    values: BTreeMap<String, Value>,
    bytes: usize,
}

impl Kv {
    /// 打开（或创建）某个插件的存储目录；文件损坏时按空存储继续并告警。
    pub fn open(root: &Path, name: &str, max_bytes: usize) -> Kv {
        let dir = root.join(name);
        let file = dir.join("kv.json");
        let mut values = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&file) {
            match serde_json::from_str::<BTreeMap<String, Value>>(&text) {
                Ok(parsed) => values = parsed,
                Err(error) => {
                    tracing::warn!("插件 `{name}` 的 kv.json 无法解析，按空存储继续：{error}");
                }
            }
        }
        let bytes = values
            .iter()
            .map(|(key, value)| value_bytes(key, value))
            .sum();
        Kv {
            file,
            max_bytes,
            inner: parking_lot::Mutex::new(Inner { values, bytes }),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>> {
        check_key(key)?;
        Ok(self.inner.lock().values.get(key).cloned())
    }

    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        check_key(key)?;
        let mut inner = self.inner.lock();
        let previous = inner.values.get(key).map_or(0, |old| value_bytes(key, old));
        let total = inner.bytes - previous + value_bytes(key, &value);
        if total > self.max_bytes {
            return Err(PluginError::new(
                "E_PLUGIN_KV_LIMIT",
                format!("kv 超过插件配额（{total} > {} 字节）", self.max_bytes),
            ));
        }
        inner.values.insert(key.to_owned(), value);
        inner.bytes = total;
        self.persist(&inner)
    }

    pub fn delete(&self, key: &str) -> Result<bool> {
        check_key(key)?;
        let mut inner = self.inner.lock();
        let Some(old) = inner.values.remove(key) else {
            return Ok(false);
        };
        inner.bytes -= value_bytes(key, &old);
        self.persist(&inner)?;
        Ok(true)
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().values.keys().cloned().collect()
    }

    pub fn clear(&self) -> Result<()> {
        let mut inner = self.inner.lock();
        inner.values.clear();
        inner.bytes = 0;
        self.persist(&inner)
    }

    fn persist(&self, inner: &Inner) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PluginError::new("E_PLUGIN_KV_IO", format!("{}: {error}", parent.display()))
            })?;
        }
        let text = serde_json::to_string(&inner.values)
            .map_err(|error| PluginError::new("E_PLUGIN_KV_IO", error.to_string()))?;
        let temp = self.file.with_extension("json.tmp");
        std::fs::write(&temp, text)
            .and_then(|()| std::fs::rename(&temp, &self.file))
            .map_err(|error| {
                PluginError::new(
                    "E_PLUGIN_KV_IO",
                    format!("{}: {error}", self.file.display()),
                )
            })
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(PluginError::new(
            "E_PLUGIN_KV_KEY",
            format!("key 必须是 1..{MAX_KEY_BYTES} 个 UTF-8 字节"),
        ));
    }
    Ok(())
}

fn value_bytes(key: &str, value: &Value) -> usize {
    key.len() + value.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_and_reloads_from_disk() {
        let temp = tempfile::tempdir().unwrap();
        let kv = Kv::open(temp.path(), "demo", 4096);
        kv.set("a", json!({"n": 1})).unwrap();
        kv.set("b", json!([1, 2])).unwrap();
        assert_eq!(kv.get("a").unwrap(), Some(json!({"n": 1})));
        assert_eq!(kv.keys(), vec!["a", "b"]);

        let reopened = Kv::open(temp.path(), "demo", 4096);
        assert_eq!(reopened.get("b").unwrap(), Some(json!([1, 2])));
        assert!(reopened.delete("a").unwrap());
        assert!(!reopened.delete("a").unwrap());
    }

    #[test]
    fn rejects_oversized_values_and_bad_keys() {
        let temp = tempfile::tempdir().unwrap();
        let kv = Kv::open(temp.path(), "demo", 64);
        let error = kv.set("big", json!("x".repeat(200))).expect_err("超过配额");
        assert_eq!(error.code, "E_PLUGIN_KV_LIMIT");
        assert_eq!(kv.get("").unwrap_err().code, "E_PLUGIN_KV_KEY");
        assert!(kv.keys().is_empty());
    }

    #[test]
    fn corrupt_file_falls_back_to_empty_storage() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kv.json"), "{not json").unwrap();
        let kv = Kv::open(temp.path(), "demo", 4096);
        assert!(kv.keys().is_empty());
        kv.set("k", json!(1)).unwrap();
        assert_eq!(
            Kv::open(temp.path(), "demo", 4096).get("k").unwrap(),
            Some(json!(1))
        );
    }
}
