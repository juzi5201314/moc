//! Hashline：文件快照 tag、行号重映射与原子编辑。

pub mod apply;
pub mod served;
pub mod store;
mod tag;

pub use tag::{content_hash, numbered_line, parse_tag, tag_of, tag_text};
pub use store::{AnchorStore, FileSnapshot, LineEnding, ServedFile, SnapshotId, TagError};
