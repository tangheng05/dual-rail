# Changelog

All notable changes to this project are recorded here. The project follows
[semantic versioning](https://semver.org); before 1.0, any release may change
behavior, and this file says how.

## 0.1.0

The first release: card payments through Stripe and Bakong KHQR payments
behind one API and one double-entry ledger.

### Added

- `POST /payments` for `card` (Stripe PaymentIntents) and `khqr` (dynamic
  KHQR), `GET /payments/{id}`, `GET /payments/{id}/qr.svg`,
  `POST /webhooks/stripe` and `GET /health`.
- Idempotent creates: the same `Idempotency-Key` and request replay the
  original payment, and a different request gets 409.
- Verified Stripe webhooks with event deduplication, and a KHQR poller that
  checks Bakong with capped backoff and a grace period at expiry.
- A double-entry ledger written in the same transaction as each status change,
  append-only at the database level.
- Daily reconciliation against Stripe and Bakong, reporting to
  `reconciliation_runs`, and a `review_flags` queue for cases that need a
  human.
- A demo checkout page at `/`.
- A Docker image published to the GitHub container registry on each release.

See [docs/decisions.md](docs/decisions.md) for the reasoning behind the
payment rules.
