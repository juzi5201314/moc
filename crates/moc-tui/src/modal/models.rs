use super::DEFAULT_LEVELS;
use super::DetailRow;
use super::FUSION_ENTRY_SELECTOR;
use super::FusionDraft;
use super::LevelState;
use super::MenuItems;
use super::ModelEntry;
use super::ModelTarget;

/// 思考等级的强弱次序；名字不可识别时比 `off` 还弱，不会成为默认等级。
pub(super) fn level_rank(level: &str) -> usize {
    DEFAULT_LEVELS
        .iter()
        .position(|known| *known == level)
        .map_or(0, |index| index + 1)
}

/// 候选里没有手动指定时的默认等级：`level_rank` 最高者。
pub fn default_level(levels: &[String]) -> Option<String> {
    levels.iter().max_by_key(|level| level_rank(level)).cloned()
}

/// 解析规范 Fusion selector（`fusion:<lead>+<sidekick>`）的两侧；非法形式返回 `None`。
pub fn parse_fusion(selector: &str) -> Option<(&str, &str)> {
    selector.strip_prefix("fusion:")?.split_once('+')
}

/// 把两侧 selector 拼成规范 Fusion selector。
pub fn fusion_selector(lead: &str, sidekick: &str) -> String {
    format!("fusion:{lead}+{sidekick}")
}

/// 带等级 selector 拆成（模型 id, 等级）；没有 `:` 后缀时等级为 `None`。
pub(super) fn split_selector(selector: &str) -> (&str, Option<&str>) {
    selector
        .rsplit_once(':')
        .map_or((selector, None), |(id, level)| (id, Some(level)))
}

/// Fusion 草稿一侧 selector 对应的候选条目（去掉 `:level` 后按 selector 查）。
pub fn side_entry<'a>(entries: &'a [ModelEntry], selector: &str) -> Option<&'a ModelEntry> {
    let (id, _) = split_selector(selector);
    entries.iter().find(|entry| entry.selector == id)
}

/// Fusion 草稿一侧的生效等级：显式等级仍在该模型 levels 内就用它，
/// 否则取 `default_level`；侧边模型或 levels 缺失时为 `None`。
pub fn side_level(entries: &[ModelEntry], selector: &str) -> Option<String> {
    let (_, level) = split_selector(selector);
    let levels = side_entry(entries, selector)?.levels.as_slice();
    level
        .filter(|level| levels.iter().any(|candidate| candidate == level))
        .map(str::to_owned)
        .or_else(|| default_level(levels))
}

/// 换侧边模型：旧等级仍在新模型 levels 内就保留，否则取默认等级；
/// 新模型没有 levels 时写裸 id。
pub fn fusion_side_pick(current: &str, entry: &ModelEntry) -> String {
    let (_, level) = split_selector(current);
    let level = level
        .filter(|level| entry.levels.iter().any(|candidate| candidate == level))
        .map(str::to_owned)
        .or_else(|| default_level(&entry.levels));
    entry.selector_with(level.as_deref())
}

/// 改侧边等级：只替换 `:level` 部分。
pub fn fusion_side_level(current: &str, level: &str) -> String {
    let (id, _) = split_selector(current);
    format!("{id}:{level}")
}

/// 详情卡控制行列表：普通模型给思考等级与 Fast Mode，Fusion 特殊项给两侧
/// 槽位配置与应用行，角色伪值没有任何控制行。
pub fn detail_rows(
    entry: Option<&ModelEntry>,
    entries: &[ModelEntry],
    target: &ModelTarget,
    fusion: &FusionDraft,
) -> Vec<DetailRow> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    let session = matches!(target, ModelTarget::Session);
    if entry.selector == FUSION_ENTRY_SELECTOR {
        let lead = side_entry(entries, &fusion.lead);
        let sidekick = side_entry(entries, &fusion.sidekick);
        let mut rows = vec![DetailRow::Lead];
        if lead.is_some_and(|entry| !entry.levels.is_empty()) {
            rows.push(DetailRow::LeadLevel);
        }
        rows.push(DetailRow::Sidekick);
        if sidekick.is_some_and(|entry| !entry.levels.is_empty()) {
            rows.push(DetailRow::SidekickLevel);
        }
        if session && lead.is_some_and(|entry| entry.fast_mode) {
            rows.push(DetailRow::Fast);
        }
        rows.push(DetailRow::Apply);
        return rows;
    }
    if entry.provider().is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    if !entry.levels.is_empty() {
        rows.push(DetailRow::Level);
    }
    if session && entry.fast_mode {
        rows.push(DetailRow::Fast);
    }
    rows
}

/// 计算某详情行的下拉内容；渲染与键盘/鼠标共用这一份，不各算一遍。
pub fn menu_items<'a>(
    row: DetailRow,
    entry: Option<&'a ModelEntry>,
    entries: &'a [ModelEntry],
    level: &LevelState,
    fusion: &'a FusionDraft,
    query: &str,
) -> MenuItems<'a> {
    let side_levels = |selector: &str| -> (&'a [String], Option<usize>) {
        let levels = side_entry(entries, selector).map_or(&[][..], |entry| entry.levels.as_slice());
        let current = side_level(entries, selector)
            .and_then(|value| levels.iter().position(|level| *level == value));
        (levels, current)
    };
    match row {
        DetailRow::Level => {
            let levels = entry.map_or(&[][..], |entry| entry.levels.as_slice());
            let current = entry
                .and_then(|entry| level.resolve(&entry.levels))
                .and_then(|value| levels.iter().position(|level| *level == value));
            MenuItems::Levels { levels, current }
        }
        DetailRow::LeadLevel => {
            let (levels, current) = side_levels(&fusion.lead);
            MenuItems::Levels { levels, current }
        }
        DetailRow::SidekickLevel => {
            let (levels, current) = side_levels(&fusion.sidekick);
            MenuItems::Levels { levels, current }
        }
        DetailRow::Lead | DetailRow::Sidekick => {
            let (side, _) = split_selector(if row == DetailRow::Lead {
                &fusion.lead
            } else {
                &fusion.sidekick
            });
            let candidates: Vec<(usize, &ModelEntry)> = filtered_entries(entries, query)
                .into_iter()
                .filter(|(_, entry)| {
                    entry.selector != FUSION_ENTRY_SELECTOR && !entry.provider().is_empty()
                })
                .collect();
            let current = candidates
                .iter()
                .position(|(_, entry)| entry.selector == side);
            MenuItems::Models {
                entries: candidates,
                current,
            }
        }
        _ => MenuItems::Levels {
            levels: &[],
            current: None,
        },
    }
}

pub fn filtered_entries<'a>(
    entries: &'a [ModelEntry],
    query: &str,
) -> Vec<(usize, &'a ModelEntry)> {
    let query = query.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let mut matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let fields = [
                entry.selector.to_lowercase(),
                entry.label.to_lowercase(),
                entry.display_name.to_lowercase(),
            ];
            let score = terms.iter().try_fold(0usize, |total, term| {
                fields
                    .iter()
                    .filter_map(|field| model_match_score(field, term))
                    .min()
                    .map(|score| total + score)
            })?;
            Some((score, index, entry))
        })
        .collect();
    matches.sort_by_key(|(score, index, _)| (*score, *index));
    matches
        .into_iter()
        .map(|(_, index, entry)| (index, entry))
        .collect()
}

/// 精确、前缀、子串、子序列依次降权；空搜索保留最近使用/供应商顺序。
pub(super) fn model_match_score(text: &str, term: &str) -> Option<usize> {
    if text == term {
        return Some(0);
    }
    if text.starts_with(term) {
        return Some(1);
    }
    if text.contains(term) {
        return Some(2);
    }
    let mut chars = text.chars();
    term.chars()
        .all(|ch| chars.by_ref().any(|candidate| candidate == ch))
        .then_some(3)
}
