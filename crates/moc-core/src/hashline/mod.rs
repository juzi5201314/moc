//! Hashline锚点：稳定分配、served 范围校验与原子编辑。

mod anchor;
pub mod apply;
pub mod served;
pub mod store;

pub use anchor::{
    AnchorParse, ID_ALPHABET, ID_SEED_LINE_LIMIT, ID_SPACE, REBASE_WINDOW, anchor_line, anchor_of,
    anchor_token, id_of, id_str, parse_anchor,
};
pub use store::{AnchorStore, FileSnapshot, LineEnding, ServedFile, SnapshotId};
