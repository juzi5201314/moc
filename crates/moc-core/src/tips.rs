//! 前端空闲时展示的一句话提示；随 `initialize` 下发，前端负责随机挑选与轮换。
//!
//! 文案约定：一句话、不提具体按键（各前端键位不同），只讲命令、配置与概念；
//! 反引号包住的片段由前端按代码样式渲染。

const TIP_KEYS: &[&str] = &[
    "core.tips.tip_01",
    "core.tips.tip_02",
    "core.tips.tip_03",
    "core.tips.tip_04",
    "core.tips.tip_05",
    "core.tips.tip_06",
    "core.tips.tip_07",
    "core.tips.tip_08",
    "core.tips.tip_09",
    "core.tips.tip_10",
    "core.tips.tip_11",
    "core.tips.tip_12",
    "core.tips.tip_13",
    "core.tips.tip_14",
    "core.tips.tip_15",
    "core.tips.tip_16",
    "core.tips.tip_17",
    "core.tips.tip_18",
    "core.tips.tip_19",
];

pub fn tips() -> Vec<String> {
    TIP_KEYS
        .iter()
        .map(|key| moc_i18n::t!(*key).into_owned())
        .collect()
}
