//! Hashline 锚点空间、规范化与稳定映射。

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use xxhash_rust::xxh32::Xxh32;

/// 三字符 base-62 锚点空间。
pub const ANCHOR_SPACE: u32 = 62 * 62 * 62;
/// 与锚点空间互质的确定性冲突探测步长。
pub const HASH_PROBE_STRIDE: u32 = 3907;
const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const BITSET_WORDS: usize = ANCHOR_SPACE.div_ceil(32) as usize;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AnchorError {
    #[error("E_ANCHOR_SPACE_EXHAUSTED")]
    Exhausted,
    #[error("E_BAD_ANCHOR")]
    BadAnchor,
    #[error("锚点索引超出固定空间")]
    InvalidIndex,
    #[error("旧行与旧锚点数量不匹配")]
    LengthMismatch,
}

/// 删除 ASCII 空白，但保留所有非 ASCII Unicode 字符。
pub fn canon(line: &str) -> String {
    if !line
        .bytes()
        .any(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return line.to_owned();
    }
    line.chars()
        .filter(|character| !matches!(character, ' ' | '\t' | '\r' | '\n'))
        .collect()
}

/// 无额外分配计算 `xxh32(canon(line))`。
pub fn canon_hash(line: &str) -> u32 {
    let bytes = line.as_bytes();
    let mut hasher = Xxh32::new(0);
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            if start < index {
                hasher.update(&bytes[start..index]);
            }
            start = index + 1;
        }
    }
    if start < bytes.len() {
        hasher.update(&bytes[start..]);
    }
    hasher.digest()
}

/// 将 canon 的 XXH32 投影到有限锚点空间。
pub fn idx_of(canon: &str) -> u32 {
    idx_of_hash(xxhash_rust::xxh32::xxh32(canon.as_bytes(), 0))
}

fn idx_of_hash(hash: u32) -> u32 {
    (hash >> 14) % ANCHOR_SPACE
}

pub fn encode_anchor(index: u32) -> Result<String, AnchorError> {
    if index >= ANCHOR_SPACE {
        return Err(AnchorError::InvalidIndex);
    }
    let high = index / 3844;
    let middle = (index / 62) % 62;
    let low = index % 62;
    Ok(String::from_utf8(vec![
        ALPHABET[high as usize],
        ALPHABET[middle as usize],
        ALPHABET[low as usize],
    ])
    .expect("base-62 alphabet is ASCII"))
}

pub fn decode_anchor(anchor: &str) -> Result<u32, AnchorError> {
    let bytes = anchor.as_bytes();
    if bytes.len() != 3 {
        return Err(AnchorError::BadAnchor);
    }
    let mut index = 0u32;
    for byte in bytes {
        let digit = ALPHABET
            .iter()
            .position(|candidate| candidate == byte)
            .ok_or(AnchorError::BadAnchor)? as u32;
        index = index * 62 + digit;
    }
    Ok(index)
}

/// 238328 个状态组成的固定 u32 位图仅在一次分配期间驻留。
pub fn assign_hashes(lines: &[String], reserved: &[u32]) -> Result<Vec<u32>, AnchorError> {
    assign_hashes_iter(lines.iter().map(String::as_str), reserved)
}

fn assign_hashes_iter<'a>(
    lines: impl ExactSizeIterator<Item = &'a str>,
    reserved: &[u32],
) -> Result<Vec<u32>, AnchorError> {
    debug_assert_eq!(BITSET_WORDS, 7448);
    let mut used = vec![0u32; BITSET_WORDS];
    for &index in reserved {
        debug_assert!(index < ANCHOR_SPACE, "reserved 锚点必须属于有限空间");
        if index >= ANCHOR_SPACE {
            return Err(AnchorError::InvalidIndex);
        }
        mark_used(&mut used, index);
    }
    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        result.push(probe(&mut used, idx_of_hash(canon_hash(line)))?);
    }
    Ok(result)
}

/// 保持内容未变行的锚点稳定；删除行的锚点在本次 epoch 保留。
pub fn map_stable_hashes(
    old_lines: &[String],
    old_hashes: &[Option<u32>],
    new_lines: &[String],
    removed: &[usize],
) -> Result<Vec<u32>, AnchorError> {
    map_stable_hashes_reserved(old_lines, old_hashes, new_lines, removed, &[])
}

pub(crate) fn map_stable_hashes_reserved(
    old_lines: &[String],
    old_hashes: &[Option<u32>],
    new_lines: &[String],
    removed: &[usize],
    retired: &[u32],
) -> Result<Vec<u32>, AnchorError> {
    if old_lines.len() != old_hashes.len() {
        return Err(AnchorError::LengthMismatch);
    }
    let removed: BTreeSet<usize> = removed.iter().copied().collect();
    let mut candidates = BTreeMap::<String, BTreeSet<usize>>::new();
    let mut reserved = Vec::with_capacity(old_hashes.len() + retired.len());
    for (position, (line, hash)) in old_lines.iter().zip(old_hashes).enumerate() {
        if let Some(hash) = hash {
            debug_assert!(*hash < ANCHOR_SPACE, "旧锚点必须属于固定空间");
            reserved.push(*hash);
            if !removed.contains(&position) {
                candidates.entry(canon(line)).or_default().insert(position);
            }
        }
    }
    reserved.extend_from_slice(retired);

    let mut result = vec![None; new_lines.len()];
    let mut used_stable = vec![0u32; BITSET_WORDS];
    for &index in retired {
        debug_assert!(index < ANCHOR_SPACE, "retired 锚点必须属于固定空间");
        if index < ANCHOR_SPACE {
            mark_used(&mut used_stable, index);
        }
    }
    for (new_position, line) in new_lines.iter().enumerate() {
        let Some(positions) = candidates.get_mut(&canon(line)) else {
            continue;
        };
        let Some(old_position) = nearest_position(positions, new_position) else {
            continue;
        };
        let Some(hash) = old_hashes[old_position] else {
            continue;
        };
        if !is_used(&used_stable, hash) {
            mark_used(&mut used_stable, hash);
            result[new_position] = Some(hash);
        }
        positions.remove(&old_position);
    }

    let fresh_positions: Vec<usize> = result
        .iter()
        .enumerate()
        .filter_map(|(position, hash)| hash.is_none().then_some(position))
        .collect();
    let fresh = assign_hashes_iter(
        fresh_positions
            .iter()
            .map(|position| new_lines[*position].as_str()),
        &reserved,
    )?;
    for (position, hash) in fresh_positions.into_iter().zip(fresh) {
        result[position] = Some(hash);
    }
    Ok(result.into_iter().map(Option::unwrap).collect())
}

fn nearest_position(candidates: &BTreeSet<usize>, position: usize) -> Option<usize> {
    let lower = candidates.range(..=position).next_back().copied();
    let upper = candidates.range(position..).next().copied();
    match (lower, upper) {
        (Some(left), Some(right)) if position.abs_diff(left) <= right.abs_diff(position) => {
            Some(left)
        }
        (Some(_), Some(right)) => Some(right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn probe(used: &mut [u32], start: u32) -> Result<u32, AnchorError> {
    debug_assert!(start < ANCHOR_SPACE);
    let mut candidate = start;
    for _ in 0..ANCHOR_SPACE {
        if !is_used(used, candidate) {
            mark_used(used, candidate);
            return Ok(candidate);
        }
        candidate = (candidate + HASH_PROBE_STRIDE) % ANCHOR_SPACE;
    }
    Err(AnchorError::Exhausted)
}

#[inline]
fn is_used(used: &[u32], index: u32) -> bool {
    debug_assert!(index < ANCHOR_SPACE);
    used[index as usize / 32] & (1 << (index % 32)) != 0
}

#[inline]
fn mark_used(used: &mut [u32], index: u32) {
    debug_assert!(index < ANCHOR_SPACE);
    used[index as usize / 32] |= 1 << (index % 32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn canonicalization_and_wire_anchor_encoding_are_stable() {
        assert_eq!(canon("  a\t中\r\n"), "a中");
        assert_eq!(canon_hash(" a\tb "), canon_hash("ab"));
        assert_eq!(encode_anchor(0).unwrap(), "AAA");
        assert_eq!(encode_anchor(ANCHOR_SPACE - 1).unwrap(), "999");
        for index in [0, 1, 61, 62, 3843, ANCHOR_SPACE - 1] {
            assert_eq!(
                decode_anchor(&encode_anchor(index).unwrap()).unwrap(),
                index
            );
        }
        assert!(decode_anchor("A?A").is_err());
    }

    #[test]
    fn allocations_are_unique_and_stable_rows_keep_their_hashes() {
        let old = lines(&["dup", "before", "dup", "after"]);
        let old_hashes = assign_hashes(&old, &[]).unwrap();
        let new = lines(&["inserted", "dup", "before", "after"]);
        let mapped = map_stable_hashes(
            &old,
            &old_hashes.iter().copied().map(Some).collect::<Vec<_>>(),
            &new,
            &[2],
        )
        .unwrap();
        assert_eq!(mapped[2], old_hashes[1]);
        assert_eq!(mapped[3], old_hashes[3]);
        assert!(!old_hashes.contains(&mapped[0]));
        assert_eq!(
            mapped.iter().copied().collect::<BTreeSet<_>>().len(),
            mapped.len()
        );
    }

    #[test]
    fn exhausted_space_returns_an_error() {
        let reserved: Vec<u32> = (0..ANCHOR_SPACE).collect();
        assert_eq!(
            assign_hashes(&lines(&["one"]), &reserved),
            Err(AnchorError::Exhausted)
        );
    }
}
