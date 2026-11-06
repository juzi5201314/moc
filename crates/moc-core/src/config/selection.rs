//! 用户最近主动选择的模型；所有前端共用，独立于静态配置与会话日志。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    model: Option<String>,
}

pub fn load(home: &Path) -> Result<Option<String>, CoreError> {
    let text = super::editor::contents(&home.join("model-selection.toml"))?;
    let selection: Selection = toml::from_str(&text)
        .map_err(|error| CoreError::Config(format!("模型选择记录无效：{error}")))?;
    Ok(selection.model)
}

/// 单值文档原子替换；并发选择以最后完成的写入为准，不会出现半条 selector。
pub fn save(home: &Path, selector: &str) -> Result<(), CoreError> {
    std::fs::create_dir_all(home)?;
    let _guard = crate::session::lock::SessionLockGuard::acquire(
        home.join("model-selection.lock"),
        "model-selection",
    )?;
    write(home, selector)
}

fn write(home: &Path, selector: &str) -> Result<(), CoreError> {
    let text = toml::to_string(&Selection {
        model: Some(selector.to_owned()),
    })
    .map_err(|error| CoreError::Config(error.to_string()))?;
    super::editor::write_atomic(&home.join("model-selection.toml"), &text)
}

/// 首次升级时只用全局旧主模型初始化记忆，不覆盖用户已保存的选择。
pub(super) fn migrate(home: &Path, selector: &str) -> Result<(), CoreError> {
    std::fs::create_dir_all(home)?;
    let _guard = crate::session::lock::SessionLockGuard::acquire(
        home.join("model-selection.lock"),
        "model-selection",
    )?;
    if load(home)?.is_none() {
        write(home, selector)?;
    }
    Ok(())
}

/// 旧角色只在配置读取边界移除，不再进入运行时或设置目录。
pub(super) fn remove_legacy_roles(raw: &mut toml::Value) {
    if let Some(roles) = raw
        .get_mut("models")
        .and_then(|models| models.get_mut("roles"))
        .and_then(toml::Value::as_table_mut)
    {
        roles.remove("default");
        roles.remove("plan");
    }
}
