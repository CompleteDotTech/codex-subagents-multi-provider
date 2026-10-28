-- Host-only effective authority, independent of rebuildable rollout metadata.
-- An active generation never authorizes cold reload; only a stopped host Session
-- may seal its nonsecret effective-grant fingerprint for that same generation.
CREATE TABLE native_authority_receipts (
    thread_id TEXT PRIMARY KEY NOT NULL,
    root_thread_id TEXT NOT NULL,
    parent_thread_id TEXT,
    version INTEGER NOT NULL,
    generation TEXT NOT NULL,
    fingerprint TEXT,
    sealed INTEGER NOT NULL DEFAULT 0 CHECK (sealed IN (0, 1)),
    CHECK (fingerprint IS NULL OR length(fingerprint) = 64),
    CHECK (sealed = 0 OR fingerprint IS NOT NULL)
);
