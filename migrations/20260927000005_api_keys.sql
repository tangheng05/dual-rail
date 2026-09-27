create table api_keys (
    id           uuid primary key default gen_random_uuid(),
    name         text not null,
    -- The first characters of the key, so a person can tell keys apart.
    prefix       text not null,
    key_hash     text not null unique,
    created_at   timestamptz not null default now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);
