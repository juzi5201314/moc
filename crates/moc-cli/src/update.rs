use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "juzi5201314/moc";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("moc-updater/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

struct Updater {
    client: reqwest::Client,
    api_base: String,
    exe: PathBuf,
}

pub async fn run(requested: Option<String>) -> Result<()> {
    let updater = Updater::new()?;
    updater.cleanup_stale();
    updater.execute(requested).await
}

impl Updater {
    fn new() -> Result<Self> {
        Self::with_api(
            format!("https://api.github.com/repos/{REPO}"),
            std::env::current_exe().context("无法定位 moc 可执行文件")?,
        )
    }

    fn with_api(api_base: String, exe: PathBuf) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(300))
            .build()
            .context("无法创建 HTTP 客户端")?;
        Ok(Self {
            client,
            api_base,
            exe,
        })
    }

    async fn execute(&self, requested: Option<String>) -> Result<()> {
        let release = match requested.as_deref() {
            Some(input) => {
                let release = self.fetch_requested(input).await?;
                let current = Version::parse(CURRENT_VERSION).expect("Cargo 版本号必须合法");
                if parse_tag(&release.tag_name).as_ref() == Some(&current) {
                    println!("已经是最新版本（v{CURRENT_VERSION}）");
                    return Ok(());
                }
                release
            }
            None => {
                let Some(release) = self.fetch_latest().await? else {
                    println!("已经是最新版本（v{CURRENT_VERSION}）");
                    return Ok(());
                };
                if !is_newer(&release.tag_name, CURRENT_VERSION)? {
                    println!("已经是最新版本（v{CURRENT_VERSION}）");
                    return Ok(());
                }
                release
            }
        };

        let tag = release.tag_name.as_str();
        let Some(asset_name) = asset_file_name(tag) else {
            println!(
                "版本 {tag} 不支持当前平台（{}-{}）",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            return Ok(());
        };
        let Some(asset) = release.assets.iter().find(|asset| asset.name == asset_name) else {
            println!(
                "版本 {tag} 不支持当前平台（{}-{}）",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            return Ok(());
        };
        let checksum_name = format!("{asset_name}.sha256");
        let Some(checksum_asset) = release
            .assets
            .iter()
            .find(|asset| asset.name == checksum_name)
        else {
            bail!("版本 {tag} 缺少 {checksum_name} 校验文件");
        };

        println!("正在更新到 {tag} ...");
        let checksum = self
            .download_text(&checksum_asset.browser_download_url)
            .await?;
        let expected = parse_checksum(&checksum)?;
        let bytes = self.download_bytes(&asset.browser_download_url).await?;
        verify_sha256(&bytes, &expected)?;
        self.install(&bytes)?;

        println!("已更新到 {tag}（原版本 v{CURRENT_VERSION}），重新运行 moc 生效");
        Ok(())
    }

    async fn fetch_latest(&self) -> Result<Option<Release>> {
        let url = self.release_url(None)?;
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("无法访问 GitHub Releases")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .context("GitHub Releases 请求失败")?;
        response
            .json()
            .await
            .context("无法解析 Release 数据")
            .map(Some)
    }

    async fn fetch_requested(&self, input: &str) -> Result<Release> {
        let input = input.trim();
        if input.is_empty() {
            bail!("版本号不能为空");
        }
        let mut candidates = vec![input.to_owned()];
        match input.strip_prefix('v') {
            Some(rest) if !rest.is_empty() => candidates.push(rest.to_owned()),
            None => candidates.push(format!("v{input}")),
            Some(_) => {}
        }
        for tag in candidates {
            if let Some(release) = self.fetch_tag(&tag).await? {
                return Ok(release);
            }
        }
        bail!("未找到版本 {input}")
    }

    async fn fetch_tag(&self, tag: &str) -> Result<Option<Release>> {
        let url = self.release_url(Some(tag))?;
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("无法访问 GitHub Releases")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .context("GitHub Releases 请求失败")?;
        response
            .json()
            .await
            .context("无法解析 Release 数据")
            .map(Some)
    }

    fn release_url(&self, tag: Option<&str>) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.api_base).context("无效的 GitHub API 地址")?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow!("无效的 GitHub API 地址"))?;
        segments.push("releases");
        match tag {
            Some(tag) => {
                segments.push("tags");
                segments.push(tag);
            }
            None => {
                segments.push("latest");
            }
        }
        drop(segments);
        Ok(url)
    }

    async fn download_text(&self, url: &str) -> Result<String> {
        let bytes = self.download_bytes(url).await?;
        String::from_utf8(bytes).context("校验文件不是 UTF-8 文本")
    }

    async fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("下载 {url} 失败"))?;
        let response = response
            .error_for_status()
            .with_context(|| format!("下载 {url} 失败"))?;
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .with_context(|| format!("读取 {url} 失败"))
    }

    fn install(&self, bytes: &[u8]) -> Result<()> {
        let dir = self.exe.parent().context("无法确定 moc 所在目录")?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = dir.join(format!(".moc-update-{}-{stamp}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("无法创建 {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("无法写入 {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("无法写入 {}", tmp.display()))?;
        drop(file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&self.exe)
                .map(|meta| meta.permissions().mode())
                .unwrap_or(0o755);
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("无法设置 {} 的权限", tmp.display()))?;
        }

        if let Err(err) = replace_file(&tmp, &self.exe) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).context("无法替换 moc 可执行文件");
        }
        Ok(())
    }

    #[cfg(windows)]
    fn cleanup_stale(&self) {
        let Some(dir) = self.exe.parent() else {
            return;
        };
        let Some(name) = self.exe.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let prefix = format!("{name}.old-");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    #[cfg(not(windows))]
    fn cleanup_stale(&self) {}
}

#[cfg(unix)]
fn replace_file(tmp: &Path, exe: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, exe)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, exe: &Path) -> std::io::Result<()> {
    // Windows 允许重命名正在运行的 exe，但不允许直接覆盖；先把旧文件挪走再落新文件。
    let old = exe.with_file_name(format!(
        "{}.old-{}",
        exe.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "moc.exe".into()),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(err) = std::fs::rename(tmp, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(err);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_file(_tmp: &Path, _exe: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台不支持自更新",
    ))
}

fn asset_file_name(tag: &str) -> Option<String> {
    let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64-musl"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64-msvc"
    } else {
        return None;
    };
    let extension = if cfg!(windows) { ".exe" } else { "" };
    Some(format!("moc-{tag}-{platform}{extension}"))
}

fn parse_tag(tag: &str) -> Option<Version> {
    let tag = tag.trim();
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(version).ok()
}

fn is_newer(tag: &str, current: &str) -> Result<bool> {
    let latest = parse_tag(tag).with_context(|| format!("无法解析版本号 {tag}"))?;
    let current = Version::parse(current).context("当前版本号无效")?;
    Ok(latest > current)
}

fn parse_checksum(text: &str) -> Result<String> {
    let hash = text.split_whitespace().next().context("校验文件为空")?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("校验文件格式无效");
    }
    Ok(hash.to_ascii_lowercase())
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("下载的二进制校验失败（期望 {expected}，实际 {actual}）");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parses_version_tags() {
        assert_eq!(parse_tag("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_tag("1.2.3-rc.2"), Version::parse("1.2.3-rc.2").ok());
        assert_eq!(parse_tag("nightly"), None);
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.0", "0.1.0").unwrap());
        assert!(!is_newer("v0.1.0", "0.1.0").unwrap());
        assert!(!is_newer("v0.1.0-rc.1", "0.1.0").unwrap());
        assert!(is_newer("v0.1.0", "0.1.0-rc.1").unwrap());
    }

    #[test]
    fn parses_checksums() {
        let hash = "a".repeat(64);
        assert_eq!(parse_checksum(&format!("{hash}  moc\n")).unwrap(), hash);
        assert!(parse_checksum("").is_err());
        assert!(parse_checksum("not-a-hash  moc").is_err());
    }

    #[tokio::test]
    async fn installs_matching_release_asset() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("moc");
        std::fs::write(&exe, b"old").unwrap();

        let payload = b"new moc binary";
        let hash = format!("{:x}", Sha256::digest(payload));
        let Some(asset_name) = asset_file_name("v9.9.9") else {
            return;
        };
        let checksum_name = format!("{asset_name}.sha256");
        let release = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [
                {
                    "name": asset_name,
                    "browser_download_url": format!("{}/asset", server.uri()),
                },
                {
                    "name": checksum_name,
                    "browser_download_url": format!("{}/checksum", server.uri()),
                },
            ],
        });

        Mock::given(method("GET"))
            .and(path("/repos/test/moc/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/asset"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/checksum"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!("{hash}  {asset_name}\n")),
            )
            .mount(&server)
            .await;

        let updater =
            Updater::with_api(format!("{}/repos/test/moc", server.uri()), exe.clone()).unwrap();
        updater.execute(None).await.unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), payload);
    }
}
