use crate::error::CoreError;
use moc_i18n::t;
use serde::Deserialize;
use std::collections::BTreeMap;

/// 插件系统配置；插件是本地目录里会被执行的 JS 代码，项目级来源等于信任该仓库。
///
/// 宿主字段保持严格校验；其它符合插件名规则的键原样保留为动态值，具体结构由
/// 插件注册配置项时校验。这样已卸载插件、损坏插件或未来版本写入的值不会阻止 moc
/// 启动，插件本身可以单独被禁用。
#[derive(Debug, Clone)]
pub struct PluginsConfig {
    pub enabled: bool,
    /// 是否扫描用户目录 `~/.moc/plugins`。
    pub user: bool,
    /// 是否扫描项目目录 `.moc/plugins`（cwd 向上到 git root）；开启即信任仓库。
    pub project: bool,
    /// 额外扫描目录；支持 `~` 展开。
    pub dirs: Vec<String>,
    /// 按插件名禁用。
    pub disable: Vec<String>,
    /// 一次最多加载的插件数。
    pub max_plugins: usize,
    /// 单个插件运行时的内存上限。
    pub memory_limit_bytes: usize,
    /// 模块求值（含 `activate`）的最长等待。
    pub load_timeout_ms: u64,
    /// 单次工具/命令/钩子调用的最长等待；超时后本进程内禁用该插件。
    pub call_timeout_ms: u64,
    /// 全部插件提示词贡献的总字符上限。
    pub max_prompt_chars: usize,
    /// `[plugins."插件名"]` 的原始值；不在核心层解释，以便孤立配置继续保存。
    pub configs: BTreeMap<String, toml::Value>,
}

/// `[plugins]` 下由宿主占用的键；插件名不得与这些键冲突。
pub const HOST_PLUGIN_KEYS: &[&str] = &[
    "enabled",
    "user",
    "project",
    "dirs",
    "disable",
    "max_plugins",
    "memory_limit_bytes",
    "load_timeout_ms",
    "call_timeout_ms",
    "max_prompt_chars",
];

/// 判断一个动态键是否可作为插件名。
pub fn is_plugin_key(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'-'
        })
        && !HOST_PLUGIN_KEYS.contains(&name)
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user: true,
            project: true,
            dirs: Vec::new(),
            disable: Vec::new(),
            max_plugins: 32,
            memory_limit_bytes: 64 * 1024 * 1024,
            load_timeout_ms: 5000,
            call_timeout_ms: 5000,
            max_prompt_chars: 4000,
            configs: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for PluginsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut values = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let defaults = Self::default();
        fn take<T: serde::de::DeserializeOwned, E: serde::de::Error>(
            values: &mut BTreeMap<String, toml::Value>,
            key: &str,
            default: T,
        ) -> Result<T, E> {
            values
                .remove(key)
                .map(|value| value.try_into().map_err(E::custom))
                .unwrap_or(Ok(default))
        }
        let mut configs = BTreeMap::new();
        let names: Vec<String> = values.keys().cloned().collect();
        for name in names {
            if HOST_PLUGIN_KEYS.contains(&name.as_str()) {
                continue;
            }
            let value = values.remove(&name).expect("刚从键列表取出的插件值");
            if !is_plugin_key(&name) {
                return Err(serde::de::Error::custom(format!(
                    "plugins 下未知宿主字段或非法插件名 `{name}`"
                )));
            }
            configs.insert(name, value);
        }
        Ok(Self {
            enabled: take(&mut values, "enabled", defaults.enabled)?,
            user: take(&mut values, "user", defaults.user)?,
            project: take(&mut values, "project", defaults.project)?,
            dirs: take(&mut values, "dirs", defaults.dirs)?,
            disable: take(&mut values, "disable", defaults.disable)?,
            max_plugins: take(&mut values, "max_plugins", defaults.max_plugins)?,
            memory_limit_bytes: take(
                &mut values,
                "memory_limit_bytes",
                defaults.memory_limit_bytes,
            )?,
            load_timeout_ms: take(&mut values, "load_timeout_ms", defaults.load_timeout_ms)?,
            call_timeout_ms: take(&mut values, "call_timeout_ms", defaults.call_timeout_ms)?,
            max_prompt_chars: take(&mut values, "max_prompt_chars", defaults.max_prompt_chars)?,
            configs,
        })
    }
}

impl serde::Serialize for PluginsConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map =
            serializer.serialize_map(Some(HOST_PLUGIN_KEYS.len() + self.configs.len()))?;
        map.serialize_entry("enabled", &self.enabled)?;
        map.serialize_entry("user", &self.user)?;
        map.serialize_entry("project", &self.project)?;
        map.serialize_entry("dirs", &self.dirs)?;
        map.serialize_entry("disable", &self.disable)?;
        map.serialize_entry("max_plugins", &self.max_plugins)?;
        map.serialize_entry("memory_limit_bytes", &self.memory_limit_bytes)?;
        map.serialize_entry("load_timeout_ms", &self.load_timeout_ms)?;
        map.serialize_entry("call_timeout_ms", &self.call_timeout_ms)?;
        map.serialize_entry("max_prompt_chars", &self.max_prompt_chars)?;
        for (name, value) in &self.configs {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl PluginsConfig {
    /// 取得插件的原始配置值；未知插件的值也会保留。
    pub fn plugin_value(&self, name: &str) -> Option<&toml::Value> {
        self.configs.get(name)
    }

    /// 枚举全部插件配置，顺序稳定且包含未安装插件。
    pub fn plugin_values(&self) -> impl Iterator<Item = (&str, &toml::Value)> {
        self.configs
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// 转成运行时设置；`dirs` 在这里展开 `~`。
    pub fn settings(&self) -> moc_plugin::Settings {
        moc_plugin::Settings {
            discovery: moc_plugin::discovery::DiscoveryConfig {
                enabled: self.enabled,
                user: self.user,
                project: self.project,
                dirs: self
                    .dirs
                    .iter()
                    .map(|dir| crate::config::expand_user_path(dir))
                    .collect(),
            },
            disable: self.disable.clone(),
            max_plugins: self.max_plugins,
            memory_limit_bytes: self.memory_limit_bytes,
            load_timeout: std::time::Duration::from_millis(self.load_timeout_ms),
            call_timeout: std::time::Duration::from_millis(self.call_timeout_ms),
            max_prompt_chars: self.max_prompt_chars,
        }
    }
}

/// 插件配置的数值校验；0 值一律拒绝，避免出现「永远超时」这类静默失效。
pub(in super::super) fn validate_plugins(plugins: &PluginsConfig) -> Result<(), CoreError> {
    for (name, value) in [
        ("plugins.max_plugins", plugins.max_plugins as u64),
        (
            "plugins.memory_limit_bytes",
            plugins.memory_limit_bytes as u64,
        ),
        ("plugins.max_prompt_chars", plugins.max_prompt_chars as u64),
        ("plugins.load_timeout_ms", plugins.load_timeout_ms),
        ("plugins.call_timeout_ms", plugins.call_timeout_ms),
    ] {
        if value == 0 {
            return Err(CoreError::Config(
                t!("core.err.must_be_positive", name = name).into_owned(),
            ));
        }
    }
    Ok(())
}
