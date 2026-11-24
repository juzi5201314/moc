//! Image Playground 独立配置文件：`{config_home}/image-playground.toml`。

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use moc_i18n::t;
use moc_llm::EditEncoding;
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "image-playground.toml";

/// 静态生成图本地自动压缩目标格式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCompressionFormat {
    /// 原样保存上游返回的格式与字节。
    #[default]
    Original,
    /// 有损 WebP（带质量调节，保留透明通道）。
    Webp,
    /// 无损 WebP。
    WebpLossless,
    /// AVIF（纯 Rust ravif，带质量调节）。
    Avif,
    /// JPEG（带质量调节，透明像素铺白底）。
    Jpeg,
    /// PNG（无损压缩）。
    Png,
    /// APNG（静态图等价于高压缩 PNG；动图请看 `animated_format`）。
    Apng,
}

/// 动图（如 GIF / 动图 WebP）本地自动压缩策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimatedOutputFormat {
    /// 原样保留（例如原 GIF）。
    #[default]
    Original,
    /// 转为动图 WebP（带质量调节）。
    WebpAnimated,
    /// 转为真彩无损 APNG（`.png`）。
    Apng,
}

/// 收到生成图后的本地自动压缩配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputCompressionConfig {
    pub format: OutputCompressionFormat,
    /// 1..=100，用于 `webp` / `avif` / `jpeg`。
    pub quality: u8,
    pub animated_format: AnimatedOutputFormat,
    /// 1..=100，用于 `webp_animated`。
    pub animated_quality: u8,
}

impl Default for OutputCompressionConfig {
    fn default() -> Self {
        Self {
            format: OutputCompressionFormat::Original,
            quality: 85,
            animated_format: AnimatedOutputFormat::Original,
            animated_quality: 80,
        }
    }
}

/// Edit 上传参考图的保护性预处理配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputCompressionConfig {
    /// 开启后仅在参考图超过尺寸/体积或非标准格式时温和缩放转码；关闭则原图直传。
    pub enabled: bool,
    /// 长边像素上限；0 表示不缩放。
    pub max_dimension: u32,
    /// 单图体积上限（MiB）。
    pub max_bytes_mb: f32,
}

impl Default for InputCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_dimension: 2048,
            max_bytes_mb: 4.0,
        }
    }
}

/// Image Playground 独立设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaygroundConfig {
    /// 本地图片与索引存放目录，支持 `~` 展开。
    pub storage_dir: String,
    pub compression: OutputCompressionConfig,
    pub input_compress: InputCompressionConfig,
    pub default_edit_encoding: EditEncoding,
}

impl Default for PlaygroundConfig {
    fn default() -> Self {
        Self {
            storage_dir: "~/.moc/image-playground".into(),
            compression: OutputCompressionConfig::default(),
            input_compress: InputCompressionConfig::default(),
            default_edit_encoding: EditEncoding::Multipart,
        }
    }
}

impl PlaygroundConfig {
    /// 校验并规范化配置值。
    pub fn validate(&mut self) -> anyhow::Result<()> {
        let dir = self.storage_dir.trim();
        if dir.is_empty() {
            anyhow::bail!("{}", t!("web.playground.storage_dir_empty"));
        }
        self.storage_dir = dir.to_owned();
        self.compression.quality = self.compression.quality.clamp(1, 100);
        self.compression.animated_quality = self.compression.animated_quality.clamp(1, 100);
        if !self.input_compress.max_bytes_mb.is_finite() || self.input_compress.max_bytes_mb <= 0.0
        {
            anyhow::bail!("{}", t!("web.playground.max_bytes_positive"));
        }
        Ok(())
    }

    /// 展开 `storage_dir` 为实际绝对路径。
    pub fn resolved_storage_dir(&self, config_home: &Path) -> PathBuf {
        let raw = self.storage_dir.trim();
        if raw == "~/.moc/image-playground" {
            return config_home.join("image-playground");
        }
        if let Some(rest) = raw
            .strip_prefix("~/.moc/")
            .or_else(|| raw.strip_prefix("~\\.moc\\"))
        {
            return config_home.join(rest);
        }
        let expanded = moc_core::config::expand_user_path(raw);
        if expanded.is_absolute() {
            expanded
        } else {
            config_home.join(expanded)
        }
    }
}

pub fn load(config_home: &Path) -> anyhow::Result<PlaygroundConfig> {
    let path = config_home.join(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(PlaygroundConfig::default()),
        Ok(text) => {
            let mut cfg: PlaygroundConfig = toml::from_str(&text).with_context(|| {
                t!("web.playground.parse_file_failed", path = path.display()).into_owned()
            })?;
            cfg.validate()?;
            Ok(cfg)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PlaygroundConfig::default())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn save(config_home: &Path, mut cfg: PlaygroundConfig) -> anyhow::Result<PlaygroundConfig> {
    cfg.validate()?;
    std::fs::create_dir_all(config_home)?;
    let path = config_home.join(CONFIG_FILE);
    let text = toml::to_string_pretty(&cfg)?;
    let tmp = tempfile::NamedTempFile::new_in(config_home)?;
    std::fs::write(tmp.path(), &text)?;
    tmp.persist(&path).map_err(|error| error.error)?;
    Ok(cfg)
}
