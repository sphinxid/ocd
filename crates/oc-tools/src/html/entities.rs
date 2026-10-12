//! Entity decoding helpers ported from `entities@4.5.0` (`lib/decode.js`,
//! `lib/decode_codepoint.js`), as used by the htmlparser2 8.0.2 tokenizer.

pub(super) use super::entities_data::HTML_DECODE_TREE;

pub(super) const VALUE_LENGTH: u16 = 49152;
const BRANCH_LENGTH: u16 = 16256;
const JUMP_TABLE: u16 = 127;

/// `determineBranch(decodeTree, current, nodeIdx, char)`.
pub(super) fn determine_branch(tree: &[u16], current: u16, node_idx: i64, ch: u16) -> i64 {
    let branch_count = ((current & BRANCH_LENGTH) >> 7) as i64;
    let jump_offset = current & JUMP_TABLE;
    // Case 1: single branch encoded in jump offset.
    if branch_count == 0 {
        return if jump_offset != 0 && ch == jump_offset { node_idx } else { -1 };
    }
    // Case 2: multiple branches encoded in jump table.
    if jump_offset != 0 {
        let value = ch as i64 - jump_offset as i64;
        return if value < 0 || value >= branch_count {
            -1
        } else {
            tree_at(tree, node_idx + value) as i64 - 1
        };
    }
    // Case 3: binary search in a dictionary.
    let mut lo = node_idx;
    let mut hi = lo + branch_count - 1;
    while lo <= hi {
        let mid = ((lo + hi) as u64 >> 1) as i64;
        let mid_val = tree_at(tree, mid);
        if mid_val < ch {
            lo = mid + 1;
        } else if mid_val > ch {
            hi = mid - 1;
        } else {
            return tree_at(tree, mid + branch_count) as i64;
        }
    }
    -1
}

/// Typed-array read: out-of-range reads yield `undefined`, which behaves like 0
/// in the bitwise/arithmetic contexts the tokenizer uses.
pub(super) fn tree_at(tree: &[u16], index: i64) -> u16 {
    if index < 0 { 0 } else { tree.get(index as usize).copied().unwrap_or(0) }
}

/// `replaceCodePoint`.
pub(super) fn replace_code_point(code_point: u64) -> u32 {
    if (0xd800..=0xdfff).contains(&code_point) || code_point > 0x10ffff {
        return 0xfffd;
    }
    match code_point {
        0 => 65533,
        128 => 8364,
        130 => 8218,
        131 => 402,
        132 => 8222,
        133 => 8230,
        134 => 8224,
        135 => 8225,
        136 => 710,
        137 => 8240,
        138 => 352,
        139 => 8249,
        140 => 338,
        142 => 381,
        145 => 8216,
        146 => 8217,
        147 => 8220,
        148 => 8221,
        149 => 8226,
        150 => 8211,
        151 => 8212,
        152 => 732,
        153 => 8482,
        154 => 353,
        155 => 8250,
        156 => 339,
        158 => 382,
        159 => 376,
        other => other as u32,
    }
}
