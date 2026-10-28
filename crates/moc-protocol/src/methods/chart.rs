use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::method::{ClientRequest, Method};

/// 分析 Markdown 表格并复用 Core 图表语义规划。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChartAnalyze;

/// Markdown 表格的表头与行。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartAnalyzeParams {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// 可绘图时返回 Core `ChartSpec` 的 JSON 表示，不可绘图时为空对象。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartAnalyzeResult {
    pub spec: Option<Value>,
}

impl Method for ChartAnalyze {
    const NAME: &'static str = "chart/analyze";
    type Params = ChartAnalyzeParams;
    type Result = ChartAnalyzeResult;
}

impl ClientRequest for ChartAnalyze {}
