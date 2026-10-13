//! Multi-currency movements through explicit FX accounts (no hidden conversions).
//!
//! An exchange of `from_amount` of one currency for `to_amount` of another is four postings:
//! each currency's pair sums to zero on its own, so the engine's per-currency invariant holds,
//! and the FX pool accounts show exactly how much of each currency the house has bought or sold.
use crate::record::Posting;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FxError {
    NonPositive,
}

impl fmt::Display for FxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FxError::NonPositive => write!(f, "exchange amounts must be greater than zero"),
        }
    }
}

impl std::error::Error for FxError {}

/// `from` (currency A) gives `from_amount`; `to` (currency B) receives `to_amount`.
/// `fx_from` is the FX pool account in currency A, `fx_to` the pool account in currency B.
pub fn exchange(
    from: &str,
    fx_from: &str,
    fx_to: &str,
    to: &str,
    from_amount: i64,
    to_amount: i64,
) -> Result<Vec<Posting>, FxError> {
    if from_amount <= 0 || to_amount <= 0 {
        return Err(FxError::NonPositive);
    }
    Ok(vec![
        Posting {
            account: from.to_string(),
            amount: -from_amount,
        },
        Posting {
            account: fx_from.to_string(),
            amount: from_amount,
        },
        Posting {
            account: fx_to.to_string(),
            amount: -to_amount,
        },
        Posting {
            account: to.to_string(),
            amount: to_amount,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::record::{Currency, RecordKind};

    #[test]
    fn exchange_is_balanced_per_currency_and_accepted_by_engine() {
        let gbp = Currency::parse("GBP").unwrap();
        let usd = Currency::parse("USD").unwrap();
        let mut l = Ledger::new();
        for (name, cur) in [
            ("alice", gbp),
            ("fx:gbp", gbp),
            ("fx:usd", usd),
            ("bob", usd),
            ("capital:gbp", gbp),
            ("capital:usd", usd),
        ] {
            let r = l.next_record(
                1,
                RecordKind::OpenAccount {
                    account: name.into(),
                    currency: cur,
                    floor: None,
                },
            );
            l.apply(&r).unwrap();
        }
        let tx = |key: &str, postings: Vec<Posting>| RecordKind::Transaction {
            idempotency_key: key.into(),
            effective_ms: 0,
            memo: String::new(),
            reverses: None,
            postings,
        };
        // Fund alice (GBP) and the USD pool.
        let fund_a = tx(
            "f1",
            vec![
                Posting { account: "capital:gbp".into(), amount: -1000 },
                Posting { account: "alice".into(), amount: 1000 },
            ],
        );
        let r = l.next_record(2, fund_a);
        l.apply(&r).unwrap();
        let fund_p = tx(
            "f2",
            vec![
                Posting { account: "capital:usd".into(), amount: -5000 },
                Posting { account: "fx:usd".into(), amount: 5000 },
            ],
        );
        let r = l.next_record(2, fund_p);
        l.apply(&r).unwrap();

        let ps = exchange("alice", "fx:gbp", "fx:usd", "bob", 1000, 1270).unwrap();
        let r = l.next_record(3, tx("x1", ps));
        l.apply(&r).unwrap();
        assert_eq!(l.account("alice").unwrap().1, 0);
        assert_eq!(l.account("fx:gbp").unwrap().1, 1000);
        assert_eq!(l.account("fx:usd").unwrap().1, 5000 - 1270);
        assert_eq!(l.account("bob").unwrap().1, 1270);
    }

    #[test]
    fn rejects_non_positive_amounts() {
        assert_eq!(exchange("a", "b", "c", "d", 0, 5), Err(FxError::NonPositive));
        assert_eq!(exchange("a", "b", "c", "d", 5, -1), Err(FxError::NonPositive));
    }
}
