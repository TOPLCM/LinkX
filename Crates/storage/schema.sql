-- LinkX 本地存储 schema（4.7，单一源：两端 build 都由本文件生成）
-- Windows: rusqlite；Android: Room（schema 由本文件迁移）
-- 时间戳一律 INTEGER UTC 毫秒；BLOB 一律固定宽（device_id 8B / 长期公钥 32B / session 16B）

PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS device_identity (
  device_id     BLOB PRIMARY KEY,   -- 8B
  long_term_pk  BLOB NOT NULL,      -- 32B X25519 公钥
  fingerprint   TEXT NOT NULL,      -- 16 hex（4.6.3）
  device_name   TEXT NOT NULL,
  os            INTEGER NOT NULL,   -- tlv.rs STATE_OS 常量
  version       TEXT NOT NULL,
  created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pair_records (
  peer_id          BLOB PRIMARY KEY, -- 8B 对端 device_id
  peer_fingerprint TEXT NOT NULL,    -- 16 hex（TOFU 持久化，4.6.3）
  peer_name        TEXT NOT NULL,
  pair_count       INTEGER NOT NULL DEFAULT 1,
  paired_at        INTEGER NOT NULL,
  last_pair_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  session_id     BLOB PRIMARY KEY,  -- 16B
  peer_id        BLOB NOT NULL,
  created_at     INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL,
  pair_count     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS config (
  config_key TEXT PRIMARY KEY,
  value      BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  sync_scope TEXT NOT NULL          -- local | cross | per_peer（3.9）
);

CREATE TABLE IF NOT EXISTS message_log (
  msg_id      BLOB NOT NULL,
  msg_type    INTEGER NOT NULL,
  payload_ref TEXT,                 -- 仅 debug build（4.7 环形日志）
  ts          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_message_log_ts ON message_log(ts);