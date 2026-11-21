//! Image Playground 本地索引与图片文件持久化。
//!
//! 目录布局（以 `<storage_dir>` 为根）：
//! - `index.json`：全部会话、生成轮次、生成图与收藏参考图的索引（原子替换写入）。
//! - `images/YYYY-MM-DD/<id>.<ext>`：生成的图片文件。
//! - `favorites/<id>.<ext>`：用户直接上传到收藏夹的本地参考图。
//! - `uploads/<sha256>.<ext>`：Edit 请求附带的本地参考图去重存储。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use moc_llm::ImageGenParams;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::codec::EncodedImage;

const INDEX_FILE: &str = "index.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaygroundIndex {
    pub version: u32,
    pub sessions: Vec<PlaygroundSession>,
    pub images: BTreeMap<String, StoredImage>,
    /// 收藏列表（仅存图片 ID，按收藏先后倒序排列：最新收藏在前）。
    pub favorites: Vec<String>,
    /// 收藏文件夹（一级分组；未归入任何文件夹的项位于收藏根）。
    #[serde(default)]
    pub favorite_folders: Vec<FavoriteFolder>,
    /// 收藏图片到文件夹的映射（image_id -> folder_id；缺省表示在收藏根）。
    #[serde(default)]
    pub favorite_image_folders: BTreeMap<String, String>,
    /// 提示词 / 组合收藏（支持纯提示词，或多张参考图 + 提示词打包为一个收藏单位）。
    #[serde(default)]
    pub favorite_presets: Vec<FavoritePreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoriteFolder {
    pub id: String,
    pub name: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoritePreset {
    pub id: String,
    pub title: String,
    pub prompt: String,
    /// 关联的参考图 ID 列表（可为空表示纯提示词收藏，或包含多张参考图）。
    #[serde(default)]
    pub image_ids: Vec<String>,
    /// 所属收藏文件夹 ID；`None` 表示位于收藏根。
    pub folder_id: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaygroundSession {
    pub id: String,
    pub title: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub model: Option<String>,
    #[serde(default)]
    pub params: ImageGenParams,
    #[serde(default)]
    pub turns: Vec<PlaygroundTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaygroundSessionSummary {
    pub id: String,
    pub title: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub model: Option<String>,
    pub turn_count: usize,
    pub image_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaygroundTurn {
    pub id: String,
    pub created_at_ms: u64,
    pub model: String,
    pub mode: String,
    pub prompt: String,
    #[serde(default)]
    pub params: ImageGenParams,
    #[serde(default)]
    pub inputs: Vec<TurnInputRef>,
    #[serde(default)]
    pub outputs: Vec<String>,
    pub revised_prompt: Option<String>,
    pub duration_ms: u64,
    pub error: Option<TurnError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnInputRef {
    /// 若来自已有图片/收藏，填 `image_id`；若为本地临时上传文件，填 `upload_id`。
    pub image_id: Option<String>,
    pub upload_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnError {
    pub status: Option<u16>,
    pub message: String,
    pub raw_body: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageOrigin {
    Generated,
    UploadedFavorite,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredImage {
    pub id: String,
    pub origin: ImageOrigin,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub created_at_ms: u64,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub revised_prompt: Option<String>,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub extension: String,
    pub mime_type: String,
    pub is_animated: bool,
    pub size_bytes: u64,
    pub original_size_bytes: u64,
    pub original_format: String,
    /// 文件在本地磁盘的绝对路径（允许后续变更 storage_dir 后仍可访问旧图片）。
    pub file_path: String,
    pub filename: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub fn load_index(storage_dir: &Path) -> anyhow::Result<PlaygroundIndex> {
    let path = storage_dir.join(INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(PlaygroundIndex {
            version: 1,
            ..Default::default()
        }),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("解析 {} 失败", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PlaygroundIndex {
            version: 1,
            ..Default::default()
        }),
        Err(error) => Err(error.into()),
    }
}

pub fn save_index(storage_dir: &Path, index: &PlaygroundIndex) -> anyhow::Result<()> {
    std::fs::create_dir_all(storage_dir)?;
    let path = storage_dir.join(INDEX_FILE);
    let data = serde_json::to_vec_pretty(index)?;
    let tmp = tempfile::NamedTempFile::new_in(storage_dir)?;
    std::fs::write(tmp.path(), data)?;
    tmp.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

/// 将生成图落盘到 `<storage_dir>/images/YYYY-MM-DD/<id>.<ext>`。
pub fn write_generated_image(
    storage_dir: &Path,
    id: &str,
    encoded: &EncodedImage,
) -> anyhow::Result<PathBuf> {
    let date_dir = today_date_dir();
    let dir = storage_dir.join("images").join(date_dir);
    std::fs::create_dir_all(&dir)?;
    let file_path = dir.join(format!("{id}.{}", encoded.extension));
    let tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::fs::write(tmp.path(), &encoded.bytes)?;
    tmp.persist(&file_path).map_err(|error| error.error)?;
    Ok(file_path)
}

/// 将用户直接上传到收藏夹的本地参考图落盘到 `<storage_dir>/favorites/<id>.<ext>`。
pub fn write_favorite_upload(
    storage_dir: &Path,
    id: &str,
    extension: &str,
    bytes: &[u8],
) -> anyhow::Result<PathBuf> {
    let dir = storage_dir.join("favorites");
    std::fs::create_dir_all(&dir)?;
    let file_path = dir.join(format!("{id}.{extension}"));
    let tmp = tempfile::NamedTempFile::new_in(&dir)?;
    std::fs::write(tmp.path(), bytes)?;
    tmp.persist(&file_path).map_err(|error| error.error)?;
    Ok(file_path)
}

/// 将 Edit 本地上传的参考图按内容哈希去重保存到 `<storage_dir>/uploads/<hash>.<ext>`。
pub fn write_edit_upload(
    storage_dir: &Path,
    extension: &str,
    bytes: &[u8],
) -> anyhow::Result<(String, PathBuf)> {
    let dir = storage_dir.join("uploads");
    std::fs::create_dir_all(&dir)?;
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let upload_id = format!("{hex}.{extension}");
    let file_path = dir.join(&upload_id);
    if !file_path.exists() {
        let tmp = tempfile::NamedTempFile::new_in(&dir)?;
        std::fs::write(tmp.path(), bytes)?;
        let _ = tmp.persist_noclobber(&file_path);
    }
    Ok((upload_id, file_path))
}

/// 删除单个本地文件：优先移入系统回收站，无回收站环境降级直接删除。
pub fn trash_or_remove(path: &Path) {
    if !path.exists() {
        return;
    }
    if let Err(error) = trash::delete(path) {
        tracing::warn!(path = %path.display(), %error, "移入回收站失败，降级为直接删除");
        let _ = std::fs::remove_file(path);
    }
}

impl PlaygroundIndex {
    pub fn summaries(&self) -> Vec<PlaygroundSessionSummary> {
        let mut list: Vec<_> = self
            .sessions
            .iter()
            .map(|s| PlaygroundSessionSummary {
                id: s.id.clone(),
                title: s.title.clone(),
                created_at_ms: s.created_at_ms,
                updated_at_ms: s.updated_at_ms,
                model: s.model.clone(),
                turn_count: s.turns.len(),
                image_count: s.turns.iter().map(|t| t.outputs.len()).sum(),
            })
            .collect();
        list.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        list
    }

    pub fn is_favorite(&self, image_id: &str) -> bool {
        self.favorites.iter().any(|id| id == image_id)
    }

    pub fn favorite_folder_of(&self, image_id: &str) -> Option<String> {
        let folder_id = self.favorite_image_folders.get(image_id)?;
        if self.has_favorite_folder(folder_id) {
            Some(folder_id.clone())
        } else {
            None
        }
    }

    pub fn has_favorite_folder(&self, folder_id: &str) -> bool {
        self.favorite_folders.iter().any(|f| f.id == folder_id)
    }

    /// 某张图片是否受收藏保护（在图片收藏列表，或被任一提示词/组合收藏引用）。
    pub fn is_protected_by_favorite(&self, image_id: &str) -> bool {
        self.is_favorite(image_id)
            || self
                .favorite_presets
                .iter()
                .any(|p| p.image_ids.iter().any(|id| id == image_id))
    }

    pub fn set_favorite(&mut self, image_id: &str, favorite: bool) -> bool {
        if !self.images.contains_key(image_id) {
            return false;
        }
        self.favorites.retain(|id| id != image_id);
        if favorite {
            self.favorites.insert(0, image_id.to_owned());
        } else {
            self.favorite_image_folders.remove(image_id);
        }
        true
    }

    /// 设置已收藏图片的所属文件夹；`None` 表示移回收藏根。
    pub fn set_favorite_image_folder(
        &mut self,
        image_id: &str,
        folder_id: Option<String>,
    ) -> anyhow::Result<()> {
        if !self.images.contains_key(image_id) {
            anyhow::bail!("图片不存在");
        }
        if let Some(ref fid) = folder_id
            && !self.has_favorite_folder(fid)
        {
            anyhow::bail!("目标收藏文件夹不存在");
        }
        if !self.is_favorite(image_id) {
            self.favorites.insert(0, image_id.to_owned());
        }
        match folder_id {
            Some(fid) => {
                self.favorite_image_folders
                    .insert(image_id.to_owned(), fid);
            }
            None => {
                self.favorite_image_folders.remove(image_id);
            }
        }
        Ok(())
    }

    /// 删除收藏文件夹：不删除其中的图片和组合预设，全部退回收藏根。
    pub fn remove_favorite_folder(&mut self, folder_id: &str) -> bool {
        let before = self.favorite_folders.len();
        self.favorite_folders.retain(|f| f.id != folder_id);
        if self.favorite_folders.len() == before {
            return false;
        }
        self.favorite_image_folders.retain(|_, fid| fid != folder_id);
        for preset in &mut self.favorite_presets {
            if preset.folder_id.as_deref() == Some(folder_id) {
                preset.folder_id = None;
            }
        }
        true
    }

    /// 删除单张图片：同时清理索引、收藏列表、组合预设引用及所属会话轮次的输出引用，返回待清理的磁盘路径。
    pub fn remove_image(&mut self, image_id: &str) -> Option<PathBuf> {
        let removed = self.images.remove(image_id)?;
        self.favorites.retain(|id| id != image_id);
        self.favorite_image_folders.remove(image_id);
        for preset in &mut self.favorite_presets {
            preset.image_ids.retain(|id| id != image_id);
        }
        for session in &mut self.sessions {
            for turn in &mut session.turns {
                turn.outputs.retain(|id| id != image_id);
            }
        }
        Some(PathBuf::from(removed.file_path))
    }

    /// 清理不再被单图收藏、组合收藏或任何会话输入引用的 `UploadedFavorite` 晋升图片，返回待清理的磁盘路径。
    pub fn prune_orphan_preset_uploads(&mut self, candidate_ids: &[String]) -> Vec<PathBuf> {
        let mut to_trash = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in candidate_ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            let is_orphan_upload = self
                .images
                .get(id)
                .is_some_and(|img| img.origin == ImageOrigin::UploadedFavorite)
                && !self.is_protected_by_favorite(id)
                && !self.sessions.iter().any(|s| {
                    s.turns.iter().any(|t| {
                        t.inputs
                            .iter()
                            .any(|inp| inp.image_id.as_deref() == Some(id.as_str()))
                    })
                });
            if is_orphan_upload && let Some(removed) = self.images.remove(id) {
                self.favorite_image_folders.remove(id);
                to_trash.push(PathBuf::from(removed.file_path));
            }
        }
        to_trash
    }

    /// 删除会话：保留所有已收藏图片或被收藏组合引用的图片（将其 `session_id` 置空）；可选删除未收藏的会话生成图。
    pub fn remove_session(&mut self, session_id: &str, delete_unfavorited: bool) -> Vec<PathBuf> {
        let candidate_input_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| s.id == session_id)
            .flat_map(|s| s.turns.iter())
            .flat_map(|t| t.inputs.iter())
            .filter_map(|inp| inp.image_id.clone())
            .collect();
        self.sessions.retain(|s| s.id != session_id);
        let mut to_trash = Vec::new();
        let mut to_remove_ids = Vec::new();
        let protected_ids: std::collections::HashSet<String> = self
            .images
            .keys()
            .filter(|id| self.is_protected_by_favorite(id))
            .cloned()
            .collect();
        for (id, img) in &mut self.images {
            if img.session_id.as_deref() == Some(session_id) {
                if protected_ids.contains(id) || !delete_unfavorited {
                    img.session_id = None;
                } else {
                    to_remove_ids.push(id.clone());
                }
            }
        }
        for id in to_remove_ids {
            if let Some(img) = self.images.remove(&id) {
                to_trash.push(PathBuf::from(img.file_path));
            }
        }
        if delete_unfavorited {
            to_trash.extend(self.prune_orphan_preset_uploads(&candidate_input_ids));
        }
        to_trash
    }
}

/// 简易 UTC YYYY-MM-DD 日期目录（不引入 chrono 依赖）。
fn today_date_dir() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant 算法：自 1970-01-01 起的整数天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_image(id: &str, session_id: Option<&str>, path: &Path) -> StoredImage {
        StoredImage {
            id: id.into(),
            origin: ImageOrigin::Generated,
            session_id: session_id.map(str::to_owned),
            turn_id: Some("t1".into()),
            created_at_ms: 1,
            model: Some("p/m".into()),
            prompt: Some("cat".into()),
            revised_prompt: None,
            width: 64,
            height: 64,
            format: "png".into(),
            extension: "png".into(),
            mime_type: "image/png".into(),
            is_animated: false,
            size_bytes: 10,
            original_size_bytes: 10,
            original_format: "png".into(),
            file_path: path.to_string_lossy().into_owned(),
            filename: format!("{id}.png"),
        }
    }

    #[test]
    fn delete_session_keeps_favorited_images_and_delete_image_clears_favorite() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("img1.png");
        let f2 = dir.path().join("img2.png");
        let f3 = dir.path().join("img3.png");
        std::fs::write(&f1, b"1").unwrap();
        std::fs::write(&f2, b"2").unwrap();
        std::fs::write(&f3, b"3").unwrap();

        let mut index = PlaygroundIndex {
            version: 1,
            sessions: vec![PlaygroundSession {
                id: "s1".into(),
                title: "test".into(),
                created_at_ms: 1,
                updated_at_ms: 1,
                model: None,
                params: ImageGenParams::default(),
                turns: vec![],
            }],
            images: BTreeMap::from([
                ("img1".into(), dummy_image("img1", Some("s1"), &f1)),
                ("img2".into(), dummy_image("img2", Some("s1"), &f2)),
                ("img3".into(), dummy_image("img3", Some("s1"), &f3)),
            ]),
            favorites: vec!["img1".into()],
            favorite_folders: vec![FavoriteFolder {
                id: "fld1".into(),
                name: "大肥鱼".into(),
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            favorite_image_folders: BTreeMap::from([("img1".into(), "fld1".into())]),
            favorite_presets: vec![FavoritePreset {
                id: "pre1".into(),
                title: "组合".into(),
                prompt: "a fat fish".into(),
                image_ids: vec!["img3".into()],
                folder_id: Some("fld1".into()),
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
        };

        // 删除会话：img1（收藏）与 img3（被组合预设引用）均保留，仅 img2 删除
        let trashed = index.remove_session("s1", true);
        assert_eq!(trashed, vec![f2]);
        assert!(index.images.contains_key("img1"), "已收藏图片应保留");
        assert!(index.images.contains_key("img3"), "被组合收藏引用的图片应保留");
        assert_eq!(index.images["img1"].session_id, None);
        assert_eq!(index.favorite_folder_of("img1").as_deref(), Some("fld1"));

        // 删除文件夹：内部图片和组合应退回收藏根
        assert!(index.remove_favorite_folder("fld1"));
        assert_eq!(index.favorite_folder_of("img1"), None);
        assert_eq!(index.favorite_presets[0].folder_id, None);

        // 删除单张图片：同时清理收藏与组合引用
        let removed_path = index.remove_image("img1");
        assert_eq!(removed_path, Some(f1));
        assert!(!index.is_favorite("img1"));
        index.remove_image("img3");
        assert!(index.favorite_presets[0].image_ids.is_empty());

        // 仅被组合引用的 UploadedFavorite 晋升图在无任何引用时应被清理；仍被收藏的保留
        let f4 = dir.path().join("img4.png");
        let f5 = dir.path().join("img5.png");
        std::fs::write(&f4, b"4").unwrap();
        std::fs::write(&f5, b"5").unwrap();
        let mut up4 = dummy_image("img4", None, &f4);
        up4.origin = ImageOrigin::UploadedFavorite;
        let mut up5 = dummy_image("img5", None, &f5);
        up5.origin = ImageOrigin::UploadedFavorite;
        index.images.insert("img4".into(), up4);
        index.images.insert("img5".into(), up5);
        index.favorites.push("img5".into());
        let pruned = index.prune_orphan_preset_uploads(&["img4".into(), "img5".into()]);
        assert_eq!(pruned, vec![f4]);
        assert!(!index.images.contains_key("img4"));
        assert!(index.images.contains_key("img5"));
    }
}
