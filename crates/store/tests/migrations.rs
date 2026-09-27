use sqlx::PgPool;

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn seeds_ledger_accounts(pool: PgPool) {
    let codes: Vec<String> = sqlx::query_scalar("select code from ledger_accounts order by code")
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(codes, ["clearing:bakong", "clearing:stripe", "revenue"]);
}

#[sqlx::test(migrator = "dual_rail_store::MIGRATOR")]
async fn ledger_rows_are_append_only(pool: PgPool) {
    sqlx::raw_sql(
        "insert into payments (id, method, provider, status, amount_minor, currency, idempotency_key)
         values ('00000000-0000-0000-0000-000000000001', 'card', 'stripe', 'succeeded', 1000, 'USD', 'k1');
         insert into journal_entries (id, payment_id)
         values ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000001');
         insert into ledger_lines (journal_entry_id, account_id, direction, amount_minor, currency)
         select '00000000-0000-0000-0000-000000000002', id, 'debit', 1000, 'USD'
         from ledger_accounts where code = 'clearing:stripe';",
    )
    .execute(&pool)
    .await
    .unwrap();

    for statement in [
        "update ledger_lines set amount_minor = 1",
        "delete from ledger_lines",
        "update journal_entries set created_at = now()",
        "delete from journal_entries",
    ] {
        let err = sqlx::query(statement).execute(&pool).await.unwrap_err();
        assert!(
            err.to_string().contains("append-only"),
            "{statement}: {err}"
        );
    }
}
