//! 插件清单 `plugin.json`：目录入口、名称规范与基础校验。
//!
//! 清单是权威元数据；模块 `default` 导出里的 `name`/`version` 只做一致性检查。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{PluginError, Result};

const DEFAULT_MAIN: &str = "index.js";

/// 一个插件目录的清单。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// 插件名；同时是 KV 目录名与 `plugin/list` 的标识。
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// ESM 入口，相对插件目录；默认 `index.js`。
    #[serde(default = "default_main")]
    pub main: String,
}

fn default_main() -> String {
    DEFAULT_MAIN.to_owned()
}

impl Manifest {
    /// 读取并校验 `<dir>/plugin.json`。
    pub fn load(dir: &Path) -> Result<Self> {
        let file = dir.join("plugin.json");
        let text = std::fs::read_to_string(&file).map_err(|error| {
            PluginError::new(
                "E_PLUGIN_MANIFEST",
                format!("读取 {}: {error}", file.display()),
            )
        })?;
        let manifest: Self = serde_json::from_str(&text).map_err(|error| {
            PluginError::new(
                "E_PLUGIN_MANIFEST",
                format!("解析 {}: {error}", file.display()),
            )
        })?;
        manifest.validate(dir)?;
        Ok(manifest)
    }

    /// 入口文件的绝对路径。
    pub fn entry(&self, dir: &Path) -> PathBuf {
        dir.join(&self.main)
    }

    fn validate(&self, dir: &Path) -> Result<()> {
        if !valid_name(&self.name) {
            return Err(PluginError::new(
                "E_PLUGIN_MANIFEST",
                format!(
                    "插件名 `{}` 不合法：需为 1-64 位小写字母、数字或连字符，且以字母开头",
                    self.name
                ),
            ));
        }
        let main = Path::new(&self.main);
        let extension = main.extension().and_then(|value| value.to_str());
        if main.is_absolute()
            || main.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
            || !matches!(extension, Some("js" | "mjs"))
        {
            return Err(PluginError::new(
                "E_PLUGIN_MANIFEST",
                format!("main `{}` 必须是插件目录内的 .js/.mjs 相对路径", self.main),
            ));
        }
        let entry = dir.join(main);
        if !entry.is_file() {
            return Err(PluginError::new(
                "E_PLUGIN_MANIFEST",
                format!("入口文件不存在：{}", entry.display()),
            ));
        }
        Ok(())
    }
}

/// 插件名：1-64 位小写字母/数字/连字符，以字母开头（与 skill 名同风格）。
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

/// 工具名：`[A-Za-z0-9_-]{1,64}`；`mcp__` 前缀保留给 MCP 桥接工具。
pub fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
        && !name.starts_with("mcp__")
}

/// 斜杠命令名：与插件名同规则，直接用作 `/name`。
pub fn valid_command_name(name: &str) -> bool {
    valid_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("plugin.json"), json).unwrap();
        std::fs::write(dir.join("index.js"), "export default {};").unwrap();
    }

    #[test]
    fn accepts_a_minimal_manifest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("hello");
        plugin(&dir, r#"{"name":"hello"}"#);
        let manifest = Manifest::load(&dir).unwrap();
        assert_eq!(manifest.name, "hello");
        assert_eq!(manifest.main, "index.js");
        assert_eq!(manifest.entry(&dir), dir.join("index.js"));
    }

    #[test]
    fn rejects_unknown_fields_bad_names_and_escaping_entry() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            r#"{"name":"hello","extra":1}"#,
            r#"{"name":"Hello"}"#,
            r#"{"name":"-bad"}"#,
            r#"{"name":"hello","main":"../escape.js"}"#,
            r#"{"name":"hello","main":"index.ts"}"#,
            r#"{"name":"hello","main":"missing.js"}"#,
        ];
        for (index, json) in cases.iter().enumerate() {
            let dir = root.path().join(format!("case{index}"));
            plugin(&dir, json);
            assert!(Manifest::load(&dir).is_err(), "{json}");
        }
    }

    #[test]
    fn tool_names_reserve_the_mcp_prefix() {
        assert!(valid_tool_name("read_file"));
        assert!(valid_tool_name("Read-Tool2"));
        assert!(!valid_tool_name("mcp__github__search"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("has space"));
    }
}
