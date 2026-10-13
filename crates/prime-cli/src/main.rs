//! `prime`: command-line front end for PrimeLedger.
#![forbid(unsafe_code)]

use prime_core::query::{self, AsOf};
use prime_core::recon::{self, StatementLine};
use prime_core::{events, fx, merkle};
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
  prime hold         <file> <idempotency-key> <memo> <ACCOUNT=AMOUNT>...
  prime settle       <file> <hold-seq>
  prime void         <file> <hold-seq>
  prime holds        <file>
  prime set-rules    <file> <rules-file>
  prime rules        <file>
  prime events       <file> [--after <seq>]
  prime reconcile    <file> <ACCOUNT> <statement.csv> [--tolerance-days <n>] [--flip]
  prime balances     <file> [--recorded-at YYYY-MM-DD] [--effective-at YYYY-MM-DD]
  prime fx           <file> <key> <memo> <from> <fx-from> <fx-to> <to> <from-amt> <to-amt>
  prime snapshot     <file>
  prime verify       <file>
  prime roots        <file>
  prime proof        <file> <seq>

Amounts are integer minor units (pence/cents). Positive = debit, negative = credit.
Each transaction's postings must sum to zero per currency.
A hold reserves outflows without moving balances; settle posts it, void releases it.
Statement CSV lines: YYYY-MM-DD,amount,reference (amount signed from the account's view).
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
        "hold" => cmd_hold(rest),
        "settle" => cmd_resolve(rest, true),
        "void" => cmd_resolve(rest, false),
        "holds" => cmd_holds(rest),
        "set-rules" => cmd_set_rules(rest),
        "rules" => cmd_rules(rest),
        "events" => cmd_events(rest),
        "reconcile" => cmd_reconcile(rest),
        "balances" => cmd_balances(rest),
        "fx" => cmd_fx(rest),
        "snapshot" => cmd_snapshot(rest),
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

fn finish(result: Result<Record, BookError>) -> Result<(), String> {
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

fn submit_tx(
    book: &mut Book,
    key: &str,
    memo: &str,
    reverses: Option<u64>,
    postings: Vec<Posting>,
) -> Result<(), String> {
    let now = now_ms();
    finish(book.submit(
        RecordKind::Transaction {
            idempotency_key: key.to_string(),
            effective_ms: now,
            memo: memo.to_string(),
            reverses,
            postings,
        },
        now,
    ))
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

fn print_rows(rows: &[(String, Currency, i64)]) {
    for (name, cur, bal) in rows {
        println!("{name:<32} {cur} {bal:>16}");
    }
}

fn bad_date(s: &str) -> String {
    format!("date '{s}' must look like YYYY-MM-DD")
}

/// Last millisecond of the given UTC day, so `--effective-at 2026-03-03` includes all of 3 March.
fn parse_date_end_ms(s: &str) -> Result<i64, String> {
    let mut it = s.split('-');
    let y: i64 = it
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| bad_date(s))?;
    let m: u32 = it
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| bad_date(s))?;
    let d: u32 = it
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| bad_date(s))?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad_date(s));
    }
    Ok((days_from_civil(y, m, d) + 1) * merkle::MS_PER_DAY - 1)
}

/// (year, month, day) to days since 1970-01-01, proleptic Gregorian.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn cmd_balances(a: &[String]) -> Result<(), String> {
    if a.is_empty() {
        return Err(usage());
    }
    let mut recorded = None;
    let mut effective = None;
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--recorded-at" => {
                let v = a.get(i + 1).ok_or("--recorded-at needs a date")?;
                recorded = Some(parse_date_end_ms(v)?);
                i += 2;
            }
            "--effective-at" => {
                let v = a.get(i + 1).ok_or("--effective-at needs a date")?;
                effective = Some(parse_date_end_ms(v)?);
                i += 2;
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    if recorded.is_none() && effective.is_none() {
        let book = open_book(&a[0])?;
        print_rows(&book.ledger().accounts());
        return Ok(());
    }
    // Historical views are computed from the verified log itself, never from a snapshot.
    prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    let data = std::fs::read(&a[0]).map_err(|e| e.to_string())?;
    let sc = prime_store::scan(&data).map_err(|e| e.to_string())?;
    let rows = query::balances_as_of(
        &sc.records,
        AsOf {
            recorded_ms: recorded,
            effective_ms: effective,
        },
    )
    .map_err(|e| e.to_string())?;
    print_rows(&rows);
    Ok(())
}

fn cmd_fx(a: &[String]) -> Result<(), String> {
    if a.len() != 9 {
        return Err(usage());
    }
    let from_amt: i64 = a[7]
        .parse()
        .map_err(|_| "from-amt must be an integer".to_string())?;
    let to_amt: i64 = a[8]
        .parse()
        .map_err(|_| "to-amt must be an integer".to_string())?;
    let postings =
        fx::exchange(&a[3], &a[4], &a[5], &a[6], from_amt, to_amt).map_err(|e| e.to_string())?;
    let mut book = open_book(&a[0])?;
    submit_tx(&mut book, &a[1], &a[2], None, postings)
}

fn cmd_snapshot(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let book = open_book(&a[0])?;
    book.write_snapshot().map_err(|e| e.to_string())?;
    println!("snapshot written at #{}", book.ledger().last_seq());
    Ok(())
}

fn cmd_verify(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let rep = prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    println!("OK: {} records, {} accounts", rep.records, rep.accounts);
    println!("head hash: {}", rep.head_hash);
    if rep.pending_holds > 0 {
        println!("pending holds: {}", rep.pending_holds);
    }
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

fn cmd_hold(a: &[String]) -> Result<(), String> {
    if a.len() < 5 {
        return Err(usage());
    }
    let postings = a[3..]
        .iter()
        .map(|s| parse_posting(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut book = open_book(&a[0])?;
    let now = now_ms();
    finish(book.submit(
        RecordKind::Hold {
            idempotency_key: a[1].clone(),
            effective_ms: now,
            memo: a[2].clone(),
            postings,
        },
        now,
    ))
}

fn cmd_resolve(a: &[String], post: bool) -> Result<(), String> {
    if a.len() != 2 {
        return Err(usage());
    }
    let hold_seq: u64 = a[1].parse().map_err(|_| "hold-seq must be a number".to_string())?;
    let mut book = open_book(&a[0])?;
    let kind = if post {
        RecordKind::Post { hold_seq }
    } else {
        RecordKind::Void { hold_seq }
    };
    finish(book.submit(kind, now_ms()))
}

fn cmd_holds(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let book = open_book(&a[0])?;
    let pending = book.ledger().pending_holds();
    if pending.is_empty() {
        println!("no pending holds");
    }
    for (seq, postings) in pending {
        println!("hold #{seq}");
        for p in postings {
            println!("  {:<32} {:>16}", p.account, p.amount);
        }
    }
    Ok(())
}

fn cmd_set_rules(a: &[String]) -> Result<(), String> {
    if a.len() != 2 {
        return Err(usage());
    }
    let source = std::fs::read_to_string(&a[1]).map_err(|e| format!("cannot read {}: {e}", a[1]))?;
    let mut book = open_book(&a[0])?;
    let version = book
        .ledger()
        .rules()
        .version
        .checked_add(1)
        .ok_or("rules version overflow")?;
    finish(book.submit(RecordKind::SetRules { version, source }, now_ms()))
}

fn cmd_rules(a: &[String]) -> Result<(), String> {
    if a.len() != 1 {
        return Err(usage());
    }
    let book = open_book(&a[0])?;
    let rs = book.ledger().rules();
    println!("rules version {} ({} rules)", rs.version, rs.rules.len());
    if !rs.source.is_empty() {
        println!("{}", rs.source.trim_end());
    }
    Ok(())
}

fn cmd_events(a: &[String]) -> Result<(), String> {
    if a.is_empty() {
        return Err(usage());
    }
    let mut after: u64 = 0;
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--after" => {
                let v = a.get(i + 1).ok_or("--after needs a sequence number")?;
                after = v.parse().map_err(|_| "--after must be a number".to_string())?;
                i += 2;
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    let data = std::fs::read(&a[0]).map_err(|e| e.to_string())?;
    let sc = prime_store::scan(&data).map_err(|e| e.to_string())?;
    for r in sc.records.iter().filter(|r| r.seq > after) {
        println!("{}", events::record_to_json(r));
    }
    Ok(())
}

fn parse_date_day(s: &str) -> Result<i64, String> {
    Ok(parse_date_end_ms(s)?.div_euclid(merkle::MS_PER_DAY))
}

fn parse_statement(text: &str, flip: bool) -> Result<Vec<StatementLine>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ',');
        let date = parts.next().unwrap_or("").trim();
        if line_no == 1 && date.eq_ignore_ascii_case("date") {
            continue;
        }
        let amount = parts
            .next()
            .ok_or_else(|| format!("statement line {line_no}: expected date,amount,reference"))?
            .trim();
        let reference = parts.next().unwrap_or("").trim().to_string();
        let day = parse_date_day(date).map_err(|e| format!("statement line {line_no}: {e}"))?;
        let mut amount: i64 = amount
            .parse()
            .map_err(|_| format!("statement line {line_no}: amount must be an integer of minor units"))?;
        if flip {
            amount = amount
                .checked_neg()
                .ok_or_else(|| format!("statement line {line_no}: amount overflow"))?;
        }
        out.push(StatementLine {
            line_no,
            day,
            amount,
            reference,
        });
    }
    Ok(out)
}

fn fmt_day(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

fn cmd_reconcile(a: &[String]) -> Result<(), String> {
    if a.len() < 3 {
        return Err(usage());
    }
    let mut tolerance: i64 = 3;
    let mut flip = false;
    let mut i = 3;
    while i < a.len() {
        match a[i].as_str() {
            "--tolerance-days" => {
                let v = a.get(i + 1).ok_or("--tolerance-days needs a number")?;
                tolerance = v.parse().map_err(|_| "--tolerance-days must be a number".to_string())?;
                if tolerance < 0 {
                    return Err("--tolerance-days must not be negative".into());
                }
                i += 2;
            }
            "--flip" => {
                flip = true;
                i += 1;
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    prime_verify::verify_file(Path::new(&a[0])).map_err(|e| format!("VERIFY FAILED: {e}"))?;
    let data = std::fs::read(&a[0]).map_err(|e| e.to_string())?;
    let sc = prime_store::scan(&data).map_err(|e| e.to_string())?;
    let known = sc.records.iter().any(|r| {
        matches!(&r.kind, RecordKind::OpenAccount { account, .. } if account == &a[1])
    });
    if !known {
        return Err(format!("unknown account '{}'", a[1]));
    }
    let text = std::fs::read_to_string(&a[2]).map_err(|e| format!("cannot read {}: {e}", a[2]))?;
    let statement = parse_statement(&text, flip)?;
    let entries = recon::account_entries(&sc.records, &a[1]);
    let r = recon::reconcile(&statement, &entries, tolerance);
    println!(
        "matched {}, statement-only {}, ledger-only {}",
        r.matched.len(),
        r.unmatched_statement.len(),
        r.unmatched_ledger.len()
    );
    for l in &r.unmatched_statement {
        println!(
            "  NOT IN LEDGER   line {:>4}  {}  {:>14}  {}",
            l.line_no,
            fmt_day(l.day),
            l.amount,
            l.reference
        );
    }
    for e in &r.unmatched_ledger {
        println!(
            "  NOT ON STATEMENT  #{:<6}  {}  {:>14}  {}",
            e.seq,
            fmt_day(e.day),
            e.amount,
            e.memo
        );
    }
    if r.unmatched_statement.is_empty() && r.unmatched_ledger.is_empty() {
        println!("RECONCILED");
    } else {
        println!("BREAKS FOUND");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, days_from_civil, parse_date_end_ms};

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn date_roundtrip_and_parsing() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 1, 1), 19_723);
        for z in [-800_000i64, -1, 0, 59, 60, 11_016, 19_723, 20_000, 800_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(parse_date_end_ms("1970-01-01").unwrap(), 86_399_999);
        assert!(parse_date_end_ms("2024-13-01").is_err());
        assert!(parse_date_end_ms("2024-01").is_err());
        assert!(parse_date_end_ms("x").is_err());
    }

    #[test]
    fn posting_parser() {
        let p = super::parse_posting("acct:x=-250").unwrap();
        assert_eq!((p.account.as_str(), p.amount), ("acct:x", -250));
        assert!(super::parse_posting("nope").is_err());
        assert!(super::parse_posting("a=1.5").is_err());
    }

    #[test]
    fn statement_parser() {
        let txt = "date,amount,reference\n2024-01-01,500,pay\n\n# note\n2024-01-02,-70,fee, with comma\n";
        let v = super::parse_statement(txt, false).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].day, v[0].amount), (19_723, 500));
        assert_eq!(v[1].reference, "fee, with comma");
        let f = super::parse_statement(txt, true).unwrap();
        assert_eq!(f[0].amount, -500);
        assert!(super::parse_statement("2024-01-01,abc,x", false).is_err());
        assert!(super::parse_statement("2024-01-01", false).is_err());
    }
}
