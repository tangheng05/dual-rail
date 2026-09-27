# Security policy

dual-rail handles payments, so please report security problems privately.

## Reporting a vulnerability

Use GitHub's private reporting: the **Security** tab of this repository, then
**Report a vulnerability**. Please don't open a public issue, pull request or
discussion for a security problem.

Include what you found, how to reproduce it, and what an attacker could do
with it. You'll get an acknowledgement within 7 days, and a fix or a plan
before any details are made public.

## Supported versions

Only the latest release gets security fixes.

## Scope

In scope: anything that could credit money that was not paid, lose a payment
that was, bypass webhook verification or idempotency, or expose secrets.

Stripe, Bakong and the Bakong KHQR apps are out of scope; report problems in
those to their owners.
