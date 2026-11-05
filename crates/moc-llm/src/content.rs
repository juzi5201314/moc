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
    // 冷分支含字节句柄、解码槽和两个 ID 列表，装箱保持热分支接近 Vec 的大小。
    Archived(Box<Archive>),
}

struct Archive {
    bytes: Bytes,
    parsed: OnceLock<Vec<ContentPart>>,
    call_ids: Vec<String>,
    result_ids: Vec<String>,
}

impl Clone for Archive {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
            parsed: OnceLock::new(),
            call_ids: self.call_ids.clone(),
            result_ids: self.result_ids.clone(),
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
            Self::Archived(archive) => archive.parsed.get_or_init(|| archive.parse()),
        }
    }

    fn into_parts(self) -> Vec<ContentPart> {
        match self {
            Self::Resident(parts) => parts,
            Self::Archived(mut archive) => archive.parsed.take().unwrap_or_else(|| archive.parse()),
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
        Ok(Self(Arc::new(Storage::Archived(Box::new(Archive {
            bytes,
            parsed: OnceLock::new(),
            call_ids,
            result_ids,
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

    /// 归档的索引权重，不把可由操作系统回收的映射页算成堆中的正文副本。
    pub fn archive_index_bytes(&self) -> Option<usize> {
        match self.0.as_ref() {
            Storage::Resident(_) => None,
            Storage::Archived(archive) => Some(
                128 + archive
                    .call_ids
                    .iter()
                    .chain(&archive.result_ids)
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
            Storage::Archived(archive) => Some(archive.call_ids.as_slice()),
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
            Storage::Archived(archive) => Some(archive.result_ids.as_slice()),
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
impl PartialEq<Vec<ContentPart>> for MessageContent {
    fn eq(&self, other: &Vec<ContentPart>) -> bool {
        self.with_parts(|parts| parts == other.as_slice())
    }
}
