//! Ledger state and the rules every record must satisfy.
//!
//! The same `apply` path is used when writing new records and when replaying a file,
//! so a record that cannot be written cannot be replayed either.
use crate::hash::Hash;
use crate::record::*;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    BadSequence { expected: u64, got: u64 },
    BadPrevHash,
    TimeWentBackwards,
    InvalidAccountId(String),
    InvalidCurrency,
    InvalidFloor,
    AccountExists(String),
    UnknownAccount(String),
    InvalidKey,
    MemoTooLong,
    BadPostingCount,
    ZeroAmount(String),
    Unbalanced { currency: Currency, sum: i64 },
    Overflow,
    BelowFloor { account: String, balance: i64, floor: i64 },
    DuplicateIdempotencyKey { key: String, seq: u64 },
    UnknownReversalTarget(u64),
    AlreadyReversed(u64),
    CannotReverseReversal(u64),
    ReversalMismatch,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use LedgerError::*;
        match self {
            BadSequence { expected, got } => write!(f, "bad sequence: expected {expected}, got {got}"),
            BadPrevHash => write!(f, "previous-hash link does not match (chain broken)"),
            TimeWentBackwards => write!(f, "record timestamp earlier than previous record"),
            InvalidAccountId(a) => write!(f, "invalid account id '{a}'"),
            InvalidCurrency => write!(f, "invalid currency code"),
            InvalidFloor => write!(f, "account floor must be zero or negative"),
            AccountExists(a) => write!(f, "account '{a}' already exists"),
            UnknownAccount(a) => write!(f, "unknown account '{a}'"),
            InvalidKey => write!(f, "idempotency key must be 1-{MAX_KEY_LEN} bytes"),
            MemoTooLong => write!(f, "memo longer than {MAX_MEMO_LEN} bytes"),
            BadPostingCount => write!(f, "a transaction needs 2-{MAX_POSTINGS} postings"),
            ZeroAmount(a) => write!(f, "zero-amount posting on '{a}'"),
            Unbalanced { currency, sum } => write!(f, "postings in {currency} sum to {sum}, not zero"),
            Overflow => write!(f, "arithmetic overflow"),
            BelowFloor { account, balance, floor } => {
                write!(f, "'{account}' would go to {balance}, below its floor {floor}")
            }
            DuplicateIdempotencyKey { key, seq } => {
                write!(f, "idempotency key '{key}' already used by record #{seq}")
            }
            UnknownReversalTarget(s) => write!(f, "record #{s} is not a transaction"),
            AlreadyReversed(s) => write!(f, "transaction #{s} was already reversed"),
            CannotReverseReversal(s) => write!(f, "#{s} is itself a reversal"),
            ReversalMismatch => write!(f, "postings are not the exact negation of the original"),
        }
    }
}

impl std::error::Error for LedgerError {}

struct AccountState {
    currency: Currency,
    floor: Option<i64>,
    balance: i64,
}

struct TxState {
    postings: Vec<Posting>,
    is_reversal: bool,
    reversed: bool,
}

enum Plan {
    Open,
    Tx { balances: Vec<(String, i64)> },
}

pub struct Ledger {
    accounts: BTreeMap<String, AccountState>,
    keys: HashMap<String, u64>,
    txs: BTreeMap<u64, TxState>,
    last_seq: u64,
    last_hash: Hash,
    last_ms: i64,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

fn valid_account_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ACCOUNT_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-' | b'/'))
}

impl Ledger {
    pub fn new() -> Self {
        Ledger {
            accounts: BTreeMap::new(),
            keys: HashMap::new(),
            txs: BTreeMap::new(),
            last_seq: 0,
            last_hash: Hash::ZERO,
            last_ms: i64::MIN,
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }
    pub fn head_hash(&self) -> Hash {
        self.last_hash
    }
    pub fn account(&self, name: &str) -> Option<(Currency, i64)> {
        self.accounts.get(name).map(|a| (a.currency, a.balance))
    }
    /// (name, currency, balance), sorted by name.
    pub fn accounts(&self) -> Vec<(String, Currency, i64)> {
        self.accounts
            .iter()
            .map(|(n, a)| (n.clone(), a.currency, a.balance))
            .collect()
    }
    pub fn find_idempotent(&self, key: &str) -> Option<u64> {
        self.keys.get(key).copied()
    }
    pub fn transaction_postings(&self, seq: u64) -> Option<&[Posting]> {
        self.txs.get(&seq).map(|t| t.postings.as_slice())
    }

    /// Build the next record (sequence, link and timestamp filled in). Not yet applied.
    pub fn next_record(&self, now_ms: i64, kind: RecordKind) -> Record {
        Record {
            seq: self.last_seq.saturating_add(1),
            prev_hash: self.last_hash,
            recorded_ms: now_ms.max(self.last_ms),
            kind,
        }
    }

    /// Validate without changing state.
    pub fn check(&self, rec: &Record) -> Result<(), LedgerError> {
        self.plan(rec).map(|_| ())
    }

    /// Validate and apply. Either everything happens or nothing does.
    pub fn apply(&mut self, rec: &Record) -> Result<(), LedgerError> {
        let plan = self.plan(rec)?;
        match (&rec.kind, plan) {
            (
                RecordKind::OpenAccount {
                    account,
                    currency,
                    floor,
                },
                Plan::Open,
            ) => {
                self.accounts.insert(
                    account.clone(),
                    AccountState {
                        currency: *currency,
                        floor: *floor,
                        balance: 0,
                    },
                );
            }
            (
                RecordKind::Transaction {
                    idempotency_key,
                    reverses,
                    postings,
                    ..
                },
                Plan::Tx { balances },
            ) => {
                for (name, bal) in balances {
                    if let Some(st) = self.accounts.get_mut(&name) {
                        st.balance = bal;
                    }
                }
                self.keys.insert(idempotency_key.clone(), rec.seq);
                if let Some(t) = reverses {
                    if let Some(orig) = self.txs.get_mut(t) {
                        orig.reversed = true;
                    }
                }
                self.txs.insert(
                    rec.seq,
                    TxState {
                        postings: postings.clone(),
                        is_reversal: reverses.is_some(),
                        reversed: false,
                    },
                );
            }
            _ => {}
        }
        self.last_seq = rec.seq;
        self.last_hash = rec.hash();
        self.last_ms = rec.recorded_ms;
        Ok(())
    }

    fn plan(&self, rec: &Record) -> Result<Plan, LedgerError> {
        let expected = self.last_seq.checked_add(1).ok_or(LedgerError::Overflow)?;
        if rec.seq != expected {
            return Err(LedgerError::BadSequence {
                expected,
                got: rec.seq,
            });
        }
        if rec.prev_hash != self.last_hash {
            return Err(LedgerError::BadPrevHash);
        }
        if rec.recorded_ms < self.last_ms {
            return Err(LedgerError::TimeWentBackwards);
        }
        match &rec.kind {
            RecordKind::OpenAccount {
                account,
                currency,
                floor,
            } => {
                if !valid_account_id(account) {
                    return Err(LedgerError::InvalidAccountId(account.clone()));
                }
                if !currency.0.iter().all(|b| b.is_ascii_uppercase()) {
                    return Err(LedgerError::InvalidCurrency);
                }
                if matches!(floor, Some(f) if *f > 0) {
                    return Err(LedgerError::InvalidFloor);
                }
                if self.accounts.contains_key(account) {
                    return Err(LedgerError::AccountExists(account.clone()));
                }
                Ok(Plan::Open)
            }
            RecordKind::Transaction {
                idempotency_key,
                memo,
                reverses,
                postings,
                ..
            } => self.plan_tx(idempotency_key, memo, *reverses, postings),
        }
    }

    fn plan_tx(
        &self,
        key: &str,
        memo: &str,
        reverses: Option<u64>,
        postings: &[Posting],
    ) -> Result<Plan, LedgerError> {
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Err(LedgerError::InvalidKey);
        }
        if memo.len() > MAX_MEMO_LEN {
            return Err(LedgerError::MemoTooLong);
        }
        if postings.len() < 2 || postings.len() > MAX_POSTINGS {
            return Err(LedgerError::BadPostingCount);
        }
        if let Some(&seq) = self.keys.get(key) {
            return Err(LedgerError::DuplicateIdempotencyKey {
                key: key.to_string(),
                seq,
            });
        }

        let mut deltas: BTreeMap<&str, i64> = BTreeMap::new();
        let mut sums: BTreeMap<Currency, i64> = BTreeMap::new();
        for p in postings {
            if p.amount == 0 {
                return Err(LedgerError::ZeroAmount(p.account.clone()));
            }
            let acct = self
                .accounts
                .get(p.account.as_str())
                .ok_or_else(|| LedgerError::UnknownAccount(p.account.clone()))?;
            let d = deltas.entry(p.account.as_str()).or_insert(0);
            *d = d.checked_add(p.amount).ok_or(LedgerError::Overflow)?;
            let s = sums.entry(acct.currency).or_insert(0);
            *s = s.checked_add(p.amount).ok_or(LedgerError::Overflow)?;
        }
        for (currency, sum) in &sums {
            if *sum != 0 {
                return Err(LedgerError::Unbalanced {
                    currency: *currency,
                    sum: *sum,
                });
            }
        }

        if let Some(t) = reverses {
            let orig = self
                .txs
                .get(&t)
                .ok_or(LedgerError::UnknownReversalTarget(t))?;
            if orig.is_reversal {
                return Err(LedgerError::CannotReverseReversal(t));
            }
            if orig.reversed {
                return Err(LedgerError::AlreadyReversed(t));
            }
            let mut want: Vec<(String, i64)> = Vec::with_capacity(orig.postings.len());
            for p in &orig.postings {
                let neg = p.amount.checked_neg().ok_or(LedgerError::Overflow)?;
                want.push((p.account.clone(), neg));
            }
            let mut got: Vec<(String, i64)> = postings
                .iter()
                .map(|p| (p.account.clone(), p.amount))
                .collect();
            want.sort();
            got.sort();
            if want != got {
                return Err(LedgerError::ReversalMismatch);
            }
        }

        let mut balances = Vec::with_capacity(deltas.len());
        for (name, delta) in &deltas {
            let st = self
                .accounts
                .get(*name)
                .ok_or_else(|| LedgerError::UnknownAccount(name.to_string()))?;
            let new_bal = st.balance.checked_add(*delta).ok_or(LedgerError::Overflow)?;
            // Reversals must always be possible, so they bypass floors.
            if reverses.is_none() && *delta < 0 {
                if let Some(floor) = st.floor {
                    if new_bal < floor {
                        return Err(LedgerError::BelowFloor {
                            account: name.to_string(),
                            balance: new_bal,
                            floor,
                        });
                    }
                }
            }
            balances.push((name.to_string(), new_bal));
        }
        Ok(Plan::Tx { balances })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbp() -> Currency {
        Currency::parse("GBP").unwrap()
    }

    fn open(l: &mut Ledger, name: &str, cur: Currency, floor: Option<i64>) -> Record {
        let r = l.next_record(
            1000,
            RecordKind::OpenAccount {
                account: name.into(),
                currency: cur,
                floor,
            },
        );
        l.apply(&r).unwrap();
        r
    }

    fn post(
        l: &mut Ledger,
        key: &str,
        ps: &[(&str, i64)],
        rev: Option<u64>,
    ) -> Result<Record, LedgerError> {
        let kind = RecordKind::Transaction {
            idempotency_key: key.into(),
            effective_ms: 0,
            memo: String::new(),
            reverses: rev,
            postings: ps
                .iter()
                .map(|(a, n)| Posting {
                    account: a.to_string(),
                    amount: *n,
                })
                .collect(),
        };
        let r = l.next_record(2000, kind);
        l.apply(&r)?;
        Ok(r)
    }

    fn setup() -> Ledger {
        let mut l = Ledger::new();
        open(&mut l, "equity", gbp(), None);
        open(&mut l, "a", gbp(), Some(0));
        open(&mut l, "b", gbp(), None);
        l
    }

    #[test]
    fn balanced_transfer_updates_balances() {
        let mut l = setup();
        post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        post(&mut l, "k2", &[("a", -40), ("b", 40)], None).unwrap();
        assert_eq!(l.account("a"), Some((gbp(), 60)));
        assert_eq!(l.account("b"), Some((gbp(), 40)));
        assert_eq!(l.account("equity"), Some((gbp(), -100)));
        let total: i64 = l.accounts().iter().map(|(_, _, b)| *b).sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn unbalanced_unknown_zero_and_cross_currency_rejected() {
        let mut l = setup();
        open(&mut l, "usd", Currency::parse("USD").unwrap(), None);
        assert!(matches!(
            post(&mut l, "k", &[("a", 10), ("b", -9)], None),
            Err(LedgerError::Unbalanced { .. })
        ));
        assert!(matches!(
            post(&mut l, "k", &[("a", 10), ("nope", -10)], None),
            Err(LedgerError::UnknownAccount(_))
        ));
        assert!(matches!(
            post(&mut l, "k", &[("a", 0), ("b", 0)], None),
            Err(LedgerError::ZeroAmount(_))
        ));
        // Same numbers, but across currencies: needs explicit FX accounts.
        assert!(matches!(
            post(&mut l, "k", &[("b", -10), ("usd", 10)], None),
            Err(LedgerError::Unbalanced { .. })
        ));
        // A failed attempt must not consume the idempotency key or change state.
        assert_eq!(l.last_seq(), 4);
        assert!(l.find_idempotent("k").is_none());
    }

    #[test]
    fn duplicate_idempotency_key_rejected() {
        let mut l = setup();
        let r = post(&mut l, "k1", &[("equity", -5), ("b", 5)], None).unwrap();
        let err = post(&mut l, "k1", &[("equity", -5), ("b", 5)], None).unwrap_err();
        assert_eq!(
            err,
            LedgerError::DuplicateIdempotencyKey {
                key: "k1".into(),
                seq: r.seq
            }
        );
        assert_eq!(l.account("b"), Some((gbp(), 5)));
    }

    #[test]
    fn floor_enforced() {
        let mut l = setup();
        let err = post(&mut l, "k", &[("a", -50), ("b", 50)], None).unwrap_err();
        assert!(matches!(err, LedgerError::BelowFloor { .. }));
        assert_eq!(l.account("a"), Some((gbp(), 0)));
    }

    #[test]
    fn reversal_rules() {
        let mut l = setup();
        let t = post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        assert!(matches!(
            post(&mut l, "bad", &[("equity", 99), ("a", -99)], Some(t.seq)),
            Err(LedgerError::ReversalMismatch)
        ));
        let rev = post(&mut l, "k2", &[("equity", 100), ("a", -100)], Some(t.seq)).unwrap();
        assert_eq!(l.account("a"), Some((gbp(), 0)));
        assert!(matches!(
            post(&mut l, "k3", &[("equity", 100), ("a", -100)], Some(t.seq)),
            Err(LedgerError::AlreadyReversed(_))
        ));
        assert!(matches!(
            post(&mut l, "k4", &[("equity", -100), ("a", 100)], Some(rev.seq)),
            Err(LedgerError::CannotReverseReversal(_))
        ));
    }

    #[test]
    fn broken_chain_and_sequence_rejected() {
        let mut l = setup();
        let mut r = l.next_record(
            3000,
            RecordKind::OpenAccount {
                account: "x".into(),
                currency: gbp(),
                floor: None,
            },
        );
        let good = r.clone();
        r.prev_hash = Hash([1u8; 32]);
        assert_eq!(l.apply(&r), Err(LedgerError::BadPrevHash));
        let mut skipped = good.clone();
        skipped.seq += 1;
        assert!(matches!(l.apply(&skipped), Err(LedgerError::BadSequence { .. })));
        assert!(l.apply(&good).is_ok());
    }

    #[test]
    fn invalid_ids_and_floor_rejected() {
        let mut l = Ledger::new();
        let bad = l.next_record(
            1,
            RecordKind::OpenAccount {
                account: "has space".into(),
                currency: gbp(),
                floor: None,
            },
        );
        assert!(matches!(l.apply(&bad), Err(LedgerError::InvalidAccountId(_))));
        let bad = l.next_record(
            1,
            RecordKind::OpenAccount {
                account: "ok".into(),
                currency: gbp(),
                floor: Some(5),
            },
        );
        assert_eq!(l.apply(&bad), Err(LedgerError::InvalidFloor));
    }
}
