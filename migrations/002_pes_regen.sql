-- Track per-file captions.pes regeneration state.
-- 0 = idle (default), 1 = queued, 2 = active (being regenerated).
-- Status stays 'done'; this column is independent of the ingest status column.
ALTER TABLE ts_files ADD COLUMN pes_regen INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_ts_files_pes_regen ON ts_files(pes_regen);
