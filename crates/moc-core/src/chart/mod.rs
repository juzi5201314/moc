//! 表格 → 图表的语义层：解析（[`table`]）、规划（[`plan`]）、绘制模型（[`spec`]）。
//!
//! 本模块只处理纯文本，不引入渲染、字体或终端依赖：moc-tui 负责把 [`ChartSpec`]
//! 栅格化成图片。三层之间的契约：
//! - [`analyze`] 把 Markdown 单元格解析成数值语义并判定列角色；
//! - [`plan`] 按固定顺序选一种图型，返回 `None` 表示这张表不该出图；
//! - [`build`] 把规划结果映射成按类别对齐的点集。

pub mod plan;
pub mod spec;
pub mod table;

pub use plan::{ChartKind, ChartPlan, Polarity, plan};
pub use spec::{ChartSpec, Point, Series, build};
pub use table::{
    Cell, Column, ColumnRole, Composition, Dimension, Number, RawTable, TableAnalysis, analyze,
};
