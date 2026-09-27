create table payments (
    id              uuid primary key default gen_random_uuid(),
    method          text not null check (method in ('card', 'khqr')),
    status          text not null check (status in ('pending', 'succeeded', 'failed', 'expired')),
    amount_minor    bigint not null check (amount_minor > 0),
    currency        text not null check (currency in ('USD', 'KHR')),
    idempotency_key text not null unique,
    provider_ref    text,
    khqr_payload    text,
    expires_at      timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);

create table processed_events (
    provider     text not null check (provider in ('stripe', 'bakong')),
    event_id     text not null,
    processed_at timestamptz not null default now(),
    primary key (provider, event_id)
);

create table ledger_accounts (
    id   bigint generated always as identity primary key,
    code text not null unique,
    name text not null
);

insert into ledger_accounts (code, name) values
    ('clearing:stripe', 'Stripe clearing'),
    ('clearing:bakong', 'Bakong clearing'),
    ('revenue', 'Revenue');

create table journal_entries (
    id         uuid primary key default gen_random_uuid(),
    payment_id uuid not null references payments (id),
    created_at timestamptz not null default now()
);

create table ledger_lines (
    id               uuid primary key default gen_random_uuid(),
    journal_entry_id uuid not null references journal_entries (id),
    account_id       bigint not null references ledger_accounts (id),
    direction        text not null check (direction in ('debit', 'credit')),
    amount_minor     bigint not null check (amount_minor > 0),
    currency         text not null check (currency in ('USD', 'KHR'))
);

create index ledger_lines_journal_entry_id_idx on ledger_lines (journal_entry_id);

create function reject_ledger_mutation() returns trigger
language plpgsql as $$
begin
    raise exception '% is append-only', tg_table_name;
end;
$$;

create trigger journal_entries_append_only
    before update or delete on journal_entries
    for each row execute function reject_ledger_mutation();

create trigger ledger_lines_append_only
    before update or delete on ledger_lines
    for each row execute function reject_ledger_mutation();

create table reconciliation_runs (
    id         uuid primary key default gen_random_uuid(),
    run_date   date not null,
    status     text not null,
    mismatches jsonb not null default '[]',
    created_at timestamptz not null default now()
);
