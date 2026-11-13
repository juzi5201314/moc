//! 引擎、权限菜单与模型选择显示。

use super::{App, LegendItem, LegendMenu, engine_label};
use moc_protocol::methods::{SessionEngine, SessionPermission};

impl LegendItem for SessionEngine {
    fn label(&self) -> &'static str {
        engine_label(*self)
    }

    fn description(&self) -> &'static str {
        match self {
            SessionEngine::Standard => "默认引擎：模型直接调用工具",
            SessionEngine::Ptc => "模型只调用 execute，全部工具经代码沙盒触达",
            SessionEngine::ClmExperimental => "模型自主维护实时上下文（实验）",
        }
    }
}

impl LegendItem for SessionPermission {
    fn label(&self) -> &'static str {
        self.as_str()
    }

    fn description(&self) -> &'static str {
        match self {
            SessionPermission::Full => "宿主全部可读写，无沙盒边界",
            SessionPermission::Workspace => "工作区可写，宿主其余路径只读",
            SessionPermission::WorkspaceOnly => "仅工作区与临时目录可见，隔离最强",
        }
    }
}

impl<T> LegendMenu<T> {
    /// 命中的条目下标；点在边框、说明行和菜单外返回 `None`。
    pub fn entry_at(&self, column: u16, row: u16) -> Option<usize> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row.checked_sub(self.area.y.saturating_add(1))?);
        (index < self.items.len()).then_some(index)
    }
}

/// Fusion 配置卡的初始草稿：当前 selector 本身是 Fusion 就沿用它的两侧；
/// 否则取 `config/get` 下发的 `models.fusion` 默认；都没有就把非 Fusion 的
/// 当前值放进 lead，sidekick 留空待选。
pub fn initial_fusion_draft(app: &App, current: &str) -> crate::modal::FusionDraft {
    if let Some((lead, sidekick)) = crate::modal::parse_fusion(current) {
        return crate::modal::FusionDraft {
            lead: lead.to_owned(),
            sidekick: sidekick.to_owned(),
        };
    }
    if let Some(defaults) = &app.fusion_defaults {
        return crate::modal::FusionDraft {
            lead: defaults.lead.clone(),
            sidekick: defaults.sidekick.clone(),
        };
    }
    crate::modal::FusionDraft {
        lead: current.to_owned(),
        sidekick: String::new(),
    }
}

/// 单份模型用量折算美元成本：`input_tokens` 已折入缓存命中，非命中部分按全价、
/// 命中部分按缓存价（无缓存价时退回输入价）计费。
pub(super) fn model_cost(
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    pricing: &moc_protocol::methods::ModelPricing,
) -> f64 {
    let uncached = input_tokens.saturating_sub(cached_tokens);
    let cache_price = pricing.cache_read.unwrap_or(pricing.input);
    uncached as f64 / 1e6 * pricing.input
        + output_tokens as f64 / 1e6 * pricing.output
        + cached_tokens as f64 / 1e6 * cache_price
}

/// 模型展示名：选择串即 `供应商/模型:等级`，思考关闭时省掉 `:off` 后缀；
/// 规范 Fusion selector 展示为 `Fusion · lead + sidekick`。
pub fn model_selector_label(selector: &str) -> String {
    if let Some((lead, sidekick)) = crate::modal::parse_fusion(selector) {
        return format!(
            "Fusion · {} + {}",
            lead.strip_suffix(":off").unwrap_or(lead),
            sidekick.strip_suffix(":off").unwrap_or(sidekick)
        );
    }
    selector.strip_suffix(":off").unwrap_or(selector).to_owned()
}
