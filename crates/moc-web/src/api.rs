//! WebUI 的进程内 JSON 端点：数据只在 `moc web` 进程里现取（procstats 环形
//! 缓冲、特性注册表、平台探测），不进 JSON-RPC 协议表——TUI/CLI 在本进程内
//! 直接读同一份数据。三条路由都挂令牌保护（暴露进程资源与路径信息）。

use std::time::Instant;

use axum::Json;
use axum::extract::State;
use moc_protocol::features::{FEATURES, FlagToggle};
use serde::Serialize;

use crate::AppState;

/// `/features.json`：特性注册表 + 有效状态；与 TUI `/features` 同一份事实。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureRow {
    pub key: &'static str,
    pub stage: &'static str,
    pub enabled: bool,
    pub context_tokens: Option<u32>,
    pub summary: &'static str,
    /// 开关来源：`[features] <key>` 或既有配置路径（如 `workflow.enabled`）。
    pub toggle: String,
}

#[derive(Serialize)]
pub struct FeaturesResponse {
    pub features: Vec<FeatureRow>,
}

pub async fn features(State(state): State<AppState>) -> Json<FeaturesResponse> {
    let states = state.core.features();
    Json(FeaturesResponse {
        features: FEATURES
            .iter()
            .map(|spec| FeatureRow {
                key: spec.key,
                stage: spec.stage.label(),
                enabled: states.enabled(spec.flag),
                context_tokens: spec.context_tokens,
                summary: spec.summary,
                toggle: match spec.toggle {
                    FlagToggle::Features => format!("[features] {}", spec.key),
                    FlagToggle::Existing(path) => path.to_owned(),
                },
            })
            .collect(),
    })
}

/// `/doctor.json`：服务端环境事实；前端再叠上连接与浏览器侧检查成行。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorInfo {
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub config_home: String,
    pub sandbox: SandboxInfo,
    pub git_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInfo {
    pub runner: Option<&'static str>,
    pub modes: Vec<&'static str>,
    pub home_overlay: bool,
}

pub async fn doctor(State(state): State<AppState>) -> Json<DoctorInfo> {
    let support = state.core.sandbox_support();
    let git_version = tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    Json(DoctorInfo {
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        config_home: state.core.config_home().display().to_string(),
        sandbox: SandboxInfo {
            runner: support.runner.map(|runner| runner.name()),
            modes: support.modes.iter().map(|mode| mode.as_str()).collect(),
            home_overlay: support.home_overlay,
        },
        git_version,
    })
}

/// `/procstats.json`：`moc_core::procstats` 环形缓冲的窗口快照。
/// `at` 是 Instant（不可序列化），转成距查询时刻的毫秒数下发。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcStatsResponse {
    pub interval_secs: u64,
    pub window_secs: u64,
    pub samples: Vec<ProcSample>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcSample {
    pub age_ms: u64,
    pub rss_bytes: u64,
    pub cpu_pct: f32,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

pub async fn procstats() -> Json<ProcStatsResponse> {
    let now = Instant::now();
    let samples = moc_core::procstats::samples()
        .iter()
        .map(|sample| ProcSample {
            age_ms: now.saturating_duration_since(sample.at).as_millis() as u64,
            rss_bytes: sample.rss_bytes,
            cpu_pct: sample.cpu_pct,
            disk_read_bytes: sample.disk_read_bytes,
            disk_write_bytes: sample.disk_write_bytes,
            net_rx_bytes: sample.net_rx_bytes,
            net_tx_bytes: sample.net_tx_bytes,
        })
        .collect();
    Json(ProcStatsResponse {
        interval_secs: moc_core::procstats::INTERVAL.as_secs(),
        window_secs: moc_core::procstats::WINDOW_SECS,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 特性表与注册表同序且带上有效状态；守住序列化形状（camelCase）。
    #[tokio::test]
    async fn features_rows_cover_registry() {
        let json = serde_json::to_value(FeaturesResponse {
            features: FEATURES
                .iter()
                .map(|spec| FeatureRow {
                    key: spec.key,
                    stage: spec.stage.label(),
                    enabled: true,
                    context_tokens: spec.context_tokens,
                    summary: spec.summary,
                    toggle: match spec.toggle {
                        FlagToggle::Features => format!("[features] {}", spec.key),
                        FlagToggle::Existing(path) => path.to_owned(),
                    },
                })
                .collect(),
        })
        .expect("可序列化");
        let rows = json["features"].as_array().expect("数组");
        assert_eq!(rows.len(), FEATURES.len());
        assert!(json.to_string().contains("\"contextTokens\""));
        assert!(json.to_string().contains("workflow.enabled"));
    }
}
