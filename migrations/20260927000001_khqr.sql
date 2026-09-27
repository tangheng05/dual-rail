alter table payments add column provider text;
update payments set provider = case method when 'card' then 'stripe' else 'bakong' end;

alter table payments
    alter column provider set not null,
    add constraint payments_provider_check check (provider in ('stripe', 'bakong')),
    add column next_check_at  timestamptz,
    add column check_attempts integer not null default 0,
    add constraint khqr_payments_are_complete check (
        method <> 'khqr'
        or (khqr_payload is not null and provider_ref is not null and expires_at is not null)
    );

create unique index payments_provider_ref_key on payments (provider, provider_ref)
    where provider_ref is not null;

create index payments_khqr_due_idx on payments (next_check_at)
    where provider = 'bakong' and status = 'pending' and next_check_at is not null;

create table review_flags (
    id         uuid primary key default gen_random_uuid(),
    payment_id uuid not null references payments (id),
    reason     text not null check (reason in ('amount_mismatch', 'paid_after_expiry', 'unverifiable', 'duplicate_transfer')),
    details    jsonb not null default '{}',
    created_at timestamptz not null default now(),
    unique (payment_id, reason)
);

-- v1 has no refunds or corrections, so a payment has at most one entry. Drop this
-- when correction entries arrive.
alter table journal_entries add constraint journal_entries_payment_id_key unique (payment_id);
