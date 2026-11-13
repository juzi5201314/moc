use super::SessionRecord;
use super::SessionTable;
use super::validate_plugin_state_component;
use super::validate_plugin_state_key;
use crate::error::CoreError;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl SessionTable {
    /// 读取插件在本会话中的一个状态值。插件状态不进入 transcript，且按插件文件隔离。
    pub async fn plugin_state_get(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        let state = self.read_plugin_state(&record, plugin).await?;
        Ok(state.get(key).cloned())
    }

    /// 一次读取完整命名空间，供插件本次回调缓存，避免枚举时逐键重读文件。
    pub async fn plugin_state_snapshot(
        &self,
        session_id: &str,
        plugin: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, CoreError> {
        validate_plugin_state_component(plugin, "plugin")?;
        let record = self.get(session_id).await?;
        self.read_plugin_state(&record, plugin).await
    }

    /// 列出插件在本会话中已保存的状态键。
    pub async fn plugin_state_keys(
        &self,
        session_id: &str,
        plugin: &str,
    ) -> Result<Vec<String>, CoreError> {
        validate_plugin_state_component(plugin, "plugin")?;
        let record = self.get(session_id).await?;
        let state = self.read_plugin_state(&record, plugin).await?;
        Ok(state.keys().cloned().collect())
    }

    /// 原子写入插件状态；磁盘失败时不更新内存（状态仅以文件为准）。
    pub async fn plugin_state_set(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        self.ensure_write_lease(session_id, &record, "moc-plugin")
            .await?;
        let _append = record.append_lock.lock().await;
        if !record.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        self.ensure_transcript_header(&record).await?;
        let mut state = self.read_plugin_state(&record, plugin).await?;
        state.insert(key.to_owned(), value);
        self.store
            .write_plugin_state(&record.paths, plugin, &state)
            .await
    }

    /// 原子删除插件状态；返回是否存在该键。
    pub async fn plugin_state_remove(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
    ) -> Result<bool, CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        self.ensure_write_lease(session_id, &record, "moc-plugin")
            .await?;
        let _append = record.append_lock.lock().await;
        if !record.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        let mut state = self.read_plugin_state(&record, plugin).await?;
        let removed = state.remove(key).is_some();
        if removed {
            self.store
                .write_plugin_state(&record.paths, plugin, &state)
                .await?;
        }
        Ok(removed)
    }

    pub(super) async fn read_plugin_state(
        &self,
        record: &Arc<SessionRecord>,
        plugin: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, CoreError> {
        let ensure_active = || {
            if record.active.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(CoreError::Session(
                    "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
                ))
            }
        };
        ensure_active()?;
        let state = self.store.read_plugin_state(&record.paths, plugin).await;
        ensure_active()?;
        state
    }
}
