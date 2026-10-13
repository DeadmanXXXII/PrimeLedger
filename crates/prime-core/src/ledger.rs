//! Ledger state and the rules every record must satisfy.
//!
//! The same `apply` path is used when writing new records and when replaying a file,
//! so a record that cannot be written cannot be replayed either.
use crate::codec::{CodecError, Decoder, Encoder};
use crate::hash::{sha256, Hash};
use crate::record::*;
use crate::rules::{RuleError, RuleSet};
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
    UnknownHold(u64),
    HoldNotPending(u64),
    BadRulesVersion { expected: u32, got: u32 },
    RulesTooLong,
    Rule(RuleError),
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
            UnknownHold(s) => write!(f, "record #{s} is not a hold"),
            HoldNotPending(s) => write!(f, "hold #{s} was already posted or voided"),
            BadRulesVersion { expected, got } => {
                write!(f, "rules version must be {expected}, got {got}")
            }
            RulesTooLong => write!(f, "rules longer than {MAX_RULES_LEN} bytes"),
            Rule(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

struct AccountState {
    currency: Currency,
    floor: Option<i64>,
    balance: i64,
    /// Sum of outflows promised by pending holds (always >= 0). Not yet in `balance`.
    reserved: i64,
}

struct TxState {
    postings: Vec<Posting>,
    is_reversal: bool,
    reversed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoldStatus {
    Pending,
    Posted,
    Voided,
}

struct HoldState {
    /// Kept only while pending; emptied once posted or voided.
    postings: Vec<Posting>,
    status: HoldStatus,
}

enum Plan {
    Open,
    Tx { balances: Vec<(String, i64)> },
    Hold { reserved: Vec<(String, i64)> },
    Settle { balances: Vec<(String, i64)>, reserved: Vec<(String, i64)> },
    Void { reserved: Vec<(String, i64)> },
    Rules(RuleSet),
}

pub struct Ledger {
    accounts: BTreeMap<String, AccountState>,
    keys: HashMap<String, u64>,
    txs: BTreeMap<u64, TxState>,
    holds: BTreeMap<u64, HoldState>,
    rules: RuleSet,
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

fn net_deltas(postings: &[Posting]) -> Result<BTreeMap<String, i64>, LedgerError> {
    let mut m: BTreeMap<String, i64> = BTreeMap::new();
    for p in postings {
        let d = m.entry(p.account.clone()).or_insert(0);
        *d = d.checked_add(p.amount).ok_or(LedgerError::Overflow)?;
    }
    Ok(m)
}

impl Ledger {
    pub fn new() -> Self {
        Ledger {
            accounts: BTreeMap::new(),
            keys: HashMap::new(),
            txs: BTreeMap::new(),
            holds: BTreeMap::new(),
            rules: RuleSet::default(),
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
    /// Balance minus funds reserved by pending holds.
    pub fn available(&self, name: &str) -> Option<i64> {
        self.accounts
            .get(name)
            .and_then(|a| a.balance.checked_sub(a.reserved))
    }
    /// (name, currency, balance), sorted by name. Pending holds are not included.
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
    /// Holds that are still awaiting a `Post` or `Void`, in sequence order.
    pub fn pending_holds(&self) -> Vec<(u64, &[Posting])> {
        self.holds
            .iter()
            .filter(|(_, h)| h.status == HoldStatus::Pending)
            .map(|(s, h)| (*s, h.postings.as_slice()))
            .collect()
    }
    /// The rule set currently in force (version 0, no rules, until the first `SetRules`).
    pub fn rules(&self) -> &RuleSet {
        &self.rules
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
                        reserved: 0,
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
            (
                RecordKind::Hold {
                    idempotency_key,
                    postings,
                    ..
                },
                Plan::Hold { reserved },
            ) => {
                self.set_reserved(reserved);
                self.keys.insert(idempotency_key.clone(), rec.seq);
                self.holds.insert(
                    rec.seq,
                    HoldState {
                        postings: postings.clone(),
                        status: HoldStatus::Pending,
                    },
                );
            }
            (RecordKind::Post { hold_seq }, Plan::Settle { balances, reserved }) => {
                for (name, bal) in balances {
                    if let Some(st) = self.accounts.get_mut(&name) {
                        st.balance = bal;
                    }
                }
                self.set_reserved(reserved);
                if let Some(h) = self.holds.get_mut(hold_seq) {
                    let postings = std::mem::take(&mut h.postings);
                    h.status = HoldStatus::Posted;
                    // A posted hold is a real transaction from here on, so it can be reversed.
                    self.txs.insert(
                        rec.seq,
                        TxState {
                            postings,
                            is_reversal: false,
                            reversed: false,
                        },
                    );
                }
            }
            (RecordKind::Void { hold_seq }, Plan::Void { reserved }) => {
                self.set_reserved(reserved);
                if let Some(h) = self.holds.get_mut(hold_seq) {
                    h.postings.clear();
                    h.status = HoldStatus::Voided;
                }
            }
            (RecordKind::SetRules { .. }, Plan::Rules(rs)) => {
                self.rules = rs;
            }
            _ => {}
        }
        self.last_seq = rec.seq;
        self.last_hash = rec.hash();
        self.last_ms = rec.recorded_ms;
        Ok(())
    }

    fn set_reserved(&mut self, reserved: Vec<(String, i64)>) {
        for (name, r) in reserved {
            if let Some(st) = self.accounts.get_mut(&name) {
                st.reserved = r;
            }
        }
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
            RecordKind::Hold {
                idempotency_key,
                memo,
                postings,
                ..
            } => self.plan_hold(idempotency_key, memo, postings),
            RecordKind::Post { hold_seq } => {
                let (balances, reserved) = self.plan_resolve(*hold_seq, true)?;
                Ok(Plan::Settle { balances, reserved })
            }
            RecordKind::Void { hold_seq } => {
                let (_, reserved) = self.plan_resolve(*hold_seq, false)?;
                Ok(Plan::Void { reserved })
            }
            RecordKind::SetRules { version, source } => {
                if source.len() > MAX_RULES_LEN {
                    return Err(LedgerError::RulesTooLong);
                }
                let expected = self
                    .rules
                    .version
                    .checked_add(1)
                    .ok_or(LedgerError::Overflow)?;
                if *version != expected {
                    return Err(LedgerError::BadRulesVersion {
                        expected,
                        got: *version,
                    });
                }
                let rs = RuleSet::parse(*version, source).map_err(LedgerError::Rule)?;
                Ok(Plan::Rules(rs))
            }
        }
    }

    /// Checks shared by transactions and holds: sizes, idempotency, known accounts, non-zero
    /// amounts, and zero-sum per currency. Returns the net change per account.
    fn check_postings(
        &self,
        key: &str,
        memo: &str,
        postings: &[Posting],
    ) -> Result<BTreeMap<String, i64>, LedgerError> {
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

        let mut sums: BTreeMap<Currency, i64> = BTreeMap::new();
        for p in postings {
            if p.amount == 0 {
                return Err(LedgerError::ZeroAmount(p.account.clone()));
            }
            let acct = self
                .accounts
                .get(p.account.as_str())
                .ok_or_else(|| LedgerError::UnknownAccount(p.account.clone()))?;
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
        net_deltas(postings)
    }

    fn check_rules(&self, memo: &str, postings: &[Posting]) -> Result<(), LedgerError> {
        self.rules
            .check(memo, postings, &|a: &str| {
                self.accounts.get(a).map(|s| s.currency)
            })
            .map_err(LedgerError::Rule)
    }

    fn plan_tx(
        &self,
        key: &str,
        memo: &str,
        reverses: Option<u64>,
        postings: &[Posting],
    ) -> Result<Plan, LedgerError> {
        let deltas = self.check_postings(key, memo, postings)?;

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
        } else {
            // Reversals bypass rules so mistakes can always be corrected.
            self.check_rules(memo, postings)?;
        }

        let mut balances = Vec::with_capacity(deltas.len());
        for (name, delta) in &deltas {
            let st = self
                .accounts
                .get(name.as_str())
                .ok_or_else(|| LedgerError::UnknownAccount(name.clone()))?;
            let new_bal = st.balance.checked_add(*delta).ok_or(LedgerError::Overflow)?;
            // Reversals must always be possible, so they bypass floors.
            if reverses.is_none() && *delta < 0 {
                if let Some(floor) = st.floor {
                    // Funds reserved by pending holds are not spendable.
                    let avail = new_bal.checked_sub(st.reserved).ok_or(LedgerError::Overflow)?;
                    if avail < floor {
                        return Err(LedgerError::BelowFloor {
                            account: name.clone(),
                            balance: avail,
                            floor,
                        });
                    }
                }
            }
            balances.push((name.clone(), new_bal));
        }
        Ok(Plan::Tx { balances })
    }

    fn plan_hold(&self, key: &str, memo: &str, postings: &[Posting]) -> Result<Plan, LedgerError> {
        let deltas = self.check_postings(key, memo, postings)?;
        self.check_rules(memo, postings)?;
        let mut reserved = Vec::new();
        for (name, delta) in &deltas {
            if *delta >= 0 {
                continue;
            }
            let st = self
                .accounts
                .get(name.as_str())
                .ok_or_else(|| LedgerError::UnknownAccount(name.clone()))?;
            let out = delta.checked_neg().ok_or(LedgerError::Overflow)?;
            let new_res = st.reserved.checked_add(out).ok_or(LedgerError::Overflow)?;
            if let Some(floor) = st.floor {
                let avail = st.balance.checked_sub(new_res).ok_or(LedgerError::Overflow)?;
                if avail < floor {
                    return Err(LedgerError::BelowFloor {
                        account: name.clone(),
                        balance: avail,
                        floor,
                    });
                }
            }
            reserved.push((name.clone(), new_res));
        }
        Ok(Plan::Hold { reserved })
    }

    /// Settling or voiding a hold bypasses rules and floors: it was already authorised, and
    /// its outflows were reserved when it was placed. Returns (new balances if `post`, new
    /// reserved totals).
    #[allow(clippy::type_complexity)]
    fn plan_resolve(
        &self,
        hold_seq: u64,
        post: bool,
    ) -> Result<(Vec<(String, i64)>, Vec<(String, i64)>), LedgerError> {
        let h = self
            .holds
            .get(&hold_seq)
            .ok_or(LedgerError::UnknownHold(hold_seq))?;
        if h.status != HoldStatus::Pending {
            return Err(LedgerError::HoldNotPending(hold_seq));
        }
        let deltas = net_deltas(&h.postings)?;
        let mut balances = Vec::new();
        let mut reserved = Vec::new();
        for (name, delta) in &deltas {
            let st = self
                .accounts
                .get(name.as_str())
                .ok_or_else(|| LedgerError::UnknownAccount(name.clone()))?;
            if *delta < 0 {
                let out = delta.checked_neg().ok_or(LedgerError::Overflow)?;
                let r = st.reserved.checked_sub(out).ok_or(LedgerError::Overflow)?;
                if r < 0 {
                    return Err(LedgerError::Overflow);
                }
                reserved.push((name.clone(), r));
            }
            if post {
                let b = st.balance.checked_add(*delta).ok_or(LedgerError::Overflow)?;
                balances.push((name.clone(), b));
            }
        }
        Ok((balances, reserved))
    }
}

const SNAP_MAGIC: &[u8] = b"PRIMESNAP2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    BadMagic,
    BadChecksum,
    BadRules,
    Codec(CodecError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::BadMagic => write!(f, "not a PrimeLedger snapshot"),
            SnapshotError::BadChecksum => write!(f, "snapshot checksum mismatch"),
            SnapshotError::BadRules => write!(f, "snapshot holds unparsable rules"),
            SnapshotError::Codec(e) => write!(f, "snapshot undecodable: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<CodecError> for SnapshotError {
    fn from(e: CodecError) -> Self {
        SnapshotError::Codec(e)
    }
}

fn encode_postings_snap(e: &mut Encoder, postings: &[Posting]) {
    e.u32(postings.len() as u32);
    for p in postings {
        e.str(&p.account);
        e.i64(p.amount);
    }
}

fn decode_postings_snap(d: &mut Decoder<'_>) -> Result<Vec<Posting>, CodecError> {
    let n = d.u32()?;
    let mut postings = Vec::new();
    for _ in 0..n {
        let account = d.string(MAX_ACCOUNT_LEN)?;
        let amount = d.i64()?;
        postings.push(Posting { account, amount });
    }
    Ok(postings)
}

/// Snapshots are a *cache* of the state at some sequence number, used to skip replaying old
/// records. They are never the source of truth: `prime-verify` ignores them, and the store only
/// uses one if its head hash matches the record at that sequence number in the log.
impl Ledger {
    /// Deterministic encoding of the full state (same state => same bytes).
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.last_seq);
        e.raw(&self.last_hash.0);
        e.i64(self.last_ms);
        e.u32(self.accounts.len() as u32);
        for (name, a) in &self.accounts {
            e.str(name);
            e.raw(&a.currency.0);
            match a.floor {
                Some(f) => {
                    e.u8(1);
                    e.i64(f);
                }
                None => e.u8(0),
            }
            e.i64(a.balance);
            e.i64(a.reserved);
        }
        let mut keys: Vec<(&String, &u64)> = self.keys.iter().collect();
        keys.sort();
        e.u32(keys.len() as u32);
        for (k, seq) in keys {
            e.str(k);
            e.u64(*seq);
        }
        e.u32(self.txs.len() as u32);
        for (seq, t) in &self.txs {
            e.u64(*seq);
            e.u8(t.is_reversal as u8);
            e.u8(t.reversed as u8);
            encode_postings_snap(&mut e, &t.postings);
        }
        e.u32(self.holds.len() as u32);
        for (seq, h) in &self.holds {
            e.u64(*seq);
            e.u8(match h.status {
                HoldStatus::Pending => 0,
                HoldStatus::Posted => 1,
                HoldStatus::Voided => 2,
            });
            encode_postings_snap(&mut e, &h.postings);
        }
        e.u32(self.rules.version);
        e.str(&self.rules.source);
        let body = e.finish();
        let sum = sha256(&[SNAP_MAGIC, body.as_slice()]);
        let mut out = Vec::with_capacity(SNAP_MAGIC.len() + body.len() + 32);
        out.extend_from_slice(SNAP_MAGIC);
        out.extend_from_slice(&body);
        out.extend_from_slice(&sum.0);
        out
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Ledger, SnapshotError> {
        let m = SNAP_MAGIC.len();
        if bytes.len() < m + 32 || &bytes[..m] != SNAP_MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let (head, trailer) = bytes.split_at(bytes.len() - 32);
        let body = &head[m..];
        let want = sha256(&[SNAP_MAGIC, body]);
        if want.0[..] != trailer[..] {
            return Err(SnapshotError::BadChecksum);
        }
        let mut d = Decoder::new(body);
        let last_seq = d.u64()?;
        let last_hash = Hash(d.array::<32>()?);
        let last_ms = d.i64()?;

        let mut accounts = BTreeMap::new();
        let n_accounts = d.u32()?;
        for _ in 0..n_accounts {
            let name = d.string(MAX_ACCOUNT_LEN)?;
            let currency = Currency(d.array::<3>()?);
            let floor = if d.flag()? { Some(d.i64()?) } else { None };
            let balance = d.i64()?;
            let reserved = d.i64()?;
            accounts.insert(
                name,
                AccountState {
                    currency,
                    floor,
                    balance,
                    reserved,
                },
            );
        }
        let mut keys = HashMap::new();
        let n_keys = d.u32()?;
        for _ in 0..n_keys {
            let k = d.string(MAX_KEY_LEN)?;
            let seq = d.u64()?;
            keys.insert(k, seq);
        }
        let mut txs = BTreeMap::new();
        let n_txs = d.u32()?;
        for _ in 0..n_txs {
            let seq = d.u64()?;
            let is_reversal = d.flag()?;
            let reversed = d.flag()?;
            let postings = decode_postings_snap(&mut d)?;
            txs.insert(
                seq,
                TxState {
                    postings,
                    is_reversal,
                    reversed,
                },
            );
        }
        let mut holds = BTreeMap::new();
        let n_holds = d.u32()?;
        for _ in 0..n_holds {
            let seq = d.u64()?;
            let status = match d.u8()? {
                0 => HoldStatus::Pending,
                1 => HoldStatus::Posted,
                2 => HoldStatus::Voided,
                v => return Err(CodecError::InvalidFlag(v).into()),
            };
            let postings = decode_postings_snap(&mut d)?;
            holds.insert(seq, HoldState { postings, status });
        }
        let rules_version = d.u32()?;
        let rules_source = d.string(MAX_RULES_LEN)?;
        let rules = RuleSet::parse(rules_version, &rules_source)
            .map_err(|_| SnapshotError::BadRules)?;
        d.finish()?;
        Ok(Ledger {
            accounts,
            keys,
            txs,
            holds,
            rules,
            last_seq,
            last_hash,
            last_ms,
        })
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

    #[test]
    fn snapshot_roundtrip_is_exact_and_deterministic() {
        let mut l = setup();
        let t = post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        post(&mut l, "k2", &[("equity", 100), ("a", -100)], Some(t.seq)).unwrap();
        post(&mut l, "k3", &[("equity", -9), ("b", 9)], None).unwrap();
        let bytes = l.snapshot_bytes();
        let mut l2 = Ledger::from_snapshot(&bytes).unwrap();
        assert_eq!(l2.snapshot_bytes(), bytes);
        assert_eq!(l2.accounts(), l.accounts());
        assert_eq!(l2.head_hash(), l.head_hash());
        assert_eq!(l2.last_seq(), l.last_seq());
        assert_eq!(l2.find_idempotent("k3"), l.find_idempotent("k3"));
        // Restored state keeps enforcing the same rules.
        assert!(matches!(
            post(&mut l2, "k4", &[("equity", 100), ("a", -100)], Some(t.seq)),
            Err(LedgerError::AlreadyReversed(_))
        ));
        let r = post(&mut l2, "k5", &[("equity", -1), ("b", 1)], None).unwrap();
        assert_eq!(r.seq, l.last_seq() + 1);
    }

    #[test]
    fn damaged_snapshot_rejected() {
        let l = setup();
        let mut bytes = l.snapshot_bytes();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        assert!(Ledger::from_snapshot(&bytes).is_err());
        assert!(Ledger::from_snapshot(b"nope").is_err());
        assert!(Ledger::from_snapshot(&[]).is_err());
    }

    fn hold(l: &mut Ledger, key: &str, ps: &[(&str, i64)]) -> Result<Record, LedgerError> {
        let kind = RecordKind::Hold {
            idempotency_key: key.into(),
            effective_ms: 0,
            memo: "m".into(),
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

    fn submit(l: &mut Ledger, kind: RecordKind) -> Result<Record, LedgerError> {
        let r = l.next_record(2000, kind);
        l.apply(&r)?;
        Ok(r)
    }

    #[test]
    fn hold_reserves_then_post_settles() {
        let mut l = setup();
        post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        let h = hold(&mut l, "h1", &[("a", -70), ("b", 70)]).unwrap();
        // Balances are untouched, but the reserved funds cannot be spent elsewhere.
        assert_eq!(l.account("a"), Some((gbp(), 100)));
        assert_eq!(l.available("a"), Some(30));
        assert!(matches!(
            post(&mut l, "k2", &[("a", -40), ("b", 40)], None),
            Err(LedgerError::BelowFloor { .. })
        ));
        assert!(matches!(
            hold(&mut l, "h2", &[("a", -40), ("b", 40)]),
            Err(LedgerError::BelowFloor { .. })
        ));
        assert_eq!(l.pending_holds().len(), 1);
        let p = submit(&mut l, RecordKind::Post { hold_seq: h.seq }).unwrap();
        assert_eq!(l.account("a"), Some((gbp(), 30)));
        assert_eq!(l.account("b"), Some((gbp(), 70)));
        assert_eq!(l.available("a"), Some(30));
        assert!(l.pending_holds().is_empty());
        // Resolved exactly once.
        assert!(matches!(
            submit(&mut l, RecordKind::Post { hold_seq: h.seq }),
            Err(LedgerError::HoldNotPending(_))
        ));
        assert!(matches!(
            submit(&mut l, RecordKind::Void { hold_seq: h.seq }),
            Err(LedgerError::HoldNotPending(_))
        ));
        // A posted hold is an ordinary transaction and can be reversed.
        post(&mut l, "r", &[("a", 70), ("b", -70)], Some(p.seq)).unwrap();
        assert_eq!(l.account("a"), Some((gbp(), 100)));
        let total: i64 = l.accounts().iter().map(|(_, _, b)| *b).sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn void_releases_reservation() {
        let mut l = setup();
        post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        let h = hold(&mut l, "h1", &[("a", -100), ("b", 100)]).unwrap();
        assert_eq!(l.available("a"), Some(0));
        submit(&mut l, RecordKind::Void { hold_seq: h.seq }).unwrap();
        assert_eq!(l.available("a"), Some(100));
        assert_eq!(l.account("b"), Some((gbp(), 0)));
        post(&mut l, "k2", &[("a", -100), ("b", 100)], None).unwrap();
        assert!(matches!(
            submit(&mut l, RecordKind::Post { hold_seq: 999 }),
            Err(LedgerError::UnknownHold(999))
        ));
        // Hold idempotency keys share the namespace with transactions.
        assert!(matches!(
            hold(&mut l, "k2", &[("a", -1), ("b", 1)]),
            Err(LedgerError::DuplicateIdempotencyKey { .. })
        ));
    }

    #[test]
    fn rules_versioned_enforced_and_bypassed_for_reversals() {
        let mut l = setup();
        let t = post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        // Version must advance by exactly one.
        assert!(matches!(
            submit(&mut l, RecordKind::SetRules { version: 2, source: "require-memo\n".into() }),
            Err(LedgerError::BadRulesVersion { expected: 1, got: 2 })
        ));
        assert!(matches!(
            submit(&mut l, RecordKind::SetRules { version: 1, source: "nonsense\n".into() }),
            Err(LedgerError::Rule(_))
        ));
        submit(
            &mut l,
            RecordKind::SetRules { version: 1, source: "require-memo\nmax-amount GBP 50\n".into() },
        )
        .unwrap();
        assert_eq!(l.rules().version, 1);
        // The test helper posts with an empty memo, so require-memo rejects it.
        assert!(matches!(
            post(&mut l, "k2", &[("a", -10), ("b", 10)], None),
            Err(LedgerError::Rule(_))
        ));
        // Holds are checked too (this one has a memo but breaks max-amount).
        assert!(matches!(
            hold(&mut l, "h1", &[("a", -60), ("b", 60)]),
            Err(LedgerError::Rule(_))
        ));
        let h = hold(&mut l, "h2", &[("a", -10), ("b", 10)]).unwrap();
        // Reversals and settling a hold bypass rules.
        post(&mut l, "rev", &[("equity", 100), ("a", -100)], Some(t.seq)).unwrap();
        submit(&mut l, RecordKind::Post { hold_seq: h.seq }).unwrap();
        assert_eq!(l.account("a"), Some((gbp(), -10)));
        submit(
            &mut l,
            RecordKind::SetRules { version: 2, source: String::new() },
        )
        .unwrap();
        post(&mut l, "k3", &[("b", -10), ("a", 10)], None).unwrap();
    }

    #[test]
    fn snapshot_roundtrip_with_holds_and_rules() {
        let mut l = setup();
        post(&mut l, "k1", &[("equity", -100), ("a", 100)], None).unwrap();
        submit(
            &mut l,
            RecordKind::SetRules { version: 1, source: "max-postings 4\n".into() },
        )
        .unwrap();
        let h1 = hold(&mut l, "h1", &[("a", -30), ("b", 30)]).unwrap();
        let h2 = hold(&mut l, "h2", &[("a", -20), ("b", 20)]).unwrap();
        submit(&mut l, RecordKind::Post { hold_seq: h1.seq }).unwrap();
        let bytes = l.snapshot_bytes();
        let mut l2 = Ledger::from_snapshot(&bytes).unwrap();
        assert_eq!(l2.snapshot_bytes(), bytes);
        assert_eq!(l2.rules().version, 1);
        assert_eq!(l2.available("a"), Some(50));
        assert_eq!(l2.pending_holds().len(), 1);
        submit(&mut l2, RecordKind::Void { hold_seq: h2.seq }).unwrap();
        assert_eq!(l2.available("a"), Some(70));
    }
}
