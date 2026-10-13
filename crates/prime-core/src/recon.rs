//! Local reconciliation: match a bank/provider statement against one ledger account.
//!
//! Statement amounts are signed minor units *from the ledger account's point of view* (positive
//! raises the account's balance); the CLI can flip the sign. Matching is greedy and deterministic:
//! statement lines are taken in order, and each takes the unmatched ledger entry with the same
//! amount and the nearest date within the tolerance (ties go to the lowest sequence number).
use crate::merkle::day_of;
use crate::record::{Posting, Record, RecordKind};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementLine {
    pub line_no: usize,
    /// Days since 1970-01-01 (UTC).
    pub day: i64,
    pub amount: i64,
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    pub seq: u64,
    pub day: i64,
    pub amount: i64,
    pub memo: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub line_no: usize,
    pub seq: u64,
    /// ledger day minus statement day
    pub day_diff: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciliation {
    pub matched: Vec<Match>,
    /// On the statement but not in the ledger.
    pub unmatched_statement: Vec<StatementLine>,
    /// In the ledger but not on the statement.
    pub unmatched_ledger: Vec<LedgerEntry>,
}

fn net_for(account: &str, postings: &[Posting]) -> Option<i64> {
    let mut sum: i64 = 0;
    let mut touched = false;
    for p in postings {
        if p.account == account {
            sum = sum.checked_add(p.amount)?;
            touched = true;
        }
    }
    if touched && sum != 0 {
        Some(sum)
    } else {
        None
    }
}

/// The settled movements on `account`, in sequence order. Holds count only once posted, dated by
/// the hold's effective time; voided and still-pending holds never touched the balance.
pub fn account_entries(records: &[Record], account: &str) -> Vec<LedgerEntry> {
    let mut holds: HashMap<u64, (i64, &str, &[Posting])> = HashMap::new();
    let mut out = Vec::new();
    for r in records {
        match &r.kind {
            RecordKind::Transaction {
                effective_ms,
                memo,
                postings,
                ..
            } => {
                if let Some(amount) = net_for(account, postings) {
                    out.push(LedgerEntry {
                        seq: r.seq,
                        day: day_of(*effective_ms),
                        amount,
                        memo: memo.clone(),
                    });
                }
            }
            RecordKind::Hold {
                effective_ms,
                memo,
                postings,
                ..
            } => {
                holds.insert(r.seq, (*effective_ms, memo.as_str(), postings.as_slice()));
            }
            RecordKind::Post { hold_seq } => {
                if let Some((eff, memo, postings)) = holds.get(hold_seq) {
                    if let Some(amount) = net_for(account, postings) {
                        out.push(LedgerEntry {
                            seq: r.seq,
                            day: day_of(*eff),
                            amount,
                            memo: (*memo).to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn reconcile(
    statement: &[StatementLine],
    entries: &[LedgerEntry],
    tolerance_days: i64,
) -> Reconciliation {
    let mut used = vec![false; entries.len()];
    let mut matched = Vec::new();
    let mut unmatched_statement = Vec::new();
    for line in statement {
        let mut best: Option<(i64, usize)> = None;
        for (i, e) in entries.iter().enumerate() {
            if used[i] || e.amount != line.amount {
                continue;
            }
            let diff = (e.day - line.day).abs();
            if diff > tolerance_days {
                continue;
            }
            let better = match best {
                None => true,
                Some((bd, bi)) => (diff, e.seq) < (bd, entries[bi].seq),
            };
            if better {
                best = Some((diff, i));
            }
        }
        match best {
            Some((_, i)) => {
                used[i] = true;
                matched.push(Match {
                    line_no: line.line_no,
                    seq: entries[i].seq,
                    day_diff: entries[i].day - line.day,
                });
            }
            None => unmatched_statement.push(line.clone()),
        }
    }
    let unmatched_ledger = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, e)| e.clone())
        .collect();
    Reconciliation {
        matched,
        unmatched_statement,
        unmatched_ledger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize, day: i64, amount: i64) -> StatementLine {
        StatementLine {
            line_no: n,
            day,
            amount,
            reference: String::new(),
        }
    }

    fn entry(seq: u64, day: i64, amount: i64) -> LedgerEntry {
        LedgerEntry {
            seq,
            day,
            amount,
            memo: String::new(),
        }
    }

    #[test]
    fn matches_by_amount_and_nearest_date() {
        let entries = vec![entry(3, 100, 500), entry(4, 102, 500), entry(5, 100, -70)];
        let stmt = vec![line(1, 101, 500), line(2, 100, 500), line(3, 100, 999)];
        let r = reconcile(&stmt, &entries, 3);
        // Line 1 (day 101): entries 3 and 4 are 1 day away; the lower seq wins the tie.
        assert_eq!(r.matched[0], Match { line_no: 1, seq: 3, day_diff: -1 });
        // Line 2 takes the remaining 500.
        assert_eq!(r.matched[1], Match { line_no: 2, seq: 4, day_diff: 2 });
        assert_eq!(r.unmatched_statement.len(), 1);
        assert_eq!(r.unmatched_statement[0].line_no, 3);
        assert_eq!(r.unmatched_ledger.len(), 1);
        assert_eq!(r.unmatched_ledger[0].seq, 5);
    }

    #[test]
    fn tolerance_is_enforced() {
        let r = reconcile(&[line(1, 10, 5)], &[entry(1, 20, 5)], 3);
        assert!(r.matched.is_empty());
        assert_eq!(r.unmatched_statement.len(), 1);
        assert_eq!(r.unmatched_ledger.len(), 1);
    }

    #[test]
    fn entries_include_posted_holds_but_not_voided_or_pending() {
        use crate::hash::Hash;
        let mk = |seq: u64, kind: RecordKind| Record {
            seq,
            prev_hash: Hash::ZERO,
            recorded_ms: 0,
            kind,
        };
        let ps = |n: i64| {
            vec![
                Posting { account: "bank".into(), amount: n },
                Posting { account: "other".into(), amount: -n },
            ]
        };
        let hold = |key: &str, n: i64| RecordKind::Hold {
            idempotency_key: key.into(),
            effective_ms: 86_400_000 * 5,
            memo: "card".into(),
            postings: ps(n),
        };
        let records = vec![
            mk(
                1,
                RecordKind::Transaction {
                    idempotency_key: "t".into(),
                    effective_ms: 0,
                    memo: "dep".into(),
                    reverses: None,
                    postings: ps(100),
                },
            ),
            mk(2, hold("h1", -30)),
            mk(3, hold("h2", -40)),
            mk(4, RecordKind::Post { hold_seq: 2 }),
            mk(5, RecordKind::Void { hold_seq: 3 }),
            mk(6, hold("h3", -50)), // still pending
        ];
        let e = account_entries(&records, "bank");
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].seq, e[0].amount, e[0].day), (1, 100, 0));
        assert_eq!((e[1].seq, e[1].amount, e[1].day), (4, -30, 5));
    }
}
