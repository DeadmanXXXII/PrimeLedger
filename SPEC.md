# PrimeLedger format v1

All integers little-endian. Strings are `u32 length` + UTF-8 bytes (length limits enforced).
Amounts are `i64` minor units; positive = debit, negative = credit.

## Record body (canonical encoding)

    u64   seq              starts at 1, +1 each record, never reused
    [32]  prev_hash        hash of previous record; 32 zero bytes for seq 1
    i64   recorded_ms      Unix ms when written; never decreases
    u8    kind             1 = OpenAccount, 2 = Transaction

    OpenAccount: str account | [3] currency (A-Z) | u8 has_floor | (i64 floor if has_floor)
    Transaction: str idempotency_key | i64 effective_ms | str memo
                 | u8 has_reverses | (u64 seq if has_reverses)
                 | u32 n | n x (str account | i64 amount)

Flags are exactly 0 or 1. Trailing bytes are an error. Limits: account 64 bytes, key 128,
memo 256, postings 2..=256.

## Hash chain

    record_hash = SHA-256( "PRIMELEDGER/record/v1\0" || body )

`record.prev_hash` must equal the previous `record_hash`. Changing any byte of any record changes
its hash and breaks the link of every later record.

## File framing (`prime-store`)

    "PRIMELDG"                       8-byte magic
    repeat: u32 body_len | body | checksum[4]
            checksum = SHA-256(body_len_le || body)[0..4]

Torn writes: an incomplete or checksum-failing *final* frame (or a zero-filled tail) is truncated on
open. Damage anywhere else is a hard error. Appends are fsynced before returning.

## Rules enforced on every record (write and replay)

- seq = previous + 1; prev_hash matches; recorded_ms >= previous
- account ids: 1..64 chars of `A-Z a-z 0-9 : . _ - /`; currency three uppercase letters; floor <= 0
- transaction: >= 2 postings, no zero amounts, accounts exist, **sum is zero per currency**
  (cross-currency movement needs explicit FX accounts), checked arithmetic, floors respected
- idempotency key never reused (a retry returns the original sequence number)
- reversal: targets an existing, not-yet-reversed, non-reversal transaction; postings are its exact
  negation; reversals bypass floors so mistakes can always be corrected

## Holds (two-phase movements)

Record tags 3-5: `Hold` (same fields as a transaction minus `reverses`), `Post{hold_seq}`, `Void{hold_seq}`.
A hold checks the same invariants as a transaction and reserves each account's outflows against its
floor, but changes no balance. It is resolved by exactly one later `Post` (postings take effect, and
the post is reversible like any transaction) or `Void` (reservation released). Spending is checked
against balance minus reserved funds. Settling or voiding bypasses rules and floors: it was already
authorised. Hold keys share the idempotency namespace with transactions.

## Rules

Record tag 6: `SetRules{version, source}`; `version` must be the previous version + 1 (first is 1).
Rules are a line-based language (see `prime_core::rules`) hashed into the chain, so every change is
auditable. They apply to new transactions and holds; reversals, `Post` and `Void` bypass them.

## Outbox and reconciliation

`prime events --after <seq>` prints one JSON object per record (`prime_core::events`); consumers
remember the last seq handled. `prime reconcile` matches a statement CSV against one account
(`prime_core::recon`); posted holds count on their effective date, pending and voided ones never do.

## Merkle day roots

Leaves are record hashes grouped by UTC day of `recorded_ms`.

    leaf = SHA-256(0x00 || record_hash)      node = SHA-256(0x01 || left || right)

An odd last node is promoted, not duplicated. Empty set = SHA-256("PRIMELEDGER/merkle/empty").
Inclusion proofs are the sibling path with a side flag. Publish `prime roots` output externally
to make rewrites detectable even by the machine's own admin.

## Bitemporal queries

`recorded_ms` (when written) and a transaction's `effective_ms` (when it happened) are independent
clocks. `balances_as_of` can filter on either or both; account openings follow recorded time only.

## FX

Cross-currency movement is four postings through two explicit pool accounts (one per currency), so
each currency still sums to zero. See `prime_core::fx::exchange`.

## Snapshot (`<ledger file>.snap`, cache only)

    "PRIMESNAP2" | body | SHA-256("PRIMESNAP2" || body)
    body: u64 seq | [32] head_hash | i64 last_ms | accounts (with reserved) | idempotency keys | transactions | holds | rules

Deterministic: the same state yields the same bytes. On open, a snapshot is used only if the log's
record at `seq` hashes to `head_hash`; the remaining records are replayed through the normal rules.
Written atomically (temp file, fsync, rename) every 1000 records or via `prime snapshot`.
