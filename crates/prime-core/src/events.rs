//! The outbox: every record rendered as one line of JSON.
//!
//! The log is already an ordered, append-only sequence, so consumers need no extra queue: they
//! remember the last `seq` they handled and ask for everything after it (`prime events --after`).
//! JSON is hand-written to keep the dependency list at one crate.
use crate::record::{Posting, Record, RecordKind};

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn opt_i64(v: Option<i64>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "null".to_string(),
    }
}

fn opt_u64(v: Option<u64>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "null".to_string(),
    }
}

fn postings_json(ps: &[Posting]) -> String {
    let items: Vec<String> = ps
        .iter()
        .map(|p| format!("{{\"account\":{},\"amount\":{}}}", esc(&p.account), p.amount))
        .collect();
    format!("[{}]", items.join(","))
}

/// One JSON object (no trailing newline) describing the record.
pub fn record_to_json(r: &Record) -> String {
    let head = format!(
        "\"seq\":{},\"hash\":\"{}\",\"prev\":\"{}\",\"recorded_ms\":{}",
        r.seq,
        r.hash(),
        r.prev_hash,
        r.recorded_ms
    );
    let body = match &r.kind {
        RecordKind::OpenAccount {
            account,
            currency,
            floor,
        } => format!(
            "\"type\":\"open_account\",\"account\":{},\"currency\":\"{}\",\"floor\":{}",
            esc(account),
            currency,
            opt_i64(*floor)
        ),
        RecordKind::Transaction {
            idempotency_key,
            effective_ms,
            memo,
            reverses,
            postings,
        } => format!(
            "\"type\":\"transaction\",\"key\":{},\"effective_ms\":{},\"memo\":{},\"reverses\":{},\"postings\":{}",
            esc(idempotency_key),
            effective_ms,
            esc(memo),
            opt_u64(*reverses),
            postings_json(postings)
        ),
        RecordKind::Hold {
            idempotency_key,
            effective_ms,
            memo,
            postings,
        } => format!(
            "\"type\":\"hold\",\"key\":{},\"effective_ms\":{},\"memo\":{},\"postings\":{}",
            esc(idempotency_key),
            effective_ms,
            esc(memo),
            postings_json(postings)
        ),
        RecordKind::Post { hold_seq } => format!("\"type\":\"post\",\"hold_seq\":{hold_seq}"),
        RecordKind::Void { hold_seq } => format!("\"type\":\"void\",\"hold_seq\":{hold_seq}"),
        RecordKind::SetRules { version, source } => format!(
            "\"type\":\"set_rules\",\"version\":{},\"source\":{}",
            version,
            esc(source)
        ),
    };
    format!("{{{},{}}}", head, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::record::Currency;

    #[test]
    fn escapes_special_characters() {
        assert_eq!(esc("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn renders_transaction_and_open() {
        let r = Record {
            seq: 3,
            prev_hash: Hash::ZERO,
            recorded_ms: 9,
            kind: RecordKind::Transaction {
                idempotency_key: "k".into(),
                effective_ms: 7,
                memo: "say \"hi\"".into(),
                reverses: None,
                postings: vec![
                    Posting { account: "a".into(), amount: -5 },
                    Posting { account: "b".into(), amount: 5 },
                ],
            },
        };
        let j = record_to_json(&r);
        assert!(j.starts_with("{\"seq\":3,"));
        assert!(j.contains("\"type\":\"transaction\""));
        assert!(j.contains("\"memo\":\"say \\\"hi\\\"\""));
        assert!(j.contains("\"reverses\":null"));
        assert!(j.contains("{\"account\":\"a\",\"amount\":-5}"));
        assert!(j.ends_with('}'));

        let o = Record {
            seq: 1,
            prev_hash: Hash::ZERO,
            recorded_ms: 1,
            kind: RecordKind::OpenAccount {
                account: "x".into(),
                currency: Currency::parse("GBP").unwrap(),
                floor: Some(-5),
            },
        };
        assert!(record_to_json(&o).contains("\"floor\":-5"));
    }
}
