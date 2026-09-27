alter table payments add column settled_at timestamptz;
update payments set settled_at = updated_at where status <> 'pending';
create index payments_settled_at_idx on payments (settled_at) where settled_at is not null;

alter table reconciliation_runs
    add column finished_at timestamptz,
    add column summary     jsonb not null default '{}',
    add constraint reconciliation_runs_status_check
        check (status in ('running', 'completed', 'incomplete', 'failed'));

create unique index reconciliation_runs_one_running on reconciliation_runs (run_date)
    where status = 'running';
