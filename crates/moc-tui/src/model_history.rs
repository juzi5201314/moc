//! TUI 最近使用的模型；只记录成功生效/恢复的会话模型，不记录悬停。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 仅保留最近五个模型，按最近使用顺序线性查找。
#[derive(Default)]
pub struct RecentModels {
    ids: Vec<String>,
    path: Option<PathBuf>,
}

impl RecentModels {
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let path = home.join("tui-model-history.json");
        let ids = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Vec<String>>(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let mut history = Self {
            ids: Vec::new(),
            path: Some(path),
        };
        for id in ids.into_iter().rev() {
            history.touch(&id);
        }
        Ok(history)
    }

    pub fn rank(&self, id: &str) -> Option<usize> {
        self.ids.iter().position(|candidate| candidate == id)
    }

    fn touch(&mut self, selector: &str) {
        let id = selector.rsplit_once(':').map_or(selector, |(id, _)| id);
        if !id.contains('/') {
            return;
        }
        self.ids.retain(|candidate| candidate != id);
        self.ids.insert(0, id.to_owned());
        self.ids.truncate(5);
        debug_assert!(self.ids.len() <= 5);
    }

    pub fn record(&mut self, selector: &str) -> anyhow::Result<()> {
        if let Some((lead, sidekick)) = crate::modal::parse_fusion(selector) {
            self.touch(sidekick);
            self.touch(lead);
        } else {
            self.touch(selector);
        }
        if let Some(path) = &self.path {
            let parent = path.parent().expect("历史文件位于配置目录");
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(&mut file, &self.ids)?;
            file.flush()?;
            file.persist(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RecentModels;

    #[test]
    fn recent_models_persist_deduplicate_and_bound_history() {
        let root = tempfile::tempdir().unwrap();
        let mut history = RecentModels::load(root.path()).unwrap();
        for id in 0..8 {
            history.record(&format!("p/m{id}:high")).unwrap();
        }
        history.record("p/m6:low").unwrap();
        history.record("fusion:p/m6:high+p/m7:off").unwrap();
        let loaded = RecentModels::load(root.path()).unwrap();
        assert_eq!(loaded.ids, ["p/m6", "p/m7", "p/m5", "p/m4", "p/m3"]);
    }
}
