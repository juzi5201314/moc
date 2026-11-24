//! 用户最近主动选择的模型；所有前端共用，独立于静态配置与会话日志。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use moc_i18n::t;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    model: Option<String>,
    /// 最近使用的完整 selector，新到旧；`[models] recent` 为 0 时不记录。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent: Vec<String>,
}

fn read(home: &Path) -> Result<Selection, CoreError> {
    let text = super::editor::contents(&home.join("model-selection.toml"))?;
    toml::from_str(&text).map_err(|error| {
        CoreError::Config(t!("core.err.selection_invalid", error = error).into_owned())
    })
}

pub fn load(home: &Path) -> Result<Option<String>, CoreError> {
    Ok(read(home)?.model)
}

/// 最近使用列表（新到旧），未经可用性过滤。
pub fn load_recent(home: &Path) -> Result<Vec<String>, CoreError> {
    Ok(read(home)?.recent)
}

/// 记一次主动选择：`model` 换成本次；`recent` 先剔除与本次同 `key` 的旧条目
/// （同一模型的不同思考等级算一条），再前插并截到 `limit` 条。
///
/// 读改写在同一把锁内完成后原子替换；并发选择以最后完成的写入为准。
pub fn save(
    home: &Path,
    selector: &str,
    limit: usize,
    key: impl Fn(&str) -> String,
) -> Result<(), CoreError> {
    std::fs::create_dir_all(home)?;
    let _guard = crate::session::lock::SessionLockGuard::acquire(
        home.join("model-selection.lock"),
        "model-selection",
    )?;
    let mut selection = read(home)?;
    let current = key(selector);
    selection.recent.retain(|entry| key(entry) != current);
    selection.recent.insert(0, selector.to_owned());
    selection.recent.truncate(limit);
    selection.model = Some(selector.to_owned());
    write(home, &selection)
}

fn write(home: &Path, selection: &Selection) -> Result<(), CoreError> {
    let text = toml::to_string(selection).map_err(|error| CoreError::Config(error.to_string()))?;
    super::editor::write_atomic(&home.join("model-selection.toml"), &text)
}

/// 首次升级时只用全局旧主模型初始化记忆，不覆盖用户已保存的选择。
pub(super) fn migrate(home: &Path, selector: &str) -> Result<(), CoreError> {
    std::fs::create_dir_all(home)?;
    let _guard = crate::session::lock::SessionLockGuard::acquire(
        home.join("model-selection.lock"),
        "model-selection",
    )?;
    let mut selection = read(home)?;
    if selection.model.is_none() {
        selection.model = Some(selector.to_owned());
        write(home, &selection)?;
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
