//! prime-store: the append-only ledger file and the `Book` that ties it to the engine.
//!
//! File layout: 8-byte magic `PRIMELDG`, then frames:
//!   u32 LE body_len | body (an encoded `Record`) | 4-byte checksum
//! where checksum = first 4 bytes of SHA-256(body_len_le || body). The checksum only detects
//! torn writes; tamper-evidence comes from the hash chain inside the records.
//! There is no API to modify or delete anything already written.
#![forbid(unsafe_code)]

use prime_core::{sha256, Ledger, LedgerError, Record, RecordKind};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"PRIMELDG";
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    BadMagic,
    Corrupt { offset: usize, reason: String },
    RecordTooLarge,
    /// A previous write failed part-way; reopen the book to recover.
    Poisoned,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "I/O error: {e}"),
            StoreError::BadMagic => write!(f, "not a PrimeLedger file (bad header)"),
            StoreError::Corrupt { offset, reason } => {
                write!(f, "corrupt ledger file at byte {offset}: {reason}")
            }
            StoreError::RecordTooLarge => write!(f, "record too large"),
            StoreError::Poisoned => write!(f, "store poisoned by an earlier write failure; reopen"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

fn corrupt(offset: usize, reason: &str) -> StoreError {
    StoreError::Corrupt {
        offset,
        reason: reason.to_string(),
    }
}

fn checksum(body: &[u8]) -> [u8; 4] {
    let len = (body.len() as u32).to_le_bytes();
    let h = sha256(&[&len[..], body]);
    [h.0[0], h.0[1], h.0[2], h.0[3]]
}

pub fn encode_frame(rec: &Record) -> Result<Vec<u8>, StoreError> {
    let body = rec.encode();
    if body.len() > MAX_FRAME {
        return Err(StoreError::RecordTooLarge);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&checksum(&body));
    Ok(out)
}

pub struct Scan {
    pub records: Vec<Record>,
    /// Length of the valid prefix (header + whole good frames).
    pub good_len: usize,
    /// True if an incomplete or damaged *final* frame was found after `good_len`.
    pub torn_tail: bool,
}

/// Parse a whole ledger file image. A damaged final frame is reported as a torn tail
/// (the normal result of a crash mid-append); damage anywhere else is an error.
pub fn scan(data: &[u8]) -> Result<Scan, StoreError> {
    if data.len() < MAGIC.len() {
        return if MAGIC.starts_with(data) {
            Ok(Scan {
                records: Vec::new(),
                good_len: 0,
                torn_tail: !data.is_empty(),
            })
        } else {
            Err(StoreError::BadMagic)
        };
    }
    if data[..MAGIC.len()] != MAGIC[..] {
        return Err(StoreError::BadMagic);
    }
    let mut pos = MAGIC.len();
    let mut records = Vec::new();
    let mut torn = false;
    while pos < data.len() {
        let rest = &data[pos..];
        if rest.len() < 4 {
            torn = true;
            break;
        }
        let flen = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if flen == 0 {
            if rest.iter().all(|b| *b == 0) {
                torn = true; // zero-filled tail after power loss
                break;
            }
            return Err(corrupt(pos, "zero-length frame"));
        }
        if flen > MAX_FRAME {
            return Err(corrupt(pos, "frame length exceeds limit"));
        }
        let total = 4 + flen + 4;
        if rest.len() < total {
            torn = true;
            break;
        }
        let body = &rest[4..4 + flen];
        let sum = &rest[4 + flen..total];
        if checksum(body).as_slice() != sum {
            if rest.len() == total {
                torn = true;
                break;
            }
            return Err(corrupt(pos, "checksum mismatch"));
        }
        let rec = Record::decode(body)
            .map_err(|e| corrupt(pos, &format!("undecodable record: {e}")))?;
        records.push(rec);
        pos += total;
    }
    Ok(Scan {
        records,
        good_len: pos,
        torn_tail: torn,
    })
}

pub struct Store {
    file: File,
    poisoned: bool,
}

impl Store {
    /// Open (creating if needed), repair a torn tail, and return the store plus what it holds.
    pub fn open(path: &Path) -> Result<(Store, Scan), StoreError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let sc = scan(&data)?;
        if sc.good_len == 0 {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(MAGIC)?;
            file.sync_all()?;
        } else if sc.torn_tail {
            file.set_len(sc.good_len as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Store {
                file,
                poisoned: false,
            },
            sc,
        ))
    }

    /// Append one record and fsync before returning.
    pub fn append(&mut self, rec: &Record) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        let frame = encode_frame(rec)?;
        let res = self
            .file
            .write_all(&frame)
            .and_then(|_| self.file.sync_all());
        if let Err(e) = res {
            self.poisoned = true;
            return Err(StoreError::Io(e));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum BookError {
    Store(StoreError),
    Ledger(LedgerError),
}

impl fmt::Display for BookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BookError::Store(e) => write!(f, "{e}"),
            BookError::Ledger(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BookError {}

impl From<StoreError> for BookError {
    fn from(e: StoreError) -> Self {
        BookError::Store(e)
    }
}
impl From<LedgerError> for BookError {
    fn from(e: LedgerError) -> Self {
        BookError::Ledger(e)
    }
}

/// Write a snapshot automatically every this many records (0 disables).
pub const SNAPSHOT_EVERY: u64 = 1000;

fn snapshot_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".snap");
    PathBuf::from(s)
}

/// An open ledger: engine state plus its file. Single writer.
pub struct Book {
    ledger: Ledger,
    store: Store,
    path: PathBuf,
    recovered_torn_tail: bool,
    snapshot_every: u64,
}

impl Book {
    /// Open or create. If a snapshot exists and its head hash matches the record at the same
    /// sequence number in the log, only the records after it are replayed; otherwise (missing,
    /// damaged, or from a different history) everything is replayed from the start.
    pub fn open(path: &Path) -> Result<Book, BookError> {
        let (store, sc) = Store::open(path)?;

        let mut start = 0usize;
        let mut restored: Option<Ledger> = None;
        if let Ok(bytes) = std::fs::read(snapshot_path(path)) {
            if let Ok(l) = Ledger::from_snapshot(&bytes) {
                let n = l.last_seq() as usize;
                if n >= 1
                    && n <= sc.records.len()
                    && sc.records[n - 1].seq == l.last_seq()
                    && sc.records[n - 1].hash() == l.head_hash()
                {
                    start = n;
                    restored = Some(l);
                }
            }
        }
        let mut ledger = restored.unwrap_or_default();
        for rec in &sc.records[start..] {
            ledger.apply(rec)?;
        }
        Ok(Book {
            ledger,
            store,
            path: path.to_path_buf(),
            recovered_torn_tail: sc.torn_tail,
            snapshot_every: SNAPSHOT_EVERY,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// True if opening found and removed a half-written final record.
    pub fn recovered_torn_tail(&self) -> bool {
        self.recovered_torn_tail
    }

    pub fn set_snapshot_interval(&mut self, every: u64) {
        self.snapshot_every = every;
    }

    /// Atomically write `<ledger file>.snap` (temp file, fsync, rename).
    pub fn write_snapshot(&self) -> Result<(), StoreError> {
        let snap = snapshot_path(&self.path);
        let mut tmp = snap.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&self.ledger.snapshot_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &snap)?;
        Ok(())
    }

    /// Validate, write durably, then apply. Returns the record that was appended.
    pub fn submit(&mut self, kind: RecordKind, now_ms: i64) -> Result<Record, BookError> {
        let rec = self.ledger.next_record(now_ms, kind);
        self.ledger.check(&rec)?;
        self.store.append(&rec)?;
        self.ledger.apply(&rec)?;
        if self.snapshot_every > 0 && rec.seq % self.snapshot_every == 0 {
            // A snapshot is only a cache; failing to write one must not fail the transaction.
            let _ = self.write_snapshot();
        }
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prime_core::{Currency, Posting};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("primeledger-{}-{}.pld", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(snapshot_path(&p));
        p
    }

    fn open_acct(name: &str) -> RecordKind {
        RecordKind::OpenAccount {
            account: name.into(),
            currency: Currency::parse("GBP").unwrap(),
            floor: None,
        }
    }

    fn transfer(key: &str, from: &str, to: &str, amt: i64) -> RecordKind {
        RecordKind::Transaction {
            idempotency_key: key.into(),
            effective_ms: 0,
            memo: "t".into(),
            reverses: None,
            postings: vec![
                Posting {
                    account: from.into(),
                    amount: -amt,
                },
                Posting {
                    account: to.into(),
                    amount: amt,
                },
            ],
        }
    }

    fn build(path: &Path) {
        let mut b = Book::open(path).unwrap();
        b.submit(open_acct("a"), 1000).unwrap();
        b.submit(open_acct("b"), 1001).unwrap();
        b.submit(transfer("k1", "a", "b", 250), 1002).unwrap();
        b.submit(transfer("k2", "b", "a", 50), 1003).unwrap();
    }

    #[test]
    fn persists_and_replays() {
        let p = temp_path("replay");
        build(&p);
        let b = Book::open(&p).unwrap();
        assert_eq!(b.ledger().last_seq(), 4);
        assert_eq!(b.ledger().account("b").unwrap().1, 200);
        assert_eq!(b.ledger().account("a").unwrap().1, -200);
        assert!(!b.recovered_torn_tail());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_tail_is_repaired_and_appends_continue() {
        let p = temp_path("torn");
        build(&p);
        let full = std::fs::read(&p).unwrap();
        for cut in [1usize, 3, 5, 12] {
            std::fs::write(&p, &full[..full.len() - cut]).unwrap();
            let mut b = Book::open(&p).unwrap();
            assert!(b.recovered_torn_tail(), "cut={cut}");
            assert_eq!(b.ledger().last_seq(), 3, "cut={cut}");
            b.submit(transfer("k2", "b", "a", 50), 2000).unwrap();
            drop(b);
            let b = Book::open(&p).unwrap();
            assert_eq!(b.ledger().last_seq(), 4);
            assert!(!b.recovered_torn_tail());
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn mid_file_corruption_is_an_error_not_silently_dropped() {
        let p = temp_path("corrupt");
        build(&p);
        let mut data = std::fs::read(&p).unwrap();
        data[MAGIC.len() + 10] ^= 0xff; // inside the first frame
        std::fs::write(&p, &data).unwrap();
        assert!(Book::open(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rejected_submit_writes_nothing() {
        let p = temp_path("reject");
        let mut b = Book::open(&p).unwrap();
        b.submit(open_acct("a"), 1).unwrap();
        let before = std::fs::metadata(&p).unwrap().len();
        assert!(b.submit(transfer("k", "a", "ghost", 5), 2).is_err());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), before);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(matches!(scan(b"NOTALEDGERFILE!"), Err(StoreError::BadMagic)));
    }

    #[test]
    fn snapshot_fast_path_matches_full_replay() {
        let p = temp_path("snapfast");
        build(&p);
        let mut b = Book::open(&p).unwrap();
        b.write_snapshot().unwrap();
        b.submit(transfer("k3", "a", "b", 10), 3000).unwrap();
        drop(b);

        let b = Book::open(&p).unwrap(); // uses the snapshot, replays 1 record
        let head = b.ledger().head_hash();
        let accts = b.ledger().accounts();
        assert_eq!(b.ledger().last_seq(), 5);
        drop(b);

        let (_s, sc) = Store::open(&p).unwrap();
        let mut full = Ledger::new();
        for r in &sc.records {
            full.apply(r).unwrap();
        }
        assert_eq!(full.head_hash(), head);
        assert_eq!(full.accounts(), accts);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(snapshot_path(&p));
    }

    #[test]
    fn garbage_or_foreign_snapshot_is_ignored() {
        let p1 = temp_path("snapforeign1");
        let p2 = temp_path("snapforeign2");
        build(&p1); // b ends at 200
        Book::open(&p1).unwrap().write_snapshot().unwrap();

        // A different history with the same number of records.
        let mut b2 = Book::open(&p2).unwrap();
        b2.submit(open_acct("a"), 1000).unwrap();
        b2.submit(open_acct("b"), 1001).unwrap();
        b2.submit(transfer("k1", "a", "b", 999), 1002).unwrap();
        b2.submit(transfer("k2", "b", "a", 1), 1003).unwrap();
        drop(b2);

        std::fs::copy(snapshot_path(&p1), snapshot_path(&p2)).unwrap();
        let b = Book::open(&p2).unwrap();
        assert_eq!(b.ledger().account("b").unwrap().1, 998);
        drop(b);

        std::fs::write(snapshot_path(&p2), b"garbage").unwrap();
        let b = Book::open(&p2).unwrap();
        assert_eq!(b.ledger().account("b").unwrap().1, 998);
        drop(b);

        for p in [&p1, &p2] {
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(snapshot_path(p));
        }
    }

    #[test]
    fn automatic_snapshot_interval() {
        let p = temp_path("snapauto");
        let mut b = Book::open(&p).unwrap();
        b.set_snapshot_interval(2);
        b.submit(open_acct("a"), 1).unwrap();
        assert!(!snapshot_path(&p).exists());
        b.submit(open_acct("b"), 2).unwrap();
        assert!(snapshot_path(&p).exists());
        drop(b);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(snapshot_path(&p));
    }
}
