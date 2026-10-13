//! Bitemporal balance queries over the record log.
//!
//! Two clocks are kept for every transaction:
//!   * recorded time  (`Record::recorded_ms`): when the ledger wrote it. "What did we believe?"
//!   * effective time (`effective_ms`):        when it actually happened. "What was true?"
//! A late or backdated entry has effective time earlier than its recorded time, so the two views
//! differ. Filtering on both answers "what did we believe on 3 March about 1 February?".
//! Account openings follow recorded time only.
use crate::ledger::LedgerError;
use crate::record::{Currency, Record, RecordKind};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AsOf {
    /// Only count records written at or before this instant. `None` = no limit.
    pub recorded_ms: Option<i64>,
    /// Only count transactions that took effect at or before this instant. `None` = no limit.
    pub effective_ms: Option<i64>,
}

/// (account, currency, balance) sorted by account name.
pub fn balances_as_of(
    records: &[Record],
    asof: AsOf,
) -> Result<Vec<(String, Currency, i64)>, LedgerError> {
    let mut currency_of: BTreeMap<&str, Currency> = BTreeMap::new();
    let mut balance_of: BTreeMap<&str, i64> = BTreeMap::new();
    for r in records {
        if let Some(t) = asof.recorded_ms {
            if r.recorded_ms > t {
                continue;
            }
        }
        match &r.kind {
            RecordKind::OpenAccount {
                account, currency, ..
            } => {
                currency_of.insert(account.as_str(), *currency);
                balance_of.entry(account.as_str()).or_insert(0);
            }
            RecordKind::Transaction {
                effective_ms,
                postings,
                ..
            } => {
                if let Some(t) = asof.effective_ms {
                    if *effective_ms > t {
                        continue;
                    }
                }
                for p in postings {
                    let b = balance_of.entry(p.account.as_str()).or_insert(0);
                    *b = b.checked_add(p.amount).ok_or(LedgerError::Overflow)?;
                }
            }
        }
    }
    Ok(currency_of
        .iter()
        .map(|(name, cur)| {
            (
                name.to_string(),
                *cur,
                balance_of.get(name).copied().unwrap_or(0),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::record::Posting;

    fn tx(key: &str, eff: i64, amt: i64) -> RecordKind {
        RecordKind::Transaction {
            idempotency_key: key.into(),
            effective_ms: eff,
            memo: String::new(),
            reverses: None,
            postings: vec![
                Posting { account: "a".into(), amount: -amt },
                Posting { account: "b".into(), amount: amt },
            ],
        }
    }

    fn build() -> Vec<Record> {
        let gbp = Currency::parse("GBP").unwrap();
        let mut l = Ledger::new();
        let mut out = Vec::new();
        let kinds = vec![
            (1000, RecordKind::OpenAccount { account: "a".into(), currency: gbp, floor: None }),
            (1000, RecordKind::OpenAccount { account: "b".into(), currency: gbp, floor: None }),
            (2000, tx("t1", 500, 10)),
            // Written later, but backdated: it happened at effective time 100.
            (3000, tx("t2", 100, 7)),
        ];
        for (now, k) in kinds {
            let r = l.next_record(now, k);
            l.apply(&r).unwrap();
            out.push(r);
        }
        out
    }

    fn b_of(rows: &[(String, Currency, i64)]) -> i64 {
        rows.iter().find(|r| r.0 == "b").unwrap().2
    }

    #[test]
    fn recorded_time_shows_what_we_believed() {
        let recs = build();
        let all = balances_as_of(&recs, AsOf::default()).unwrap();
        assert_eq!(b_of(&all), 17);
        let early = balances_as_of(&recs, AsOf { recorded_ms: Some(2500), effective_ms: None }).unwrap();
        assert_eq!(b_of(&early), 10);
        let none = balances_as_of(&recs, AsOf { recorded_ms: Some(500), effective_ms: None }).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn effective_time_shows_what_was_true() {
        let recs = build();
        let rows = balances_as_of(&recs, AsOf { recorded_ms: None, effective_ms: Some(300) }).unwrap();
        assert_eq!(b_of(&rows), 7); // only the backdated entry had happened by then
    }

    #[test]
    fn both_clocks_combined() {
        let recs = build();
        // Believed at recorded 2500, about effective 300: the backdated entry wasn't known yet.
        let rows = balances_as_of(
            &recs,
            AsOf { recorded_ms: Some(2500), effective_ms: Some(300) },
        )
        .unwrap();
        assert_eq!(b_of(&rows), 0);
    }
}
