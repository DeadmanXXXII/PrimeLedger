//! PrimeLedger desktop app. A thin window over `prime-core` and `prime-store`: every change goes
//! through `Book::submit`, the same append-only path the CLI uses. There is no edit or delete.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod money;

use eframe::egui;
use prime_core::{Currency, LedgerError, Posting, Record, RecordKind};
use prime_store::{Book, BookError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const GREEN: egui::Color32 = egui::Color32::from_rgb(0, 150, 60);
const RED: egui::Color32 = egui::Color32::from_rgb(200, 40, 40);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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

fn fmt_day(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

fn fmt_time(ms: i64) -> String {
    let day = ms.div_euclid(86_400_000);
    let secs = ms.rem_euclid(86_400_000) / 1000;
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        fmt_day(day),
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn default_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(h) => h.join("Documents").join("PrimeLedger"),
        None => PathBuf::from("PrimeLedger"),
    }
}

fn list_ledgers(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("pld"))
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn roots_text(rep: &prime_verify::Report) -> String {
    let mut s = String::new();
    s.push_str("# PrimeLedger daily roots - publish this anywhere you like\n");
    s.push_str(&format!("# head {} records {}\n", rep.head_hash, rep.records));
    s.push_str("# date count first_seq root\n");
    for r in &rep.day_roots {
        s.push_str(&format!(
            "{} {} {} {}\n",
            fmt_day(r.day),
            r.count,
            r.first_seq,
            r.root
        ));
    }
    s
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Entry,
    Holds,
    History,
    Verify,
}

struct AccountRow {
    name: String,
    currency: String,
    balance: i64,
    available: i64,
}

struct HoldRow {
    seq: u64,
    postings: Vec<Posting>,
}

struct Line {
    account: String,
    debit: bool,
    amount: String,
}

impl Line {
    fn blank(debit: bool) -> Line {
        Line {
            account: String::new(),
            debit,
            amount: String::new(),
        }
    }
}

struct EntryForm {
    memo: String,
    lines: Vec<Line>,
    key: String,
    as_hold: bool,
}

enum Action {
    OpenPath(PathBuf),
    OpenTyped,
    NewLedger,
    CloseLedger,
    AddAccount,
    Submit,
    Settle(u64),
    Void(u64),
    Reverse(u64),
    Verify,
    ExportRoots,
}

struct PrimeApp {
    dir: PathBuf,
    files: Vec<PathBuf>,
    path_text: String,
    new_name: String,
    book: Option<Book>,
    path: Option<PathBuf>,
    tab: Tab,
    /// (is_error, message)
    notice: Option<(bool, String)>,
    accounts: Vec<AccountRow>,
    holds: Vec<HoldRow>,
    records: Vec<Record>,
    head: String,
    form: EntryForm,
    new_acct_name: String,
    new_acct_cur: String,
    new_acct_floor: String,
    verify_report: Option<prime_verify::Report>,
    verify_err: Option<String>,
    counter: u64,
}

impl PrimeApp {
    fn new() -> PrimeApp {
        let dir = default_dir();
        let files = list_ledgers(&dir);
        let mut app = PrimeApp {
            dir,
            files,
            path_text: String::new(),
            new_name: String::new(),
            book: None,
            path: None,
            tab: Tab::Accounts,
            notice: None,
            accounts: Vec::new(),
            holds: Vec::new(),
            records: Vec::new(),
            head: String::new(),
            form: EntryForm {
                memo: String::new(),
                lines: Vec::new(),
                key: String::new(),
                as_hold: false,
            },
            new_acct_name: String::new(),
            new_acct_cur: "GBP".to_string(),
            new_acct_floor: String::new(),
            verify_report: None,
            verify_err: None,
            counter: 0,
        };
        app.reset_form();
        app
    }

    fn fresh_key(&mut self) -> String {
        self.counter += 1;
        format!("app-{}-{}", now_ms(), self.counter)
    }

    /// A new form gets a new idempotency key; a failed submit keeps its key, so retrying the
    /// same entry can never record it twice.
    fn reset_form(&mut self) {
        let key = self.fresh_key();
        let as_hold = self.form.as_hold;
        self.form = EntryForm {
            memo: String::new(),
            lines: vec![Line::blank(true), Line::blank(false)],
            key,
            as_hold,
        };
    }

    fn currency_of(&self, account: &str) -> String {
        self.accounts
            .iter()
            .find(|a| a.name == account)
            .map(|a| a.currency.clone())
            .unwrap_or_default()
    }

    fn dec_for(&self, account: &str) -> u32 {
        money::decimals(&self.currency_of(account))
    }

    fn posting_text(&self, p: &Posting) -> String {
        let side = if p.amount > 0 { "Dr" } else { "Cr" };
        format!(
            "{} {} {}",
            p.account,
            side,
            money::format_minor(p.amount.saturating_abs(), self.dec_for(&p.account))
        )
    }

    fn refresh_files(&mut self) {
        self.files = list_ledgers(&self.dir);
    }

    fn refresh(&mut self) {
        self.accounts.clear();
        self.holds.clear();
        self.records.clear();
        self.head.clear();
        let (Some(book), Some(path)) = (&self.book, &self.path) else {
            return;
        };
        let ledger = book.ledger();
        self.accounts = ledger
            .accounts()
            .into_iter()
            .map(|(name, cur, balance)| {
                let available = ledger.available(&name).unwrap_or(balance);
                AccountRow {
                    name,
                    currency: cur.to_string(),
                    balance,
                    available,
                }
            })
            .collect();
        self.holds = ledger
            .pending_holds()
            .into_iter()
            .map(|(seq, ps)| HoldRow {
                seq,
                postings: ps.to_vec(),
            })
            .collect();
        self.head = format!("#{} {}", ledger.last_seq(), ledger.head_hash());
        match std::fs::read(path) {
            Ok(data) => match prime_store::scan(&data) {
                Ok(sc) => self.records = sc.records,
                Err(e) => self.notice = Some((true, format!("Could not read history: {e}"))),
            },
            Err(e) => self.notice = Some((true, format!("Could not read history: {e}"))),
        }
    }

    fn open_ledger(&mut self, path: PathBuf) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Book::open(&path) {
            Ok(book) => {
                let torn = book.recovered_torn_tail();
                self.book = Some(book);
                self.path_text = path.display().to_string();
                self.path = Some(path);
                self.verify_report = None;
                self.verify_err = None;
                self.tab = Tab::Accounts;
                self.reset_form();
                self.notice = Some((
                    false,
                    if torn {
                        "Opened. A half-written final record from an earlier crash was removed."
                            .to_string()
                    } else {
                        "Ledger opened.".to_string()
                    },
                ));
            }
            Err(e) => self.notice = Some((true, format!("Could not open that ledger: {e}"))),
        }
    }

    /// Append one record. Returns true if it is now in the ledger (including "already there").
    fn submit(&mut self, kind: RecordKind) -> bool {
        let Some(book) = self.book.as_mut() else {
            return false;
        };
        match book.submit(kind, now_ms()) {
            Ok(rec) => {
                self.notice = Some((false, format!("Recorded as #{}.", rec.seq)));
                true
            }
            Err(BookError::Ledger(LedgerError::DuplicateIdempotencyKey { seq, .. })) => {
                self.notice = Some((false, format!("That was already recorded as #{seq}.")));
                true
            }
            Err(e) => {
                self.notice = Some((true, e.to_string()));
                false
            }
        }
    }

    fn form_postings(&self) -> Result<Vec<Posting>, String> {
        let mut out = Vec::new();
        for (i, l) in self.form.lines.iter().enumerate() {
            if l.account.is_empty() && l.amount.trim().is_empty() {
                continue;
            }
            let n = i + 1;
            if !self.accounts.iter().any(|a| a.name == l.account) {
                return Err(format!("Line {n}: choose an account."));
            }
            let v = money::parse_amount(&l.amount, self.dec_for(&l.account))
                .map_err(|e| format!("Line {n}: {e}."))?;
            if v <= 0 {
                return Err(format!("Line {n}: the amount must be above zero."));
            }
            out.push(Posting {
                account: l.account.clone(),
                amount: if l.debit { v } else { -v },
            });
        }
        Ok(out)
    }

    /// Per-currency running totals of the form, ignoring lines that don't parse yet.
    fn form_sums(&self) -> Vec<(String, i64, u32)> {
        let mut m: BTreeMap<String, i64> = BTreeMap::new();
        for l in &self.form.lines {
            let cur = self.currency_of(&l.account);
            if cur.is_empty() {
                continue;
            }
            if let Ok(v) = money::parse_amount(&l.amount, money::decimals(&cur)) {
                let signed = if l.debit { v } else { v.saturating_neg() };
                let e = m.entry(cur).or_insert(0);
                *e = e.saturating_add(signed);
            }
        }
        m.into_iter()
            .map(|(c, s)| {
                let d = money::decimals(&c);
                (c, s, d)
            })
            .collect()
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::OpenPath(p) => self.open_ledger(p),
            Action::OpenTyped => {
                let p = PathBuf::from(self.path_text.trim());
                self.open_ledger(p);
            }
            Action::NewLedger => {
                let name = self.new_name.trim().to_string();
                let ok = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-');
                if !ok {
                    self.notice = Some((
                        true,
                        "Use letters, numbers, spaces, - or _ for the ledger name.".to_string(),
                    ));
                    return;
                }
                let p = self.dir.join(format!("{name}.pld"));
                if p.exists() {
                    self.notice = Some((true, "A ledger with that name already exists.".to_string()));
                    return;
                }
                self.open_ledger(p);
                self.new_name.clear();
            }
            Action::CloseLedger => {
                self.book = None;
                self.path = None;
                self.notice = None;
                self.verify_report = None;
                self.verify_err = None;
            }
            Action::AddAccount => {
                let name = self.new_acct_name.trim().to_string();
                let cur_text = self.new_acct_cur.trim().to_ascii_uppercase();
                let Some(currency) = Currency::parse(&cur_text) else {
                    self.notice = Some((
                        true,
                        "The currency must be 3 letters, for example GBP.".to_string(),
                    ));
                    return;
                };
                let floor_text = self.new_acct_floor.trim().to_string();
                let floor: Result<Option<i64>, String> = if floor_text.is_empty() {
                    Ok(None)
                } else {
                    money::parse_amount(&floor_text, money::decimals(currency.as_str())).and_then(
                        |v| {
                            if v <= 0 {
                                Ok(Some(v))
                            } else {
                                Err("The lowest allowed balance must be 0 or negative".to_string())
                            }
                        },
                    )
                };
                let floor = match floor {
                    Ok(f) => f,
                    Err(e) => {
                        self.notice = Some((true, format!("{e}.")));
                        return;
                    }
                };
                let kind = RecordKind::OpenAccount {
                    account: name,
                    currency,
                    floor,
                };
                if self.submit(kind) {
                    self.new_acct_name.clear();
                    self.new_acct_floor.clear();
                }
            }
            Action::Submit => {
                let postings = match self.form_postings() {
                    Ok(p) => p,
                    Err(e) => {
                        self.notice = Some((true, e));
                        return;
                    }
                };
                let now = now_ms();
                let key = self.form.key.clone();
                let memo = self.form.memo.trim().to_string();
                let kind = if self.form.as_hold {
                    RecordKind::Hold {
                        idempotency_key: key,
                        effective_ms: now,
                        memo,
                        postings,
                    }
                } else {
                    RecordKind::Transaction {
                        idempotency_key: key,
                        effective_ms: now,
                        memo,
                        reverses: None,
                        postings,
                    }
                };
                if self.submit(kind) {
                    self.reset_form();
                }
            }
            Action::Settle(hold_seq) => {
                self.submit(RecordKind::Post { hold_seq });
            }
            Action::Void(hold_seq) => {
                self.submit(RecordKind::Void { hold_seq });
            }
            Action::Reverse(seq) => {
                let Some(book) = self.book.as_ref() else {
                    return;
                };
                let Some(orig) = book.ledger().transaction_postings(seq) else {
                    self.notice = Some((true, format!("Record #{seq} is not a transaction.")));
                    return;
                };
                let mut postings = Vec::with_capacity(orig.len());
                for p in orig {
                    match p.amount.checked_neg() {
                        Some(n) => postings.push(Posting {
                            account: p.account.clone(),
                            amount: n,
                        }),
                        None => {
                            self.notice = Some((true, "Amount too large to reverse.".to_string()));
                            return;
                        }
                    }
                }
                let kind = RecordKind::Transaction {
                    idempotency_key: format!("app-reverse-{seq}"),
                    effective_ms: now_ms(),
                    memo: format!("Reversal of #{seq}"),
                    reverses: Some(seq),
                    postings,
                };
                self.submit(kind);
            }
            Action::Verify => {
                if let Some(p) = &self.path {
                    match prime_verify::verify_file(p) {
                        Ok(r) => {
                            self.verify_report = Some(r);
                            self.verify_err = None;
                        }
                        Err(e) => {
                            self.verify_report = None;
                            self.verify_err = Some(e.to_string());
                        }
                    }
                }
            }
            Action::ExportRoots => {
                self.handle(Action::Verify);
                if let (Some(rep), Some(p)) = (&self.verify_report, &self.path) {
                    let out = p.with_extension("roots.txt");
                    match std::fs::write(&out, roots_text(rep)) {
                        Ok(()) => {
                            self.notice =
                                Some((false, format!("Daily roots saved to {}", out.display())))
                        }
                        Err(e) => {
                            self.notice = Some((true, format!("Could not save the roots file: {e}")))
                        }
                    }
                } else {
                    self.notice = Some((
                        true,
                        "Verification failed, so no roots were exported.".to_string(),
                    ));
                }
            }
        }
    }

    fn describe(&self, kind: &RecordKind) -> String {
        let memo_text = |m: &str| {
            if m.is_empty() {
                "(no description)".to_string()
            } else {
                m.to_string()
            }
        };
        match kind {
            RecordKind::OpenAccount {
                account,
                currency,
                floor,
            } => {
                let lowest = match floor {
                    Some(f) => format!(
                        ", lowest balance {}",
                        money::format_minor(*f, money::decimals(currency.as_str()))
                    ),
                    None => String::new(),
                };
                format!("Account opened: {account} ({currency}){lowest}")
            }
            RecordKind::Transaction {
                memo,
                reverses,
                postings,
                ..
            } => {
                let head = match reverses {
                    Some(t) => format!("Reversal of #{t}"),
                    None => "Transaction".to_string(),
                };
                let lines: Vec<String> = postings.iter().map(|p| self.posting_text(p)).collect();
                format!("{head}: {} [{}]", memo_text(memo.as_str()), lines.join(", "))
            }
            RecordKind::Hold { memo, postings, .. } => {
                let lines: Vec<String> = postings.iter().map(|p| self.posting_text(p)).collect();
                format!("Hold placed: {} [{}]", memo_text(memo.as_str()), lines.join(", "))
            }
            RecordKind::Post { hold_seq } => format!("Hold #{hold_seq} settled"),
            RecordKind::Void { hold_seq } => format!("Hold #{hold_seq} voided"),
            RecordKind::SetRules { version, .. } => format!("Rules changed to version {version}"),
        }
    }

    fn ui_top(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("PrimeLedger");
            ui.label(
                egui::RichText::new("Infinite sequence, no rewrites, mathematically pure.").weak(),
            );
        });
        if self.book.is_some() {
            ui.horizontal(|ui| {
                ui.label(format!("Ledger: {}", self.path_text));
                if ui.button("Close ledger").clicked() {
                    actions.push(Action::CloseLedger);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Head:");
                ui.monospace(self.head.as_str());
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Accounts, "Accounts");
                ui.selectable_value(&mut self.tab, Tab::Entry, "New entry");
                ui.selectable_value(&mut self.tab, Tab::Holds, "Holds");
                ui.selectable_value(&mut self.tab, Tab::History, "History");
                ui.selectable_value(&mut self.tab, Tab::Verify, "Verify");
            });
        }
        if let Some((is_err, msg)) = &self.notice {
            let color = if *is_err { RED } else { GREEN };
            ui.colored_label(color, msg.as_str());
        }
    }

    fn ui_welcome(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Open or create a ledger");
        ui.label(format!("Your ledgers are kept in: {}", self.dir.display()));
        ui.add_space(6.0);
        if self.files.is_empty() {
            ui.label("No ledgers yet. Create your first one below.");
        }
        for p in &self.files {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if ui.button(format!("Open {name}")).clicked() {
                actions.push(Action::OpenPath(p.clone()));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("New ledger name:");
            ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text("e.g. household"));
            if ui.button("Create").clicked() {
                actions.push(Action::NewLedger);
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Or open a file by its path:");
            ui.text_edit_singleline(&mut self.path_text);
            if ui.button("Open").clicked() {
                actions.push(Action::OpenTyped);
            }
        });
    }

    fn ui_accounts(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Accounts");
        if self.accounts.is_empty() {
            ui.label("No accounts yet. Add one below.");
        } else {
            egui::Grid::new("accounts_grid")
                .striped(true)
                .num_columns(4)
                .spacing([24.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("Account");
                    ui.strong("Currency");
                    ui.strong("Balance");
                    ui.strong("Available");
                    ui.end_row();
                    for a in &self.accounts {
                        let dec = money::decimals(&a.currency);
                        ui.label(a.name.as_str());
                        ui.label(a.currency.as_str());
                        ui.monospace(money::format_minor(a.balance, dec));
                        ui.monospace(money::format_minor(a.available, dec));
                        ui.end_row();
                    }
                });
            ui.label(
                egui::RichText::new(
                    "Available is the balance minus funds reserved by pending holds.",
                )
                .weak(),
            );
        }
        ui.separator();
        ui.heading("Add an account");
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_acct_name)
                    .desired_width(160.0)
                    .hint_text("e.g. cash"),
            );
            ui.label("Currency");
            ui.add(egui::TextEdit::singleline(&mut self.new_acct_cur).desired_width(48.0));
        });
        ui.horizontal(|ui| {
            ui.label("Lowest allowed balance");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_acct_floor)
                    .desired_width(110.0)
                    .hint_text("blank = no limit"),
            );
        });
        if ui.button("Add account").clicked() {
            actions.push(Action::AddAccount);
        }
        ui.label(
            egui::RichText::new(
                "Tip: put money in from outside (opening balances, income) by crediting an \
                 account such as 'equity' that has no limit. Use 0 as the lowest balance for \
                 accounts that must never go negative.",
            )
            .weak(),
        );
    }

    fn ui_entry(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading(if self.form.as_hold {
            "New hold"
        } else {
            "New transaction"
        });
        if self.accounts.len() < 2 {
            ui.label("Add at least two accounts first (Accounts tab).");
            return;
        }
        ui.checkbox(
            &mut self.form.as_hold,
            "Hold only: reserve the funds now, settle or void later (Holds tab)",
        );
        ui.horizontal(|ui| {
            ui.label("Description");
            ui.add(egui::TextEdit::singleline(&mut self.form.memo).desired_width(320.0));
        });
        ui.add_space(4.0);
        let names: Vec<String> = self.accounts.iter().map(|a| a.name.clone()).collect();
        let mut remove: Option<usize> = None;
        for (i, line) in self.form.lines.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let shown = if line.account.is_empty() {
                    "choose account".to_string()
                } else {
                    line.account.clone()
                };
                egui::ComboBox::new(("entry_account", i), "")
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        for n in &names {
                            ui.selectable_value(&mut line.account, n.clone(), n.as_str());
                        }
                    });
                ui.selectable_value(&mut line.debit, true, "Debit");
                ui.selectable_value(&mut line.debit, false, "Credit");
                ui.add(
                    egui::TextEdit::singleline(&mut line.amount)
                        .desired_width(110.0)
                        .hint_text("0.00"),
                );
                if ui.small_button("x").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            if self.form.lines.len() > 2 {
                self.form.lines.remove(i);
            }
        }
        if ui.button("+ Add line").clicked() {
            self.form.lines.push(Line::blank(true));
        }
        ui.add_space(4.0);
        for (cur, sum, dec) in self.form_sums() {
            if sum == 0 {
                ui.colored_label(GREEN, format!("{cur}: debits and credits balance"));
            } else {
                ui.colored_label(
                    RED,
                    format!(
                        "{cur}: debits and credits differ by {}",
                        money::format_minor(sum.saturating_abs(), dec)
                    ),
                );
            }
        }
        let label = if self.form.as_hold {
            "Place hold"
        } else {
            "Post transaction"
        };
        if ui.button(label).clicked() {
            actions.push(Action::Submit);
        }
        ui.label(
            egui::RichText::new(
                "Entries are permanent. To fix a mistake, reverse it from the History tab.",
            )
            .weak(),
        );
    }

    fn ui_holds(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Pending holds");
        if self.holds.is_empty() {
            ui.label("No pending holds.");
        }
        for h in &self.holds {
            ui.group(|ui| {
                ui.label(format!("Hold #{}", h.seq));
                for p in &h.postings {
                    ui.monospace(self.posting_text(p));
                }
                ui.horizontal(|ui| {
                    if ui.button("Settle (post it)").clicked() {
                        actions.push(Action::Settle(h.seq));
                    }
                    if ui.button("Void (release it)").clicked() {
                        actions.push(Action::Void(h.seq));
                    }
                });
            });
        }
    }

    fn ui_history(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("History");
        let total = self.records.len();
        ui.label(format!(
            "{total} records, newest first. Entries are permanent; to correct one, reverse it."
        ));
        let row_h = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total, |ui, range| {
                for i in range {
                    let r = &self.records[total - 1 - i];
                    ui.horizontal(|ui| {
                        ui.monospace(format!("#{:<5}", r.seq));
                        ui.label(fmt_time(r.recorded_ms));
                        ui.label(self.describe(&r.kind));
                        let reversible = matches!(
                            &r.kind,
                            RecordKind::Transaction { reverses: None, .. } | RecordKind::Post { .. }
                        );
                        if reversible && ui.small_button("Reverse").clicked() {
                            actions.push(Action::Reverse(r.seq));
                        }
                    });
                }
            });
    }

    fn ui_verify(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Integrity check");
        ui.label(
            "Re-reads the whole file from the start, checks every hash link and rule, and \
             recomputes every balance independently.",
        );
        if ui.button("Run full verification").clicked() {
            actions.push(Action::Verify);
        }
        if let Some(rep) = &self.verify_report {
            ui.colored_label(
                GREEN,
                format!("VERIFIED: all {} records check out.", rep.records),
            );
            ui.label(format!(
                "{} accounts, {} pending holds.",
                rep.accounts, rep.pending_holds
            ));
            ui.horizontal(|ui| {
                ui.label("Head hash:");
                ui.monospace(rep.head_hash.to_string());
            });
            if rep.torn_tail {
                ui.label("A half-written final record from a crash is present (not an integrity failure).");
            }
            if ui.button("Export daily roots to a file").clicked() {
                actions.push(Action::ExportRoots);
            }
            ui.label(
                egui::RichText::new(
                    "Publish the roots anywhere you like (email, print, a repository) to prove \
                     history was not rewritten after that date.",
                )
                .weak(),
            );
            ui.separator();
            ui.label("Most recent daily roots:");
            for d in rep.day_roots.iter().rev().take(20) {
                ui.monospace(format!(
                    "{}  {:>4} records  {}",
                    fmt_day(d.day),
                    d.count,
                    d.root
                ));
            }
        } else if let Some(e) = &self.verify_err {
            ui.colored_label(RED, format!("VERIFICATION FAILED: {e}"));
        }
    }
}

impl eframe::App for PrimeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut actions: Vec<Action> = Vec::new();
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            self.ui_top(ui, &mut actions);
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.book.is_none() {
                self.ui_welcome(ui, &mut actions);
                return;
            }
            match self.tab {
                Tab::History => self.ui_history(ui, &mut actions),
                tab => {
                    egui::ScrollArea::vertical().show(ui, |ui| match tab {
                        Tab::Accounts => self.ui_accounts(ui, &mut actions),
                        Tab::Entry => self.ui_entry(ui, &mut actions),
                        Tab::Holds => self.ui_holds(ui, &mut actions),
                        Tab::Verify => self.ui_verify(ui, &mut actions),
                        Tab::History => {}
                    });
                }
            }
        });
        for a in actions {
            self.handle(a);
            self.refresh_files();
            self.refresh();
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PrimeLedger")
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PrimeLedger",
        options,
        Box::new(|_cc| Ok(Box::new(PrimeApp::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_day(19_723), "2024-01-01");
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time(86_400_000 + 3_661_000), "1970-01-02 01:01:01 UTC");
    }
}
