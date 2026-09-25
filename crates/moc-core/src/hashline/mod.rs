//! Hashline锚点：稳定分配、served 范围校验与原子编辑。

mod anchor;
pub mod apply;
pub mod served;
pub mod store;

pub use anchor::{
    ANCHOR_SPACE, AnchorError, HASH_PROBE_STRIDE, assign_hashes, canon, canon_hash, decode_anchor,
    encode_anchor, idx_of, map_stable_hashes,
};
pub use store::{AnchorStore, FileSnapshot, LineEnding, ServedFile, SnapshotId};
