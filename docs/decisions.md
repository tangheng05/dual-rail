# Design decisions

The choices below are the ones a reader is most likely to question, or to
"fix" back into a bug. Each says what we do, why, and what follows from it.

## 1. Money is an integer in ISO 4217 minor units, for every currency

**Decision.** Amounts are `i64` minor units inside a `Money` type that is
always positive and only adds with overflow checks. KHR uses exponent 2 like
USD, so 1000 riel is `100000`.

**Why.** Floats can't represent most decimal amounts exactly. One unit
convention for every rail means the ledger never has to know which rail an
amount came from. Exponent 2 for KHR matches both ISO 4217 and Stripe.

**Consequences.** KHQR only encodes whole riel, so a KHR amount not divisible
by 100 is rejected (422) instead of rounded. Amounts enter a QR through
`khqr-core`'s exact `amount_minor`, never through its rounding `amount(f64)`.

## 2. Provider is separate from method

**Decision.** `method` is what the customer uses (`card`, `khqr`); `provider`
is who moves the money (`stripe`, `bakong`). Clearing accounts, webhook
matching, event dedupe and polling all key on `provider`.

**Why.** One provider can serve several methods: ABA PayWay takes both cards
and KHQR. Keying the ledger on `method` would post a PayWay card payment to
Stripe's clearing account.

## 3. One settlement path, one transaction

**Decision.** Every status change goes through `settlement::apply`: lock the
row, check the transition, `UPDATE … WHERE status = 'pending'`, and write the
journal entry, all in one transaction. Database triggers reject any UPDATE or
DELETE on ledger rows.

**Why.** At-least-once webhooks, concurrent pollers and crashes can't produce
a second credit, a status without its ledger entry, or a ledger entry without
its status.

## 4. We verify Stripe webhook signatures ourselves

**Decision.** `rails::stripe_webhook` checks the HMAC over the raw body, and
parses only the fields we act on. `async-stripe` is used only to call the API.

**Why.** The `async-stripe` 1.0 release candidate keeps only the last `v1`
signature in the header, so valid events fail while a webhook secret is being
rolled. Its typed parser also fails whenever the account's API version differs
from the SDK's.

## 5. `payment_intent.payment_failed` does not fail a payment

**Decision.** A failed attempt leaves the payment `pending`. Only
`payment_intent.canceled` moves it to `failed`.

**Why.** After a declined card, Stripe returns the PaymentIntent to
`requires_payment_method`, and the customer can retry it and succeed. Treating
the first decline as final would receive the money but never record it.

**Consequences.** Abandoned card payments stay pending, and reconciliation
reports them as `stale_pending`.

## 6. Webhooks match on our metadata, and never credit a mismatch

**Decision.** A Stripe event finds its payment through `metadata.payment_id`,
set when the intent is created. If `amount_received` or the currency differs
from our record, nothing is credited and the payment goes to `review_flags`.

**Why.** An event can arrive before we have saved the intent id. Events from
other integrations on the same Stripe account carry no such metadata and are
acknowledged and ignored.

## 7. A KHQR payment expires only on evidence

**Decision.**
- A payment becomes `expired` only after Bakong answers "not paid" at least
  120 seconds past the expiry.
- A transfer Bakong timestamps (`createdDateMs`) within 60 seconds of the
  expiry is credited.
- A later transfer is expired and flagged `paid_after_expiry`, never credited.
- Errors never expire a payment. After 24 hours of failed checks it is
  flagged `unverifiable`.

**Why.** Bakong may still be indexing a payment made in the last seconds, and
our clock, Bakong's and the wallet's clock differ. Wallets refuse expired QRs
by their own clock, so a small overshoot is clock skew, not a late payment.
Expiring on an outage would lose real money.

## 8. Every KHQR is unique, and crediting needs positive evidence

**Decision.** The QR's bill number is the payment UUID in base36 (exactly 25
characters), and `(provider, provider_ref)` is unique. A Bakong transfer is
credited only when its amount, currency and receiving account are all present
and match. One transfer hash can credit at most one payment.

**Why.** Two orders with the same amount created in the same millisecond would
otherwise produce the same QR, and therefore the same md5, so one transfer
could settle both.

## 9. Bakong is checked one md5 at a time on production

**Decision.** The verifier tries the batch endpoint and, on its first 403,
switches to single `check_transaction_by_md5` lookups for good.

**Why.** Bakong's production API answers `check_transaction_by_md5_list` with
a bare 403, while single lookups work.

## 10. Idempotent replays are rebuilt, not stored

**Decision.** `Idempotency-Key` plus a SHA-256 of the parsed request identify a
create. The same key and request replay the payment in its *current* state with
`Idempotent-Replayed: true`. A different request gets 409. If the original
request died before creating the Stripe intent, the replay finishes that step
with the same Stripe idempotency key.

**Why.** Stripe advises against storing `client_secret`, so a replay fetches it
again rather than returning a stored copy. A Stripe 409 means the same key is
already in flight. It is mapped to our own 409 ("retry shortly") and must never
fail the payment.

## 11. Reconciliation reports and never fixes

**Decision.** A daily run compares the ledger with Stripe and Bakong for one
Cambodian business day (`+07:00`, windowed on `settled_at`) and writes findings
to `reconciliation_runs`. It does not change payments or the ledger.

**Why.** An automatic "fix" for a payment discrepancy is itself a way to lose
money. A human decides.

**Consequences.** A dynamic KHQR paid twice cannot be detected through
Bakong's API. Runs record `double_payment_check: "not_available"` until bank
statements are reconciled.

## 12. One TLS backend

**Decision.** The whole dependency tree uses rustls's `aws-lc-rs` backend, and
`main` installs it before creating any client.

**Why.** With both `ring` and `aws-lc-rs` compiled in, rustls cannot choose one
and panics on the first HTTPS request. `tests/startup.rs` builds the real
Stripe and Bakong clients to catch a dependency that brings the other backend
back.

## 13. KHQR timestamps are whole milliseconds

**Decision.** A KHQR payment's creation time is truncated to the millisecond
before anything uses it.

**Why.** The QR encodes milliseconds and Postgres stores microseconds. On a
Linux clock with nanoseconds, the first response and a replay would otherwise
report different expiry times.
