-- Monthly ledger partitions (decision 0005). The writer calls this ahead of
-- need (the current and the next month at start-up, and any new month it
-- meets); an ops job exports cold partitions and drops them.

CREATE FUNCTION ensure_ledger_partition(m INTEGER) RETURNS VOID AS $$
BEGIN
    EXECUTE format(
        'CREATE TABLE IF NOT EXISTS ledger_%s PARTITION OF ledger FOR VALUES IN (%s)',
        m, m
    );
END
$$ LANGUAGE plpgsql;
