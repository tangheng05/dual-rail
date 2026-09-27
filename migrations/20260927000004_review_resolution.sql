alter table review_flags
    add column resolved_at timestamptz,
    add column resolution  text,
    add constraint review_flags_resolved_with_a_note
        check ((resolved_at is null) = (resolution is null));

create index review_flags_open_idx on review_flags (created_at) where resolved_at is null;
