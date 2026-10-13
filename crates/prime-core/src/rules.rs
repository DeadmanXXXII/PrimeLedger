//! A tiny, deterministic rule language. Rules are stored *inside the chain* (as `SetRules`
//! records), so every change to the rules is itself an auditable, hash-linked entry.
//!
//! One rule per line; `#` starts a comment:
//!
//! ```text
//! max-amount GBP 500000     no single posting above 500000 minor units in that currency
//! freeze acct:suspended     no new transaction or hold may touch this account
//! require-memo              every transaction/hold needs a non-empty memo
//! max-postings 10           at most this many postings per transaction/hold
//! ```
//!
//! Rules are checks only (they never rewrite a transaction). They apply to new transactions and
//! holds; reversals and the settlement/void of an existing hold bypass them so mistakes can
//! always be corrected and an authorised hold can always be resolved.
use crate::record::{Currency, Posting, MAX_POSTINGS};
use std::fmt;

pub const MAX_RULES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    MaxAmount { currency: Currency, limit: i64 },
    Freeze(String),
    RequireMemo,
    MaxPostings(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    Parse { line: usize, msg: String },
    TooMany,
    Violation(String),
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleError::Parse { line, msg } => write!(f, "rules line {line}: {msg}"),
            RuleError::TooMany => write!(f, "too many rules (max {MAX_RULES})"),
            RuleError::Violation(m) => write!(f, "rule violated: {m}"),
        }
    }
}

impl std::error::Error for RuleError {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleSet {
    pub version: u32,
    pub source: String,
    pub rules: Vec<Rule>,
}

impl RuleSet {
    pub fn parse(version: u32, source: &str) -> Result<RuleSet, RuleError> {
        let mut rules = Vec::new();
        for (i, raw) in source.lines().enumerate() {
            let line_no = i + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let t: Vec<&str> = line.split_whitespace().collect();
            let err = |msg: &str| RuleError::Parse {
                line: line_no,
                msg: msg.to_string(),
            };
            let rule = match t[0] {
                "max-amount" => {
                    if t.len() != 3 {
                        return Err(err("usage: max-amount <CUR> <limit>"));
                    }
                    let currency = Currency::parse(t[1]).ok_or_else(|| err("bad currency code"))?;
                    let limit: i64 = t[2]
                        .parse()
                        .map_err(|_| err("limit must be an integer"))?;
                    if limit <= 0 {
                        return Err(err("limit must be positive"));
                    }
                    Rule::MaxAmount { currency, limit }
                }
                "freeze" => {
                    if t.len() != 2 {
                        return Err(err("usage: freeze <account>"));
                    }
                    Rule::Freeze(t[1].to_string())
                }
                "require-memo" => {
                    if t.len() != 1 {
                        return Err(err("usage: require-memo"));
                    }
                    Rule::RequireMemo
                }
                "max-postings" => {
                    if t.len() != 2 {
                        return Err(err("usage: max-postings <n>"));
                    }
                    let n: usize = t[1].parse().map_err(|_| err("n must be an integer"))?;
                    if !(2..=MAX_POSTINGS).contains(&n) {
                        return Err(err("n must be between 2 and the postings limit"));
                    }
                    Rule::MaxPostings(n)
                }
                other => return Err(err(&format!("unknown rule '{other}'"))),
            };
            if rules.len() >= MAX_RULES {
                return Err(RuleError::TooMany);
            }
            rules.push(rule);
        }
        Ok(RuleSet {
            version,
            source: source.to_string(),
            rules,
        })
    }

    /// Check a new transaction or hold against every rule.
    pub fn check(
        &self,
        memo: &str,
        postings: &[Posting],
        currency_of: &dyn Fn(&str) -> Option<Currency>,
    ) -> Result<(), RuleError> {
        for rule in &self.rules {
            match rule {
                Rule::MaxAmount { currency, limit } => {
                    for p in postings {
                        if currency_of(p.account.as_str()) == Some(*currency)
                            && p.amount.unsigned_abs() > *limit as u64
                        {
                            return Err(RuleError::Violation(format!(
                                "max-amount {currency} {limit}: posting of {} on '{}'",
                                p.amount, p.account
                            )));
                        }
                    }
                }
                Rule::Freeze(account) => {
                    if postings.iter().any(|p| &p.account == account) {
                        return Err(RuleError::Violation(format!(
                            "freeze: account '{account}' is frozen"
                        )));
                    }
                }
                Rule::RequireMemo => {
                    if memo.trim().is_empty() {
                        return Err(RuleError::Violation("require-memo: memo is empty".into()));
                    }
                }
                Rule::MaxPostings(n) => {
                    if postings.len() > *n {
                        return Err(RuleError::Violation(format!(
                            "max-postings {n}: got {}",
                            postings.len()
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbp() -> Currency {
        Currency::parse("GBP").unwrap()
    }

    fn p(a: &str, n: i64) -> Posting {
        Posting {
            account: a.into(),
            amount: n,
        }
    }

    #[test]
    fn parses_all_rule_kinds_with_comments() {
        let src = "# header\nmax-amount GBP 1000  # trailing\n\nfreeze acct:x\nrequire-memo\nmax-postings 4\n";
        let rs = RuleSet::parse(1, src).unwrap();
        assert_eq!(rs.rules.len(), 4);
        assert_eq!(rs.version, 1);
        assert_eq!(rs.source, src);
    }

    #[test]
    fn parse_errors_carry_line_numbers() {
        let e = RuleSet::parse(1, "require-memo\nbogus thing\n").unwrap_err();
        assert!(matches!(e, RuleError::Parse { line: 2, .. }));
        assert!(RuleSet::parse(1, "max-amount gbp 5").is_err());
        assert!(RuleSet::parse(1, "max-amount GBP 0").is_err());
        assert!(RuleSet::parse(1, "max-postings 1").is_err());
        assert!(RuleSet::parse(1, "freeze").is_err());
    }

    #[test]
    fn checks_enforce_each_rule() {
        let rs = RuleSet::parse(
            1,
            "max-amount GBP 1000\nfreeze bad\nrequire-memo\nmax-postings 2\n",
        )
        .unwrap();
        let cur = |_: &str| Some(gbp());
        let ok = [p("a", -10), p("b", 10)];
        assert!(rs.check("hello", &ok, &cur).is_ok());
        assert!(rs.check("  ", &ok, &cur).is_err());
        assert!(rs.check("m", &[p("a", -2000), p("b", 2000)], &cur).is_err());
        assert!(rs.check("m", &[p("bad", -1), p("b", 1)], &cur).is_err());
        assert!(rs
            .check("m", &[p("a", -2), p("b", 1), p("c", 1)], &cur)
            .is_err());
        // The limit is per currency: a USD posting is not limited by a GBP rule.
        let usd = |_: &str| Some(Currency::parse("USD").unwrap());
        assert!(rs.check("m", &[p("a", -2000), p("b", 2000)], &usd).is_ok());
    }
}
