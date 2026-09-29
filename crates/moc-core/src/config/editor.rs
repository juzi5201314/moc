//! 只保存显式覆盖项；提交前比对版本，避免覆盖外部编辑器的并发修改。

use super::{ResolvedConfig, catalog, merge_optional_file, merge_required_file, merge_value};
use crate::{CoreConfig, CoreError};
use moc_protocol::{ConfigScope, SettingsDocument};
use serde_json::{Value as Json, json};
use std::path::{Path, PathBuf};

fn path(home: &Path, cwd: &Path, scope: ConfigScope) -> PathBuf {
    match scope {
        ConfigScope::Global => home.join("config.toml"),
        ConfigScope::Project => cwd.join(".moc/config.toml"),
    }
}

fn contents(path: &Path) -> Result<String, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn revision(text: &str) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
}
fn parse(text: &str) -> Result<toml::Value, CoreError> {
    toml::from_str(text).map_err(|error| CoreError::Config(format!("配置文件格式有误：{error}")))
}

fn merged(
    boot: &CoreConfig,
    scope: ConfigScope,
    cwd: &Path,
    document: toml::Value,
) -> Result<(toml::Value, toml::Value), CoreError> {
    let home = super::expand_home_path(&boot.home);
    let mut base = parse(include_str!("defaults.toml"))?;
    if scope == ConfigScope::Global {
        merge_value(&mut base, document.clone());
    } else {
        merge_optional_file(&mut base, &home.join("config.toml"))?;
    }
    let mut effective = base.clone();
    let project_enabled = base
        .get("sources")
        .and_then(|v| v.get("project_config"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    if project_enabled {
        if scope == ConfigScope::Project {
            merge_value(&mut effective, document);
        } else {
            merge_optional_file(&mut effective, &cwd.join(".moc/config.toml"))?;
        }
    }
    for overlay in &boot.overlays {
        merge_required_file(&mut effective, overlay)?;
    }
    Ok((base, effective))
}

pub fn inspect(
    boot: &CoreConfig,
    scope: ConfigScope,
    cwd: &Path,
) -> Result<SettingsDocument, CoreError> {
    let home = super::expand_home_path(&boot.home);
    let path = path(&home, cwd, scope);
    let text = contents(&path)?;
    let document = parse(&text)?;
    let (_, inherited) = merged(boot, scope, cwd, toml::Value::Table(Default::default()))?;
    let (_, mut effective) = merged(boot, scope, cwd, document.clone())?;
    super::apply_model_defaults(&mut effective)?;
    let mut effective =
        serde_json::to_value(effective).map_err(|error| CoreError::Config(error.to_string()))?;
    catalog::complete_view(&mut effective);
    Ok(SettingsDocument {
        path: path.to_string_lossy().into_owned(),
        revision: revision(&text),
        document: serde_json::to_value(document)
            .map_err(|error| CoreError::Config(error.to_string()))?,
        fields: catalog::fields(&effective),
        effective,
        inherited: serde_json::to_value(inherited)
            .map_err(|error| CoreError::Config(error.to_string()))?,
        defaults: serde_json::to_value(parse(include_str!("defaults.toml"))?)
            .map_err(|error| CoreError::Config(error.to_string()))?,
        provider_template: catalog::provider_template(),
        model_template: catalog::model_template(),
    })
}

pub fn save(
    boot: &CoreConfig,
    scope: ConfigScope,
    cwd: &Path,
    mut document: Json,
    expected: &str,
) -> Result<ResolvedConfig, CoreError> {
    let home = super::expand_home_path(&boot.home);
    let target = path(&home, cwd, scope);
    let parent = target.parent().expect("配置文件父目录");
    std::fs::create_dir_all(parent)?;
    let _guard =
        crate::session::lock::SessionLockGuard::acquire(parent.join("config.lock"), "config")?;
    if revision(&contents(&target)?) != expected {
        return Err(CoreError::Config(
            "配置文件已被其他窗口修改；按 Ctrl+Z 撤回这次改动，或重新打开设置页读取最新配置".into(),
        ));
    }
    remove_nulls(&mut document);
    let text =
        toml::to_string_pretty(&document).map_err(|error| CoreError::Config(error.to_string()))?;
    let parsed = parse(&text)?;
    let (base, effective) = merged(boot, scope, cwd, parsed)?;
    // 校验编辑目标本身，即便项目配置来源暂时关闭，也不保存无效 TOML。
    if scope == ConfigScope::Project {
        let mut checked = base.clone();
        merge_value(&mut checked, parse(&text)?);
        ResolvedConfig::from_merged(
            checked,
            base.clone(),
            home.clone(),
            cwd.to_owned(),
            boot.overlays.clone(),
        )?;
    }
    ResolvedConfig::from_merged(
        effective,
        base.clone(),
        home.clone(),
        cwd.to_owned(),
        boot.overlays.clone(),
    )?;
    // Core 基础配置只包含全局层；项目层仍按会话 cwd 单独解析。
    let mut core_raw = base.clone();
    for overlay in &boot.overlays {
        merge_required_file(&mut core_raw, overlay)?;
    }
    let mut resolved = ResolvedConfig::from_merged(
        core_raw,
        base,
        home,
        boot.cwd.canonicalize()?,
        boot.overlays.clone(),
    )?;
    if let Some(directory) = &boot.session_dir {
        resolved.session.dir = directory.clone();
    }
    let temp = parent.join(format!(".config-{}.tmp", crate::ids::entry_id()));
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &target)?;
        Ok::<_, CoreError>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(resolved)
}

pub fn merge_json(target: &mut Json, overlay: Json) {
    match (target, overlay) {
        (Json::Object(target), Json::Object(overlay)) => {
            for (key, value) in overlay {
                merge_json(target.entry(key).or_insert(Json::Null), value);
            }
        }
        (target, overlay) => *target = overlay,
    }
}

pub fn remove_nulls(value: &mut Json) {
    match value {
        Json::Object(object) => {
            object.retain(|_, value| !value.is_null());
            for value in object.values_mut() {
                remove_nulls(value);
            }
        }
        Json::Array(values) => {
            for value in values {
                remove_nulls(value);
            }
        }
        _ => {}
    }
}

pub fn empty_document() -> Json {
    json!({})
}
