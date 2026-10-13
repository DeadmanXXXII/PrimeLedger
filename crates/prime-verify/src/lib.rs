//! prime-verify: re-walks a ledger file from scratch and proves its integrity.
//!
//! Beyond replaying through the engine rules (sequence, hash links, invariants), it recomputes
//! every balance with its own independent summation and checks that each currency sums to zero.
//! It is strictly read-only: it never repairs or truncates anything.
#![forbid(unsafe_code)]

use prime_core::merkle::{day_roots, DayRoot};
use prime_core::{Currency, Hash, Ledger, LedgerError, Posting, RecordKind};
use prime_store::{scan, StoreError};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub struct Report {
    pub records: u64,
    pub head_hash: Hash,
    pub accounts: usize,
    /// Holds placed but not yet posted or voided.
    pub pending_holds: usize,
    /// A half-written final record exists (a crash happened); not an integrity failure.
    pub torn_tail: bool,
    pub day_roots: Vec<DayRoot>,
}

#[derive(Debug)]
pub enum VerifyError {
    Io(std::io::Error),
    Store(StoreError),
    Ledger { seq: u64, err: LedgerError },
    BalanceMismatch { account: String, ledger: i64, recomputed: i64 },
    NonZeroSum { currency: Currency, sum: i64 },
    Overflow,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Io(e) => write!(f, "I/O error: {e}"),
            VerifyError::Store(e) => write!(f, "{e}"),
            VerifyError::Ledger { seq, err } => write!(f, "record #{seq} invalid: {err}"),
            VerifyError::BalanceMismatch {
                account,
                ledger,
                recomputed,
            } => write!(
                f,
                "balance mismatch on '{account}': engine {ledger}, recomputed {recomputed}"
            ),
            VerifyError::NonZeroSum { currency, sum } => {
                write!(f, "{currency} balances sum to {sum}, not zero")
            }
            VerifyError::Overflow => write!(f, "arithmetic overflow while recomputing"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<StoreError> for VerifyError {
    fn from(e: StoreError) -> Self {
        VerifyError::Store(e)
    }
}

pub fn verify_file(path: &Path) -> Result<Report, VerifyError> {
    let data = std::fs::read(path).map_err(VerifyError::Io)?;
    verify_bytes(&data)
}

pub fn verify_bytes(data: &[u8]) -> Result<Report, VerifyError> {
    let sc = scan(data)?;

    let mut ledger = Ledger::new();
    for rec in &sc.records {
        ledger
            .apply(rec)
            .map_err(|err| VerifyError::Ledger { seq: rec.seq, err })?;
    }

    // Independent recomputation of balances, straight from the postings.
    let mut currency_of: BTreeMap<&str, Currency> = BTreeMap::new();
    let mut balance_of: BTreeMap<&str, i64> = BTreeMap::new();
    let mut held: HashMap<u64, &[Posting]> = HashMap::new();
    for rec in &sc.records {
        match &rec.kind {
            RecordKind::OpenAccount {
                account, currency, ..
            } => {
                currency_of.insert(account.as_str(), *currency);
            }
            RecordKind::Transaction { postings, .. } => {
                for p in postings {
                    let b = balance_of.entry(p.account.as_str()).or_insert(0);
                    *b = b.checked_add(p.amount).ok_or(VerifyError::Overflow)?;
                }
            }
            RecordKind::Hold { postings, .. } => {
                held.insert(rec.seq, postings.as_slice());
            }
            RecordKind::Post { hold_seq } => {
                if let Some(postings) = held.get(hold_seq) {
                    for p in postings.iter() {
                        let b = balance_of.entry(p.account.as_str()).or_insert(0);
                        *b = b.checked_add(p.amount).ok_or(VerifyError::Overflow)?;
                    }
                }
            }
            RecordKind::Void { .. } | RecordKind::SetRules { .. } => {}
        }
    }
    let mut sums: BTreeMap<Currency, i64> = BTreeMap::new();
    for (account, currency) in &currency_of {
        let recomputed = balance_of.get(account).copied().unwrap_or(0);
        let engine = ledger.account(account).map(|(_, b)| b);
        if engine != Some(recomputed) {
            return Err(VerifyError::BalanceMismatch {
                account: account.to_string(),
                ledger: engine.unwrap_or(i64::MIN),
                recomputed,
            });
        }
        let s = sums.entry(*currency).or_insert(0);
        *s = s.checked_add(recomputed).ok_or(VerifyError::Overflow)?;
    }
    for (currency, sum) in &sums {
        if *sum != 0 {
            return Err(VerifyError::NonZeroSum {
                currency: *currency,
                sum: *sum,
            });
        }
    }

    Ok(Report {
        records: sc.records.len() as u64,
        head_hash: ledger.head_hash(),
        accounts: currency_of.len(),
        pending_holds: ledger.pending_holds().len(),
        torn_tail: sc.torn_tail,
        day_roots: day_roots(&sc.records),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prime_core::{Posting, Record};
    use prime_store::encode_frame;

    fn file_image(records: &[Record]) -> Vec<u8> {
        let mut out = prime_store::MAGIC.to_vec();
        for r in records {
            out.extend_from_slice(&encode_frame(r).unwrap());
        }
        out
    }

    fn make_records() -> Vec<Record> {
        let mut l = Ledger::new();
        let mut out = Vec::new();
        let kinds = vec![
            RecordKind::OpenAccount {
                account: "a".into(),
                currency: Currency::parse("GBP").unwrap(),
                floor: None,
            },
            RecordKind::OpenAccount {
                account: "b".into(),
                currency: Currency::parse("GBP").unwrap(),
                floor: None,
            },
            RecordKind::Transaction {
                idempotency_key: "k".into(),
                effective_ms: 0,
                memo: String::new(),
                reverses: None,
                postings: vec![
                    Posting {
                        account: "a".into(),
                        amount: -7,
                    },
                    Posting {
                        account: "b".into(),
                        amount: 7,
                    },
                ],
            },
        ];
        for (i, k) in kinds.into_iter().enumerate() {
            let r = l.next_record(86_400_000 * (i as i64) + 5, k);
            l.apply(&r).unwrap();
            out.push(r);
        }
        out
    }

    #[test]
    fn good_file_verifies_with_one_root_per_day() {
        let recs = make_records();
        let rep = verify_bytes(&file_image(&recs)).unwrap();
        assert_eq!(rep.records, 3);
        assert_eq!(rep.accounts, 2);
        assert_eq!(rep.day_roots.len(), 3);
        assert_eq!(rep.head_hash, recs[2].hash());
    }

    #[test]
    fn rewritten_history_is_caught_even_with_valid_checksums() {
        let mut recs = make_records();
        // Attacker edits the amount in record 3 and re-frames it with a correct checksum.
        if let RecordKind::Transaction { postings, .. } = &mut recs[2].kind {
            postings[0].amount = -700;
            postings[1].amount = 700;
        }
        // Still valid on its own, but changing record 2 breaks the link into record 3.
        let mut tampered = make_records();
        if let RecordKind::OpenAccount { floor, .. } = &mut tampered[1].kind {
            *floor = Some(-1);
        }
        let err = verify_bytes(&file_image(&tampered)).unwrap_err();
        assert!(matches!(err, VerifyError::Ledger { seq: 3, .. }));
        // Editing the last record alone yields a file that is self-consistent but whose head
        // hash differs, which is what published day roots / head hashes are for.
        let rep_a = verify_bytes(&file_image(&make_records())).unwrap();
        let rep_b = verify_bytes(&file_image(&recs)).unwrap();
        assert_ne!(rep_a.head_hash, rep_b.head_hash);
        assert_ne!(rep_a.day_roots, rep_b.day_roots);
    }

    #[test]
    fn holds_and_rules_verify_and_pending_is_reported() {
        let mut l = Ledger::new();
        let gbp = Currency::parse("GBP").unwrap();
        let p = |a: &str, n: i64| Posting { account: a.into(), amount: n };
        let kinds = vec![
            RecordKind::OpenAccount { account: "a".into(), currency: gbp, floor: None },
            RecordKind::OpenAccount { account: "b".into(), currency: gbp, floor: None },
            RecordKind::SetRules { version: 1, source: "require-memo\n".into() },
            RecordKind::Hold {
                idempotency_key: "h1".into(),
                effective_ms: 0,
                memo: "m".into(),
                postings: vec![p("a", -5), p("b", 5)],
            },
            RecordKind::Hold {
                idempotency_key: "h2".into(),
                effective_ms: 0,
                memo: "m".into(),
                postings: vec![p("a", -2), p("b", 2)],
            },
            RecordKind::Post { hold_seq: 4 },
        ];
        let mut recs = Vec::new();
        for k in kinds {
            let r = l.next_record(10, k);
            l.apply(&r).unwrap();
            recs.push(r);
        }
        let rep = verify_bytes(&file_image(&recs)).unwrap();
        assert_eq!(rep.records, 6);
        assert_eq!(rep.pending_holds, 1);
    }
}
