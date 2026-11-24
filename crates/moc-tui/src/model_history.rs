//! TUI 模型选择器里的「最近使用」：数据来自 core 共享的最近选择列表
//! （`config/get` 的 `recent_models`），本地只把完整 selector 投影成模型 ID 的排名。

/// 条数受 `[models] recent` 约束（core 已截断），按最近使用顺序线性查找。
#[derive(Default)]
pub struct RecentModels {
    ids: Vec<String>,
}

impl RecentModels {
    /// selector 去掉思考等级后按模型去重；Fusion 组合的 lead、sidekick 各算一个模型。
    pub fn from_selectors(selectors: &[String]) -> Self {
        let mut ids: Vec<String> = Vec::new();
        for selector in selectors {
            let sides = match crate::modal::parse_fusion(selector) {
                Some((lead, sidekick)) => [Some(lead), Some(sidekick)],
                None => [Some(selector.as_str()), None],
            };
            // core 存的是规范 selector，等级恒在末段，rsplit 不会切进带冒号的模型 ID。
            for side in sides.into_iter().flatten() {
                let id = side.rsplit_once(':').map_or(side, |(id, _)| id);
                if !ids.iter().any(|candidate| candidate == id) {
                    ids.push(id.to_owned());
                }
            }
        }
        Self { ids }
    }

    pub fn rank(&self, id: &str) -> Option<usize> {
        self.ids.iter().position(|candidate| candidate == id)
    }
}

#[cfg(test)]
mod tests {
    use super::RecentModels;

    #[test]
    fn recent_selectors_rank_models_and_expand_fusion_sides() {
        let selectors = ["p/m6:low", "fusion:p/m6:high+p/m7:off", "p/m5:off"].map(String::from);
        let recent = RecentModels::from_selectors(&selectors);
        assert_eq!(recent.ids, ["p/m6", "p/m7", "p/m5"]);
        assert_eq!(recent.rank("p/m7"), Some(1));
        assert_eq!(recent.rank("p/other"), None);
    }
}
