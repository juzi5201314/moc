//! Hashline 锚点：稳定令牌、served 范围校验与原子编辑。

mod anchor;
pub mod apply;
pub mod served;
pub mod store;

pub use anchor::{
    AnchorParse, AnchorToken, ID_TOTAL, anchor_line_token, anchor_token, id_from_index, id_index,
    id_of, id_str, parse_anchor, token_key,
};
pub use store::{AnchorStore, FileSnapshot, LineEnding, ServedFile, SnapshotId};
