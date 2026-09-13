-- Recent-first API lists.
CREATE INDEX runs_created ON runs (created_at);
CREATE INDEX artifacts_created ON artifacts (created_at);

-- Lifecycle cleanup filters by terminal status and age. The composite index also
-- replaces the old status-only index because status remains its leading column.
ALTER TABLE runs
    ADD INDEX runs_status_updated (status, updated_at),
    DROP INDEX runs_status;

-- Resume and cleanup both address artifacts by their owning run.
CREATE INDEX artifacts_run_created ON artifacts (run_id, created_at);

-- Inventory builds symbol summaries per file in source order.
CREATE INDEX chunks_file_line ON chunks (file_id, start_line);

-- Cache eviction removes the oldest bounded batch from each cache.
CREATE INDEX parse_cache_updated ON parse_cache (updated_at);
CREATE INDEX llm_cache_created ON llm_cache (created_at);
CREATE INDEX retrieval_cache_created ON retrieval_cache (created_at);
