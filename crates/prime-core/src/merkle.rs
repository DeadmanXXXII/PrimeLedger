//! Merkle trees over record hashes, grouped per UTC day.
//!
//! Leaf = H(0x00 || record_hash), node = H(0x01 || left || right). An odd node at the end of a
//! level is promoted unchanged (never duplicated), so different leaf sets cannot share a root.
use crate::hash::{sha256, Hash};
use crate::record::Record;

pub const MS_PER_DAY: i64 = 86_400_000;
const LEAF: &[u8] = &[0x00];
const NODE: &[u8] = &[0x01];
const EMPTY: &[u8] = b"PRIMELEDGER/merkle/empty";

fn leaf_hash(h: &Hash) -> Hash {
    sha256(&[LEAF, &h.0[..]])
}

fn node_hash(l: &Hash, r: &Hash) -> Hash {
    sha256(&[NODE, &l.0[..], &r.0[..]])
}

fn next_level(level: &[Hash]) -> Vec<Hash> {
    level
        .chunks(2)
        .map(|c| {
            if c.len() == 2 {
                node_hash(&c[0], &c[1])
            } else {
                c[0]
            }
        })
        .collect()
}

pub fn merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return sha256(&[EMPTY]);
    }
    let mut level: Vec<Hash> = leaves.iter().map(leaf_hash).collect();
    while level.len() > 1 {
        level = next_level(&level);
    }
    level[0]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProofStep {
    pub sibling: Hash,
    pub sibling_is_left: bool,
}

/// Inclusion proof for `leaves[index]`.
pub fn proof(leaves: &[Hash], index: usize) -> Option<Vec<ProofStep>> {
    if index >= leaves.len() {
        return None;
    }
    let mut idx = index;
    let mut level: Vec<Hash> = leaves.iter().map(leaf_hash).collect();
    let mut steps = Vec::new();
    while level.len() > 1 {
        let sib = idx ^ 1;
        if sib < level.len() {
            steps.push(ProofStep {
                sibling: level[sib],
                sibling_is_left: sib < idx,
            });
        }
        level = next_level(&level);
        idx /= 2;
    }
    Some(steps)
}

pub fn verify_proof(leaf: &Hash, steps: &[ProofStep], root: &Hash) -> bool {
    let mut cur = leaf_hash(leaf);
    for s in steps {
        cur = if s.sibling_is_left {
            node_hash(&s.sibling, &cur)
        } else {
            node_hash(&cur, &s.sibling)
        };
    }
    cur == *root
}

pub fn day_of(ms: i64) -> i64 {
    ms.div_euclid(MS_PER_DAY)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayRoot {
    /// Days since 1970-01-01 (UTC) of the records' `recorded_ms`.
    pub day: i64,
    pub first_seq: u64,
    pub count: usize,
    pub root: Hash,
}

/// One Merkle root per UTC day. Records must be in sequence order (timestamps non-decreasing).
pub fn day_roots(records: &[Record]) -> Vec<DayRoot> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let day = day_of(records[i].recorded_ms);
        let mut j = i;
        let mut hashes = Vec::new();
        while j < records.len() && day_of(records[j].recorded_ms) == day {
            hashes.push(records[j].hash());
            j += 1;
        }
        out.push(DayRoot {
            day,
            first_seq: records[i].seq,
            count: hashes.len(),
            root: merkle_root(&hashes),
        });
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n).map(|i| sha256(&[&(i as u64).to_le_bytes()[..]])).collect()
    }

    #[test]
    fn proofs_verify_for_every_leaf_and_size() {
        for n in 1..=17 {
            let ls = leaves(n);
            let root = merkle_root(&ls);
            for i in 0..n {
                let p = proof(&ls, i).unwrap();
                assert!(verify_proof(&ls[i], &p, &root), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn tampering_breaks_proof() {
        let ls = leaves(5);
        let root = merkle_root(&ls);
        let p = proof(&ls, 2).unwrap();
        assert!(!verify_proof(&ls[3], &p, &root));
        let other = merkle_root(&leaves(6));
        assert!(!verify_proof(&ls[2], &p, &other));
    }

    #[test]
    fn root_depends_on_order_and_content() {
        let mut ls = leaves(4);
        let r1 = merkle_root(&ls);
        ls.swap(0, 1);
        assert_ne!(r1, merkle_root(&ls));
        assert_ne!(merkle_root(&leaves(3)), merkle_root(&leaves(4)));
    }
}
