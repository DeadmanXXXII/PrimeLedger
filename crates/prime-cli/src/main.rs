//! `prime`: command-line front end for PrimeLedger.
#![forbid(unsafe_code)]

use prime_core::merkle;
use prime_core::{Currency, Hash, Posting, Record, RecordKind};
use prime_store::{Book, BookError};
use prime_core::LedgerError;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn usage() -> String {
    "PrimeLedger - infinite sequence, no rewrites, mathematically pure.

USAGE:
  prime open-account <file> <ACCOUNT> <CUR> [--floor <minor-units<=0>]
  prime post         <file> <idempotency-key> <memo> <ACCOUNT=AMOUNT>...
  prime reverse      <file> <seq> <idempotency-key>
  prime balances     <file>
  prime verify       <file>
  prime roots        <file>
  prime proof        <file> <seq>

Amounts are integer minor units (pence/cents). Positive = debit, negative = credit.
Each transaction's postings must sum to zero per currency.
Example:
  prime open-account my.pld equity GBP
  prime open-account my.pld cash GBP --floor 0
  prime post my.pld seed-1 \"opening capital\" equity=-10000 cash=10000"
        .to_string()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().ok_or_else(usage)?.as_str();
    let rest = &args[1..];
    match cmd {
        "open-account" => cmd_open(rest),
        "post" => cmd_post(rest),
        "reverse" => cmd_reverse(rest),
        "balances" => cmd_balances(rest),
        "verify" => cmd_verify(rest),
        "roots" => cmd_roots(rest),
        "proof" => cmd_proof(rest),
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(format!("unknown command '{other}'\n\n{}", usage())),
    }
}

fn open_book(path: &str) -> Result<Book, String> {
    let book = Book::open(Path::new(path)).map_err(|e| e.to_string())?;
    if book.recovered_torn_tail() {
        eprintln!("note: removed a half-written final record left by an earlier crash");
    }
    Ok(book)
}

fn cmd_open(a: &[String]) -> Result<(), String> {
    if a.len() < 3 {
        return Err(usage());
    }
    let currency = Currency::parse(&a[2]).ok_or("currency must be 3 uppercase letters, e.g. GBP")?;
    let floor = match a.get(3).map(|s| s.as_str()) {
        None => None,
        Some("--floor") => {
            let v = a.get(4).ok_or("--floor needs a value")?;
            Some(v.parse::<i64>().map_err(|e| format!("bad floor: {e}"))?)
        }
        Some(other) => return Err(format!("unexpected argument '{other}'")),
    };
    let mut book = open_book(&a[0])?;
    let rec = book
        .submit(
            RecordKind::OpenAccount {
                account: a[1].clone(),
                currency,
                floor,
            },
            now_ms(),
        )
        .map_err(|e| e.to_string())?;
    println!("#{} {}", rec.seq, rec.hash());
    Ok(())
}

fn parse_posting(s: &str) -> Result<Posting, String> {
    let (account, amount) = s
        .rsplit_once('=')
        .ok_or_else(|| format!("posting '{s}' must look like ACCOUNT=AMOUNT"))?;
    let amount: i64 = amount
        .parse()
        .map_err(|_| format!("amount in '{s}' must be an integer number of minor units"))?;
    Ok(Posting {
        account: account.to_string(),
        amount,
    })
}

fn submit_tx(
    book: &mut Book,
    key: &str,
    memo: &str,
    reverses: Option<u64>,
    postings: Vec<Posting>,
) -> Result<(), String> {
    let now = now_ms();
    let result = book.submit(
        RecordKind::Transaction {
            idempotency_key: key.to_string(),
            effective_ms: now,
            memo: memo.to_string(),
            reverses,
            postings,
        },
        now,
    );
    match result {
        Ok(rec) => {
            println!("#{} {}", rec.seq, rec.hash());
            Ok(())
        }
        // Retrying a request that already went through is safe, not an error.
        Err(BookError::Ledger(LedgerError::DuplicateIdempotencyKey { seq, .. })) => {
            println!("already applied as #{seq} (no change)");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn cmd_post(a: &[String]) -> Result<(), String> {
    if a.len() < 5 {
        return Err(usage());
    }
    let postings = a[3..]
        .iter()
        .map(|s| parse_posting(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut book = open_book(&a[0])?;
    submit_tx(&mut book, &a[1], &a[2], None, postings)
}

fn cmd_reverse(a: &[String]) -> Result<(), String> {
    if a.len() != 3 {
        return Err(usage());
    }
    let seq: u64 = a[1].parse().map_err(|_| "seq must be a number".to_string())?;
    let mut book = open_book(&a[0])?;
    let original = book
        .ledger()
        .transaction_postings(seq)
        .ok_or_else(|| format!("record #{seq} is not a transaction"))?;
    let mut postings = Vec::with_capacity(original.len());
    for p in original {
        postings.push(Posting {
            account: p.account.clone(),
            amount: p.amount.checked_neg().ok_or("amount overflow")?,
        });
    }
    submit_tx(&mut book, &a[2], &format!("reversal of #{seq}"), Some(seq), postings)
}

fn cmd_balances(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let book = open_book(&a[0])?;
    for (name, cur, bal) in book.ledger().accounts() {
        println!("{name:<32} {cur} {bal:>16}");
    }
    Ok(())
}

fn cmd_verify(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let rep = prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    println!("OK: {} records, {} accounts", rep.records, rep.accounts);
    println!("head hash: {}", rep.head_hash);
    if rep.torn_tail {
        println!("warning: file ends with a half-written record (it will be removed on next open)");
    }
    Ok(())
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (mp * 153 + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn cmd_roots(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let rep = prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    println!("# PrimeLedger daily roots - publish this anywhere you like");
    println!("# head {} records {}", rep.head_hash, rep.records);
    println!("# date count first_seq root");
    for r in &rep.day_roots {
        let (y, m, d) = civil_from_days(r.day);
        println!("{y:04}-{m:02}-{d:02} {} {} {}", r.count, r.first_seq, r.root);
    }
    Ok(())
}

fn cmd_proof(a: &[String]) -> Result<(), String> {
    if a.len() != 2 {
        return Err(usage());
    }
    let seq: u64 = a[1].parse().map_err(|_| "seq must be a number".to_string())?;
    prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    let data = std::fs::read(&a[0]).map_err(|e| e.to_string())?;
    let sc = prime_store::scan(&data).map_err(|e| e.to_string())?;
    let target = sc
        .records
        .iter()
        .find(|r| r.seq == seq)
        .ok_or("no such record")?;
    let day = merkle::day_of(target.recorded_ms);
    let day_recs: Vec<&Record> = sc
        .records
        .iter()
        .filter(|r| merkle::day_of(r.recorded_ms) == day)
        .collect();
    let hashes: Vec<Hash> = day_recs.iter().map(|r| r.hash()).collect();
    let idx = day_recs
        .iter()
        .position(|r| r.seq == seq)
        .ok_or("internal error: record missing from its day")?;
    let root = merkle::merkle_root(&hashes);
    let steps = merkle::proof(&hashes, idx).ok_or("internal error: bad index")?;
    let leaf = target.hash();
    let (y, m, d) = civil_from_days(day);
    println!("record   #{seq}");
    println!("hash     {leaf}");
    println!("day      {y:04}-{m:02}-{d:02} ({} records)", hashes.len());
    println!("root     {root}");
    println!("path     ({} steps, L/R = side of the sibling)", steps.len());
    for s in &steps {
        println!("  {} {}", if s.sibling_is_left { "L" } else { "R" }, s.sibling);
    }
    let ok = merkle::verify_proof(&leaf, &steps, &root);
    println!("self-check: {}", if ok { "OK" } else { "FAILED" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn posting_parser() {
        let p = super::parse_posting("acct:x=-250").unwrap();
        assert_eq!((p.account.as_str(), p.amount), ("acct:x", -250));
        assert!(super::parse_posting("nope").is_err());
        assert!(super::parse_posting("a=1.5").is_err());
    }
}
