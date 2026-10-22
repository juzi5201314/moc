//! 插件模块解析与加载：只允许插件目录内的相对导入与内建 `moc` 模块。
//!
//! 没有 npm 依赖、没有 `node:` 内建模块、没有绝对路径逃逸：模块名最终必须是插件
//! 目录内的真实 `.js`/`.mjs` 文件，否则解析阶段就报错。

use std::path::{Path, PathBuf};

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{Ctx, Module, Result as JsResult};

use crate::error::{PluginError, Result};

/// 宿主 API 的虚拟模块名。
pub const MOC_MODULE: &str = "moc";
/// `moc` 模块的实现；在 worker 线程内作为 ESM 源码求值。
pub const PRELUDE: &str = include_str!("prelude.js");

pub struct PluginResolver {
    root: PathBuf,
}

impl PluginResolver {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Resolver for PluginResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<String> {
        resolve(&self.root, base, name).map_err(|error| {
            rquickjs::Error::new_loading_message(
                name,
                format!("[{}] {}", error.code, error.message),
            )
        })
    }
}

pub struct PluginLoader {
    root: PathBuf,
}

impl PluginLoader {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Loader for PluginLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> JsResult<Module<'js>> {
        if name == MOC_MODULE {
            return Module::declare(ctx.clone(), MOC_MODULE, PRELUDE);
        }
        let path = Path::new(name);
        if !path.starts_with(&self.root) {
            return Err(rquickjs::Error::new_loading_message(
                name,
                "模块路径不在插件目录内",
            ));
        }
        let source = std::fs::read(path).map_err(|error| {
            rquickjs::Error::new_loading_message(name, format!("读取模块失败：{error}"))
        })?;
        Module::declare(ctx.clone(), name, source)
    }
}

fn resolve(root: &Path, base: &str, name: &str) -> Result<String> {
    if name == MOC_MODULE {
        return Ok(MOC_MODULE.into());
    }
    let candidate = if name.starts_with("./") || name.starts_with("../") {
        let directory = Path::new(base).parent().unwrap_or(root);
        directory.join(name)
    } else if Path::new(name).is_absolute() {
        PathBuf::from(name)
    } else {
        return Err(PluginError::new(
            "E_PLUGIN_IMPORT",
            format!("`{name}` 不是相对路径；插件不能导入 npm 依赖或 node: 内建模块"),
        ));
    };
    let real = candidate.canonicalize().map_err(|error| {
        PluginError::new("E_PLUGIN_IMPORT", format!("解析 `{name}` 失败：{error}"))
    })?;
    if !real.starts_with(root) {
        return Err(PluginError::new(
            "E_PLUGIN_IMPORT",
            format!("`{name}` 指向插件目录之外"),
        ));
    }
    if !matches!(
        real.extension().and_then(|value| value.to_str()),
        Some("js" | "mjs")
    ) {
        return Err(PluginError::new(
            "E_PLUGIN_IMPORT",
            format!("`{name}` 只支持 .js/.mjs 模块"),
        ));
    }
    Ok(real.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_imports_inside_the_plugin_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("util.js"), "export const n = 1;").unwrap();
        let base = root.join("index.js").to_string_lossy().into_owned();
        let resolved = resolve(&root, &base, "./util.js").unwrap();
        assert_eq!(PathBuf::from(resolved), root.join("util.js"));
    }

    #[test]
    fn rejects_bare_specifiers_and_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plugin");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(temp.path().join("escape.js"), "export {};").unwrap();
        let base = root.join("index.js").to_string_lossy().into_owned();
        assert_eq!(
            resolve(&root, &base, "lodash").unwrap_err().code,
            "E_PLUGIN_IMPORT"
        );
        assert_eq!(
            resolve(&root, &base, "node:fs").unwrap_err().code,
            "E_PLUGIN_IMPORT"
        );
        assert_eq!(
            resolve(&root, &base, "../escape.js").unwrap_err().code,
            "E_PLUGIN_IMPORT"
        );
        assert_eq!(resolve(&root, &base, "moc").unwrap(), "moc");
    }
}
