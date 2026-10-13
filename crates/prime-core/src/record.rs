//! The ledger record: the one thing that is ever appended.
//!
//! Sign convention: positive amount = debit, negative = credit. An account's balance
//! is the sum of its posted amounts. All amounts are integer minor units (pence, cents).
use crate::codec::{CodecError, Decoder, Encoder};
use crate::hash::{sha256, Hash};
use std::fmt;

pub const MAX_ACCOUNT_LEN: usize = 64;
pub const MAX_KEY_LEN: usize = 128;
pub const MAX_MEMO_LEN: usize = 256;
pub const MAX_POSTINGS: usize = 256;

/// Domain separator mixed into every record hash.
pub const RECORD_DOMAIN: &[u8] = b"PRIMELEDGER/record/v1\0";

/// ISO-4217-style code: exactly three uppercase ASCII letters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Currency(pub [u8; 3]);

impl Currency {
    pub fn parse(s: &str) -> Option<Currency> {
        let b = s.as_bytes();
        if b.len() == 3 && b.iter().all(|c| c.is_ascii_uppercase()) {
            Some(Currency([b[0], b[1], b[2]]))
        } else {
            None
        }
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub account: String,
    pub amount: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordKind {
    /// Create an account. `floor` is the lowest permitted balance (must be <= 0).
    OpenAccount {
        account: String,
        currency: Currency,
        floor: Option<i64>,
    },
    /// A balanced set of postings. Corrections are new transactions with `reverses` set.
    Transaction {
        idempotency_key: String,
        /// When it happened (caller supplied). `recorded_ms` on the record is when we wrote it.
        effective_ms: i64,
        memo: String,
        reverses: Option<u64>,
        postings: Vec<Posting>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    pub prev_hash: Hash,
    pub recorded_ms: i64,
    pub kind: RecordKind,
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.seq);
        e.raw(&self.prev_hash.0);
        e.i64(self.recorded_ms);
        match &self.kind {
            RecordKind::OpenAccount {
                account,
                currency,
                floor,
            } => {
                e.u8(1);
                e.str(account);
                e.raw(&currency.0);
                match floor {
                    Some(f) => {
                        e.u8(1);
                        e.i64(*f);
                    }
                    None => e.u8(0),
                }
            }
            RecordKind::Transaction {
                idempotency_key,
                effective_ms,
                memo,
                reverses,
                postings,
            } => {
                e.u8(2);
                e.str(idempotency_key);
                e.i64(*effective_ms);
                e.str(memo);
                match reverses {
                    Some(s) => {
                        e.u8(1);
                        e.u64(*s);
                    }
                    None => e.u8(0),
                }
                e.u32(postings.len() as u32);
                for p in postings {
                    e.str(&p.account);
                    e.i64(p.amount);
                }
            }
        }
        e.finish()
    }

    pub fn decode(body: &[u8]) -> Result<Record, CodecError> {
        let mut d = Decoder::new(body);
        let seq = d.u64()?;
        let prev_hash = Hash(d.array::<32>()?);
        let recorded_ms = d.i64()?;
        let kind = match d.u8()? {
            1 => {
                let account = d.string(MAX_ACCOUNT_LEN)?;
                let currency = Currency(d.array::<3>()?);
                let floor = if d.flag()? { Some(d.i64()?) } else { None };
                RecordKind::OpenAccount {
                    account,
                    currency,
                    floor,
                }
            }
            2 => {
                let idempotency_key = d.string(MAX_KEY_LEN)?;
                let effective_ms = d.i64()?;
                let memo = d.string(MAX_MEMO_LEN)?;
                let reverses = if d.flag()? { Some(d.u64()?) } else { None };
                let n = d.u32()? as usize;
                if n > MAX_POSTINGS {
                    return Err(CodecError::TooLong);
                }
                let mut postings = Vec::with_capacity(n);
                for _ in 0..n {
                    let account = d.string(MAX_ACCOUNT_LEN)?;
                    let amount = d.i64()?;
                    postings.push(Posting { account, amount });
                }
                RecordKind::Transaction {
                    idempotency_key,
                    effective_ms,
                    memo,
                    reverses,
                    postings,
                }
            }
            t => return Err(CodecError::InvalidTag(t)),
        };
        d.finish()?;
        Ok(Record {
            seq,
            prev_hash,
            recorded_ms,
            kind,
        })
    }

    /// Hash of this record; the next record's `prev_hash` must equal it.
    pub fn hash(&self) -> Hash {
        let body = self.encode();
        sha256(&[RECORD_DOMAIN, body.as_slice()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tx() -> Record {
        Record {
            seq: 2,
            prev_hash: Hash([9u8; 32]),
            recorded_ms: 1_700_000_000_000,
            kind: RecordKind::Transaction {
                idempotency_key: "k1".into(),
                effective_ms: 1_699_999_999_000,
                memo: "rent".into(),
                reverses: Some(1),
                postings: vec![
                    Posting {
                        account: "a".into(),
                        amount: -500,
                    },
                    Posting {
                        account: "b".into(),
                        amount: 500,
                    },
                ],
            },
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let r = sample_tx();
        assert_eq!(Record::decode(&r.encode()).unwrap(), r);
        let o = Record {
            seq: 1,
            prev_hash: Hash::ZERO,
            recorded_ms: 5,
            kind: RecordKind::OpenAccount {
                account: "cash".into(),
                currency: Currency::parse("GBP").unwrap(),
                floor: Some(-100),
            },
        };
        assert_eq!(Record::decode(&o.encode()).unwrap(), o);
    }

    #[test]
    fn hash_changes_with_any_field() {
        let a = sample_tx();
        let mut b = sample_tx();
        b.recorded_ms += 1;
        assert_ne!(a.hash(), b.hash());
        let mut c = sample_tx();
        if let RecordKind::Transaction { postings, .. } = &mut c.kind {
            postings[0].amount = -501;
        }
        assert_ne!(a.hash(), c.hash());
    }

    #[test]
    fn decode_rejects_trailing_bytes_and_bad_tag() {
        let mut bytes = sample_tx().encode();
        bytes.push(0);
        assert_eq!(Record::decode(&bytes), Err(CodecError::TrailingBytes));
        let mut bytes = sample_tx().encode();
        bytes[8 + 32 + 8] = 99;
        assert_eq!(Record::decode(&bytes), Err(CodecError::InvalidTag(99)));
    }

    #[test]
    fn currency_parse() {
        assert!(Currency::parse("GBP").is_some());
        assert!(Currency::parse("gbp").is_none());
        assert!(Currency::parse("GBPX").is_none());
    }
}
