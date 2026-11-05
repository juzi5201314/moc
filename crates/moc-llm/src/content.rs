//! 消息正文的共享存储：活动内容写时复制，已验证的冻结 JSON 可延迟还原。
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::ContentPart;

/// 对外仍是同样的内容数组；归档字节不可变且在构造时完整校验。
#[derive(Clone)]
pub struct MessageContent(Arc<Storage>);

#[derive(Clone)]
enum Storage {
    Resident(Vec<ContentPart>),
    // 冷分支装箱，保持热分支接近 Vec 的大小。
    Archived(Box<Archive>),
}

struct Archive {
    bytes: Bytes,
    // 冷消息通常不解析；把 Vec 缓存移到首次解析时分配。
    parsed: OnceLock<Box<Vec<ContentPart>>>,
    // 两类 ID 同生同死且归档后不追加：一个紧凑切片，calls_len 为分界。
    ids: Box<[String]>,
    calls_len: usize,
}

impl Clone for Archive {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            parsed: OnceLock::new(),
            ids: self.ids.clone(),
            calls_len: self.calls_len,
        }
    }
}

impl Archive {
    fn parse(&self) -> Vec<ContentPart> {
        // 不执行文件 I/O：Bytes 的不可变性保证构造时验证过的 JSON 不会改变。
        serde_json::from_slice(&self.bytes).expect("冻结正文在构造时已通过类型校验")
    }
}

impl Storage {
    fn parts(&self) -> &Vec<ContentPart> {
        match self {
            Self::Resident(parts) => parts,
            Self::Archived(archive) => archive.parsed.get_or_init(|| Box::new(archive.parse())),
        }
    }

    fn into_parts(self) -> Vec<ContentPart> {
        match self {
            Self::Resident(parts) => parts,
            Self::Archived(mut archive) => archive
                .parsed
                .take()
                .map(|parts| *parts)
                .unwrap_or_else(|| archive.parse()),
        }
    }
}

impl MessageContent {
    /// 字节可由文件映射持有；校验失败由调用方处理，绝不发布坏的惰性正文。
    pub fn from_archive(bytes: Bytes) -> Result<Self, serde_json::Error> {
        let parts: Vec<ContentPart> = serde_json::from_slice(&bytes)?;
        let mut call_ids = Vec::new();
        let mut result_ids = Vec::new();
        for part in &parts {
            match part {
                ContentPart::ToolCall(call) => call_ids.push(call.id.clone()),
                ContentPart::ToolResult(result) => result_ids.push(result.tool_call_id.clone()),
                _ => {}
            }
        }
        let calls_len = call_ids.len();
        call_ids.extend(result_ids);
        debug_assert!(calls_len <= call_ids.len());
        Ok(Self(Arc::new(Storage::Archived(Box::new(Archive {
            bytes,
            parsed: OnceLock::new(),
            ids: call_ids.into_boxed_slice(),
            calls_len,
        })))))
    }

    /// 只需要派生值的读取使用此入口，归档解码在闭包结束后释放。
    pub fn with_parts<R>(&self, read: impl FnOnce(&[ContentPart]) -> R) -> R {
        match self.0.as_ref() {
            Storage::Resident(parts) => read(parts),
            Storage::Archived(archive) => match archive.parsed.get() {
                Some(parts) => read(parts),
                None => read(&archive.parse()),
            },
        }
    }

    pub fn is_archived(&self) -> bool {
        matches!(self.0.as_ref(), Storage::Archived(_))
    }

    /// 同一份内容的进程内身份；只用于持有内容引用期间的去重，不能持久化。
    pub fn storage_id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    pub fn archive_cache_loaded(&self) -> bool {
        matches!(self.0.as_ref(), Storage::Archived(archive) if archive.parsed.get().is_some())
    }

    /// 不影响任何已有读取者；新句柄保留相同冻结字节，但不继续持有已解码的副本。
    pub fn cold_clone(&self) -> Self {
        match self.0.as_ref() {
            Storage::Archived(archive) if archive.parsed.get().is_some() => {
                Self(Arc::new(Storage::Archived(archive.clone())))
            }
            _ => self.clone(),
        }
    }

    /// 临时编码持有独立解码槽；即使原归档尚冷，也不让临时解析加热所有者。
    pub(crate) fn detached_archive(&self) -> Self {
        match self.0.as_ref() {
            Storage::Archived(archive) => Self(Arc::new(Storage::Archived(archive.clone()))),
            _ => self.clone(),
        }
    }

    /// 归档的索引权重，不把可由操作系统回收的映射页算成堆中的正文副本。
    pub fn archive_index_bytes(&self) -> Option<usize> {
        match self.0.as_ref() {
            Storage::Resident(_) => None,
            Storage::Archived(archive) => Some(
                std::mem::size_of::<Archive>()
                    + std::mem::size_of::<Storage>()
                    + 2 * std::mem::size_of::<usize>()
                    + archive
                        .ids
                        .iter()
                        .map(|id| id.len() + std::mem::size_of::<String>())
                        .sum::<usize>(),
            ),
        }
    }

    fn resident_parts(&self) -> Option<&[ContentPart]> {
        match self.0.as_ref() {
            Storage::Resident(parts) => Some(parts),
            Storage::Archived(_) => None,
        }
    }

    /// 修补未完成调用只需要 ID，不能为此解码已归档的工具正文。
    pub fn tool_call_ids(&self) -> impl Iterator<Item = &str> {
        let archived = match self.0.as_ref() {
            Storage::Archived(archive) => Some(&archive.ids[..archive.calls_len]),
            Storage::Resident(_) => None,
        };
        archived.into_iter().flatten().map(String::as_str).chain(
            self.resident_parts()
                .into_iter()
                .flatten()
                .filter_map(|part| match part {
                    ContentPart::ToolCall(call) => Some(call.id.as_str()),
                    _ => None,
                }),
        )
    }

    pub fn tool_result_ids(&self) -> impl Iterator<Item = &str> {
        let archived = match self.0.as_ref() {
            Storage::Archived(archive) => Some(&archive.ids[archive.calls_len..]),
            Storage::Resident(_) => None,
        };
        archived.into_iter().flatten().map(String::as_str).chain(
            self.resident_parts()
                .into_iter()
                .flatten()
                .filter_map(|part| match part {
                    ContentPart::ToolResult(result) => Some(result.tool_call_id.as_str()),
                    _ => None,
                }),
        )
    }
}

impl Default for MessageContent {
    fn default() -> Self {
        Vec::new().into()
    }
}
impl std::fmt::Debug for MessageContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_parts(|parts| f.debug_tuple("MessageContent").field(&parts).finish())
    }
}
impl PartialEq for MessageContent {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || self.with_parts(|left| other.with_parts(|right| left == right))
    }
}
impl Serialize for MessageContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 临时解码不进入缓存；已有借用缓存则直接复用，不再分配第二份正文。
        self.with_parts(|parts| parts.serialize(serializer))
    }
}
impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<ContentPart>::deserialize(deserializer).map(Into::into)
    }
}
impl std::ops::Deref for MessageContent {
    type Target = Vec<ContentPart>;
    fn deref(&self) -> &Self::Target {
        self.0.parts()
    }
}
impl std::ops::DerefMut for MessageContent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.is_archived() {
            let old = std::mem::take(self);
            *self = Arc::unwrap_or_clone(old.0).into_parts().into();
        }
        let Storage::Resident(parts) = Arc::make_mut(&mut self.0) else {
            unreachable!("可写内容已转为驻留表示")
        };
        parts
    }
}
impl From<Vec<ContentPart>> for MessageContent {
    fn from(parts: Vec<ContentPart>) -> Self {
        Self(Arc::new(Storage::Resident(parts)))
    }
}
impl IntoIterator for MessageContent {
    type Item = ContentPart;
    type IntoIter = std::vec::IntoIter<ContentPart>;
    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.0).into_parts().into_iter()
    }
}
impl<'a> IntoIterator for &'a MessageContent {
    type Item = &'a ContentPart;
    type IntoIter = std::slice::Iter<'a, ContentPart>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a> IntoIterator for &'a mut MessageContent {
    type Item = &'a mut ContentPart;
    type IntoIter = std::slice::IterMut<'a, ContentPart>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
impl FromIterator<ContentPart> for MessageContent {
    fn from_iter<T: IntoIterator<Item = ContentPart>>(iter: T) -> Self {
        Vec::from_iter(iter).into()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_ids_keep_each_kind_in_order_without_parsing() {
        let json = serde_json::json!([
            {"ToolResult": {"tool_call_id": "r1", "content": "result", "is_error": false}},
            {"ToolCall": {"id": "c1", "name": "read", "arguments": "{}"}},
            {"Text": "text"},
            {"ToolCall": {"id": "c2", "name": "read", "arguments": "{}"}},
            {"ToolResult": {"tool_call_id": "r2", "content": "result", "is_error": false}}
        ]);
        let mut content =
            MessageContent::from_archive(serde_json::to_vec(&json).unwrap().into()).unwrap();
        assert_eq!(content.tool_call_ids().collect::<Vec<_>>(), ["c1", "c2"]);
        assert_eq!(content.tool_result_ids().collect::<Vec<_>>(), ["r1", "r2"]);
        assert!(!content.archive_cache_loaded());
        assert_eq!(serde_json::to_value(&content).unwrap(), json);
        let cold = content.clone();
        assert_eq!(content.len(), 5);
        assert!(content.archive_cache_loaded());
        content.push(ContentPart::Text("new".into()));
        assert_eq!(cold.len(), 5);
        assert_eq!(content.len(), 6);
        assert_eq!(serde_json::to_value(&cold).unwrap(), json);
    }
}

impl PartialEq<Vec<ContentPart>> for MessageContent {
    fn eq(&self, other: &Vec<ContentPart>) -> bool {
        self.with_parts(|parts| parts == other.as_slice())
    }
}
