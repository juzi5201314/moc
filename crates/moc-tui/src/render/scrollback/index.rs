//! 工具批次与嵌套调用的布局索引。

use super::{BatchGroup, BatchIndex, BatchKey, TreeIndex, TreeLayout, TreeShape};
use crate::app::App;
use crate::transcript::{Block, Entry, ToolCallBlock};

impl BatchIndex {
    pub(super) fn sync(&mut self, blocks: &[Entry], generations: &mut Vec<u64>) {
        // 全部 parent_id：自己也是父的嵌套块不能并入同名段（子块会插在段中间）。
        let mut parents = std::collections::HashSet::new();
        for entry in blocks {
            if let Block::Tool(tool) = &entry.block
                && let Some(parent) = tool.parent_id.as_deref()
            {
                parents.insert(parent);
            }
        }
        let mut ids = std::collections::HashMap::new();
        let mut count = 0;
        self.by_block.clear();
        generations.clear();
        for group in &mut self.groups {
            group.members.clear();
        }
        // 上一块的嵌套身份（parent, name, id）与本段首块的下标。
        let mut previous: Option<(&str, &str, &str)> = None;
        let mut run_start = 0usize;
        for (index, entry) in blocks.iter().enumerate() {
            generations.push(entry.generation);
            let key = match &entry.block {
                Block::Tool(tool) => {
                    if let Some(parent) = tool.parent_id.as_deref() {
                        let merges = previous.is_some_and(|(p, name, id)| {
                            p == parent
                                && name == tool.name
                                && !parents.contains(tool.id.as_str())
                                && !parents.contains(id)
                        });
                        if !merges {
                            run_start = index;
                        }
                        previous = Some((parent, tool.name.as_str(), tool.id.as_str()));
                        Some(BatchKey::Run(run_start))
                    } else {
                        previous = None;
                        tool.batch_id
                            .as_deref()
                            .map(|batch| BatchKey::Batch(batch, tool.name.as_str()))
                    }
                }
                _ => {
                    previous = None;
                    None
                }
            };
            let group = key.map(|key| {
                *ids.entry(key).or_insert_with(|| {
                    let id = count;
                    count += 1;
                    if self.groups.len() == id {
                        self.groups.push(BatchGroup::default());
                    }
                    id
                })
            });
            self.by_block.push(group);
            if let Some(group) = group {
                let group = &mut self.groups[group];
                group.members.push(index);
            }
        }
        self.groups.truncate(count);
        for group in &mut self.groups {
            // 单成员的组退回单块渲染，不参与批次：批次按（批次, 工具名）分组，
            // 一批异名调用拆出的单成员组没有可合并对象，画组头只会多一行 `(1)`。
            if group.members.len() == 1 {
                self.by_block[group.members[0]] = None;
                continue;
            }
            let inputs = || {
                group
                    .members
                    .iter()
                    .map(|&index| (index, blocks[index].generation, blocks[index].layout_epoch))
            };
            if group.previous.iter().copied().ne(inputs()) {
                group.generation = crate::transcript::next_layout_epoch();
                group.previous.clear();
                group.previous.extend(inputs());
            }
            debug_assert_eq!(group.previous.len(), group.members.len());
            for &index in &group.members {
                generations[index] = group.generation;
            }
        }
    }

    pub(super) fn tools<'a>(&self, index: usize, app: &'a App) -> Option<Vec<&'a ToolCallBlock>> {
        let group = &self.groups[self.by_block[index]?];
        debug_assert!(!group.members.is_empty());
        if group.members[0] != index {
            return Some(Vec::new());
        }
        Some(
            group
                .members
                .iter()
                .map(|&member| {
                    debug_assert!(member < app.blocks.len());
                    let Block::Tool(tool) = &app.blocks[member].block else {
                        unreachable!("批次索引只保存工具块");
                    };
                    tool.as_ref()
                })
                .collect(),
        )
    }

    /// 块的版式代表：批次组的全部成员都以最后一个成员的树形状为准
    /// （整组的 `more` 与 `gap` 由它代表）。
    pub(super) fn layout_owner(&self, index: usize) -> usize {
        self.by_block
            .get(index)
            .copied()
            .flatten()
            .and_then(|group| self.groups[group].members.last().copied())
            .unwrap_or(index)
    }
}

impl TreeIndex {
    pub(super) const MAX_DEPTH: u8 = 32;
    pub(super) const EMPTY: TreeLayout = TreeLayout {
        shape: None,
        gap: true,
    };

    pub(super) fn sync(&mut self, blocks: &[Entry]) {
        // 调用 id → 块下标：父链解析与父块 running 读取共用。
        let mut index_of = std::collections::HashMap::new();
        for (index, entry) in blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                index_of.insert(tool.id.as_str(), index);
            }
        }
        // 逆序：「已在后面出现过子块的 parent_id」集合，标出后面还有兄弟的子块；
        // 同时记下每个块后面是否已有子块（父块不留尾随空行）。
        let mut seen = std::collections::HashSet::new();
        let mut later_sibling = vec![false; blocks.len()];
        let mut has_children = vec![false; blocks.len()];
        for (index, entry) in blocks.iter().enumerate().rev() {
            let Block::Tool(tool) = &entry.block else {
                continue;
            };
            has_children[index] = seen.contains(tool.id.as_str());
            let Some(parent) = tool.parent_id.as_deref() else {
                continue;
            };
            later_sibling[index] = seen.contains(parent);
            seen.insert(parent);
        }
        // 正序：深度沿父链 +1，祖先的 `more` 标志压进位掩码；
        // 父块缺席（或排在后面）时按深度 1 处理。
        self.layouts.clear();
        self.layouts.resize(blocks.len(), Self::EMPTY);
        for (index, entry) in blocks.iter().enumerate() {
            let Block::Tool(tool) = &entry.block else {
                continue;
            };
            let shape = tool.parent_id.as_deref().map(|parent_id| {
                let parent = index_of.get(parent_id).copied().filter(|&p| p < index);
                let (depth, rails) =
                    parent
                        .and_then(|p| self.layouts[p].shape)
                        .map_or((1, 0), |shape| {
                            let depth = shape.depth.saturating_add(1).min(Self::MAX_DEPTH);
                            let rails = shape.rails | u32::from(shape.more) << (depth - 2);
                            (depth, rails)
                        });
                let parent_running = parent
                    .is_some_and(|p| matches!(&blocks[p].block, Block::Tool(tool) if tool.running));
                TreeShape {
                    depth,
                    rails,
                    more: later_sibling[index] || parent_running,
                }
            });
            let gap =
                !has_children[index] && shape.is_none_or(|shape| !shape.more && shape.rails == 0);
            self.layouts[index] = TreeLayout { shape, gap };
        }
    }

    pub(super) fn layout(&self, index: usize) -> TreeLayout {
        self.layouts.get(index).copied().unwrap_or(Self::EMPTY)
    }
}

/// 块用于版式判定的树布局：批次组取最后一个成员的形状，其余块取自身。
pub(super) fn block_layout(batches: &BatchIndex, tree: &TreeIndex, index: usize) -> TreeLayout {
    tree.layout(batches.layout_owner(index))
}
