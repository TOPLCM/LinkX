//! LinkX 本地存储
//!
//! - 单一源 schema：`Crates/storage/schema.sql`，build 时嵌入并执行
//! - 模型：device_identity / pair_records(TOFU) / sessions / config / message_log

use std::path::Path;

use debuglog::Level;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite 打开/初始化失败: {0}")]
    Open(String),
    #[error("SQL 执行失败: {0}")]
    Sql(String),
    #[error("记录不存在: {0}")]
    NotFound(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e.to_string())
    }
}

pub const SCHEMA_SQL: &str = include_str!("../schema.sql");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub device_id: Vec<u8>,
    pub long_term_pk: Vec<u8>,
    pub fingerprint: String,
    pub device_name: String,
    pub os: u8,
    pub version: String,
    pub created_at: i64,
}

/// 已绑定设备记录（设备生命周期：绑定列表 / 解绑）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRecord {
    pub peer_id: Vec<u8>,
    pub peer_fingerprint: String,
    pub peer_name: String,
    pub pair_count: i64,
    pub paired_at: i64,
    pub last_pair_at: i64,
}

/// SQLite 存储（thin DAO，双端共用；Windows 文件库/Android 由 Room 镜像同一 schema）
pub struct LinkxStore {
    conn: Connection,
}

impl LinkxStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self { conn };
        store.init()?;
        debuglog::log!(Level::Info, "storage", "db.open", &[("mode", "file")]);
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self { conn };
        store.init()?;
        debuglog::log!(Level::Info, "storage", "db.open", &[("mode", "memory")]);
        Ok(store)
    }

    fn init(&self) -> Result<(), StoreError> {
        match self.conn.execute_batch(SCHEMA_SQL) {
            Ok(()) => {
                // 埋点：schema 应用/迁移成功
                debuglog::log!(Level::Info, "storage", "db.migrate", &[("ok", "true")]);
                Ok(())
            }
            Err(e) => {
                // 埋点：schema 应用失败
                debuglog::log!(
                    Level::Warn,
                    "storage",
                    "db.migrate",
                    &[("ok", "false"), ("err", &e.to_string())]
                );
                Err(e.into())
            }
        }
    }

    // ---- device_identity ----

    pub fn upsert_device_identity(&self, d: &DeviceIdentity) -> Result<(), StoreError> {
        let rows = self.conn.execute(
            "INSERT OR REPLACE INTO device_identity
                 (device_id, long_term_pk, fingerprint, device_name, os, version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                d.device_id,
                d.long_term_pk,
                d.fingerprint,
                d.device_name,
                d.os as i64,
                d.version,
                d.created_at
            ],
        )?;
        // 埋点：DAO 写（只记行数与身份指纹 ID，不记业务明文）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.upsert_identity",
            &[("rows", &rows.to_string()), ("fp", &d.fingerprint)]
        );
        Ok(())
    }

    pub fn get_device_identity(&self) -> Result<Option<DeviceIdentity>, StoreError> {
        self.conn
            .query_row(
                // 身份轮换会写入新的 device_id，旧行仍在：不带 ORDER BY 的 LIMIT 1 让 SQLite
                // 随便挑一行，那等于随机决定"本机是谁"。按建号时间取最新的那条。
                "SELECT device_id, long_term_pk, fingerprint, device_name, os, version, created_at
                 FROM device_identity ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                |row| {
                    Ok(DeviceIdentity {
                        device_id: row.get(0)?,
                        long_term_pk: row.get(1)?,
                        fingerprint: row.get(2)?,
                        device_name: row.get(3)?,
                        os: row.get::<_, i64>(4)? as u8,
                        version: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    // ---- pair_records（TOFU） ----

    pub fn upsert_pair(
        &self,
        peer_id: &[u8],
        fingerprint: &str,
        peer_name: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO pair_records (peer_id, peer_fingerprint, peer_name, pair_count, paired_at, last_pair_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4)
             ON CONFLICT(peer_id) DO UPDATE SET
               peer_fingerprint=excluded.peer_fingerprint,
               peer_name=excluded.peer_name,
               pair_count=pair_count+1,
               last_pair_at=excluded.last_pair_at",
            params![peer_id, fingerprint, peer_name, now_ms],
        )?;
        // 埋点：配对记录写入（记行数与指纹 ID，不记设备名明文）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.upsert_pair",
            &[("rows", &rows.to_string()), ("fp", fingerprint)]
        );
        Ok(())
    }

    pub fn get_pair_fingerprint(&self, peer_id: &[u8]) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT peer_fingerprint FROM pair_records WHERE peer_id=?1",
                [peer_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_pair(&self, peer_id: &[u8]) -> Result<(), StoreError> {
        let rows = self
            .conn
            .execute("DELETE FROM pair_records WHERE peer_id=?1", [peer_id])?;
        // 埋点：配对记录删除
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.delete_pair",
            &[("rows", &rows.to_string())]
        );
        Ok(())
    }

    // ---- config（跨端配置） ----

    pub fn set_config(
        &self,
        key: &str,
        value: &[u8],
        scope: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO config (config_key, value, updated_at, sync_scope)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(config_key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at, sync_scope=excluded.sync_scope",
            params![key, value, now_ms, scope],
        )?;
        // 埋点：配置写入（记 config key 与行数，不记 value 明文）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.set_config",
            &[("rows", &rows.to_string()), ("key", key)]
        );
        Ok(())
    }

    pub fn get_config(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.conn
            .query_row(
                "SELECT value FROM config WHERE config_key=?1",
                [key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn count_pairs(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM pair_records", [], |r| {
                r.get::<_, i64>(0)
            })?)
    }

    // ---- 绑定设备列表 ----

    /// 已绑定设备列表（按最近配对时间倒序）
    pub fn list_pairs(&self) -> Result<Vec<PairRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT peer_id, peer_fingerprint, peer_name, pair_count, paired_at, last_pair_at
             FROM pair_records ORDER BY last_pair_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PairRecord {
                peer_id: row.get(0)?,
                peer_fingerprint: row.get(1)?,
                peer_name: row.get(2)?,
                pair_count: row.get(3)?,
                paired_at: row.get(4)?,
                last_pair_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 解绑设备：删除 TOFU 记录并清理其会话历史
    pub fn unbind_pair(&self, peer_id: &[u8]) -> Result<(), StoreError> {
        let pairs = self
            .conn
            .execute("DELETE FROM pair_records WHERE peer_id=?1", [peer_id])?;
        let sessions = self
            .conn
            .execute("DELETE FROM sessions WHERE peer_id=?1", [peer_id])?;
        // 埋点：解绑设备（连带清理会话历史）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.unbind_pair",
            &[
                ("pairs", &pairs.to_string()),
                ("sessions", &sessions.to_string()),
            ]
        );
        Ok(())
    }

    // ---- sessions ----

    /// 记录/更新会话（同一 session_id 幂等；`pair_count` 累加由调用方决定）
    pub fn upsert_session(
        &self,
        session_id: &[u8],
        peer_id: &[u8],
        now_ms: i64,
        pair_count: i64,
    ) -> Result<(), StoreError> {
        let rows = self.conn.execute(
            "INSERT INTO sessions (session_id, peer_id, created_at, last_active_at, pair_count)
             VALUES (?1, ?2, ?3, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
               last_active_at=excluded.last_active_at,
               pair_count=excluded.pair_count",
            params![session_id, peer_id, now_ms, pair_count],
        )?;
        // 埋点：会话记录写入（记行数与会话 ID 长度，不记内容）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.upsert_session",
            &[
                ("rows", &rows.to_string()),
                ("sid_len", &session_id.len().to_string()),
            ]
        );
        Ok(())
    }

    /// 会话最近活跃时间（返回 None = 无此会话）
    pub fn session_last_active(&self, session_id: &[u8]) -> Result<Option<i64>, StoreError> {
        self.conn
            .query_row(
                "SELECT last_active_at FROM sessions WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// 某设备的会话条数（「换设备恢复」判据）
    pub fn count_sessions_of_peer(&self, peer_id: &[u8]) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE peer_id=?1",
            [peer_id],
            |r| r.get::<_, i64>(0),
        )?)
    }

    // ---- message_log（可选、仅 debug build 的环形日志） ----

    /// 追加一条消息日志并裁剪到最近 `keep` 条（环形语义）
    pub fn append_message_log(
        &self,
        msg_id: &[u8],
        msg_type: i64,
        payload_ref: Option<&str>,
        ts: i64,
        keep: usize,
    ) -> Result<(), StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO message_log (msg_id, msg_type, payload_ref, ts) VALUES (?1, ?2, ?3, ?4)",
            params![msg_id, msg_type, payload_ref, ts],
        )?;
        let trimmed = self.conn.execute(
            "DELETE FROM message_log WHERE rowid NOT IN
               (SELECT rowid FROM message_log ORDER BY ts DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        // 埋点：消息日志环形裁剪（只记条数与 msg_type，不记 payload）
        debuglog::log!(
            Level::Info,
            "storage",
            "dao.append_log",
            &[
                ("inserted", &inserted.to_string()),
                ("trimmed", &trimmed.to_string()),
                ("msg_type", &msg_type.to_string()),
            ]
        );
        Ok(())
    }

    pub fn count_message_log(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM message_log", [], |r| {
                r.get::<_, i64>(0)
            })?)
    }

    /// 清空消息日志（隐私：用户可随时清除）
    pub fn clear_message_log(&self) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM message_log", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn schema_applies_and_roundtrip() {
        let store = LinkxStore::open_in_memory().unwrap();

        // TOFU：首次配对持久化指纹 → 二次握手可比对
        let peer_id = [0xAA; 8].to_vec();
        store
            .upsert_pair(&peer_id, "aabbccddeeff0011", "pc-home", now())
            .unwrap();
        assert_eq!(
            store.get_pair_fingerprint(&peer_id).unwrap().unwrap(),
            "aabbccddeeff0011"
        );
        // 更新指纹（重配对）
        store
            .upsert_pair(&peer_id, "1122334455667788", "pc-home", now())
            .unwrap();
        assert_eq!(
            store.get_pair_fingerprint(&peer_id).unwrap().unwrap(),
            "1122334455667788"
        );
        assert_eq!(store.count_pairs().unwrap(), 1);
        store.delete_pair(&peer_id).unwrap();
        assert_eq!(store.count_pairs().unwrap(), 0);
        assert!(store.get_pair_fingerprint(&peer_id).unwrap().is_none());
    }

    #[test]
    fn device_identity_persist() {
        let store = LinkxStore::open_in_memory().unwrap();
        assert!(store.get_device_identity().unwrap().is_none());
        let d = DeviceIdentity {
            device_id: [0x01; 8].to_vec(),
            long_term_pk: [0x77; 32].to_vec(),
            fingerprint: "deadbeefcafef00d".into(),
            device_name: "misha-phone".into(),
            os: 1,
            version: "0.1.0".into(),
            created_at: now(),
        };
        store.upsert_device_identity(&d).unwrap();
        let back = store.get_device_identity().unwrap().unwrap();
        assert_eq!(back, d);
    }

    /// 身份轮换写入的是**新的 device_id**，旧行不会被 REPLACE 掉。读取端必须确定性地拿到最新
    /// 那一份，而不是让 SQLite 随便挑一行 —— 那等于随机决定"本机是谁"。
    #[test]
    fn rotated_identity_reads_the_newest_row() {
        let store = LinkxStore::open_in_memory().unwrap();
        let old = DeviceIdentity {
            device_id: [0x01; 8].to_vec(),
            long_term_pk: [0x77; 32].to_vec(),
            fingerprint: "0000000000000001".into(),
            device_name: "pc-home".into(),
            os: 0,
            version: "0.5.0".into(),
            created_at: 1_000,
        };
        store.upsert_device_identity(&old).unwrap();
        let mut fresh = old.clone();
        fresh.device_id = [0x02; 8].to_vec();
        fresh.fingerprint = "0000000000000002".into();
        fresh.created_at = 2_000;
        store.upsert_device_identity(&fresh).unwrap();
        assert_eq!(
            store.get_device_identity().unwrap().unwrap(),
            fresh,
            "读到的应是轮换后的身份"
        );
        // 反序写入（新的先写、旧的后写）也必须读最新的那一条：判据是 created_at，不是插入顺序
        let store2 = LinkxStore::open_in_memory().unwrap();
        store2.upsert_device_identity(&fresh).unwrap();
        store2.upsert_device_identity(&old).unwrap();
        assert_eq!(store2.get_device_identity().unwrap().unwrap(), fresh);
    }

    #[test]
    fn config_set_get() {
        let store = LinkxStore::open_in_memory().unwrap();
        store
            .set_config("notify.blacklist", b"[\"com.example\"]", "cross", now())
            .unwrap();
        assert_eq!(
            store.get_config("notify.blacklist").unwrap().unwrap(),
            b"[\"com.example\"]"
        );
        assert!(store.get_config("nope").unwrap().is_none());
    }

    #[test]
    fn file_db_roundtrip() {
        let dir = std::env::temp_dir().join(format!("linkx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let store = LinkxStore::open(&path).unwrap();
        store
            .upsert_pair(&[0x0F; 8], "0011ff22aa44cc77", "win-pc", now())
            .unwrap();
        drop(store);
        // 重开验证持久化
        let store2 = LinkxStore::open(&path).unwrap();
        assert_eq!(
            store2.get_pair_fingerprint(&[0x0F; 8]).unwrap().unwrap(),
            "0011ff22aa44cc77"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pair_list_and_unbind() {
        let store = LinkxStore::open_in_memory().unwrap();
        let a = [0x01; 8].to_vec();
        let b = [0x02; 8].to_vec();
        store
            .upsert_pair(&a, "aaaa000011112222", "phone-a", 1000)
            .unwrap();
        store
            .upsert_pair(&b, "bbbb000011112222", "phone-b", 2000)
            .unwrap();
        let list = store.list_pairs().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].peer_name, "phone-b", "应按最近配对倒序");
        assert_eq!(list[0].peer_fingerprint, "bbbb000011112222");

        store.unbind_pair(&a).unwrap();
        let list = store.list_pairs().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].peer_id, b);
    }

    #[test]
    fn sessions_upsert_and_query() {
        let store = LinkxStore::open_in_memory().unwrap();
        let sid = [0x33; 16].to_vec();
        let peer = [0x44; 8].to_vec();
        store.upsert_session(&sid, &peer, 1000, 0).unwrap();
        assert_eq!(store.session_last_active(&sid).unwrap(), Some(1000));
        assert_eq!(store.count_sessions_of_peer(&peer).unwrap(), 1);
        // 幂等更新（同 session_id）
        store.upsert_session(&sid, &peer, 5000, 3).unwrap();
        assert_eq!(store.session_last_active(&sid).unwrap(), Some(5000));
        assert_eq!(store.count_sessions_of_peer(&peer).unwrap(), 1);
        // 解绑设备应连带清理其会话
        store.unbind_pair(&peer).unwrap();
        assert_eq!(store.count_sessions_of_peer(&peer).unwrap(), 0);
        assert!(store.session_last_active(&sid).unwrap().is_none());
    }

    #[test]
    fn message_log_ring_trim() {
        let store = LinkxStore::open_in_memory().unwrap();
        for i in 0..10i64 {
            store
                .append_message_log(&[i as u8; 16], 0x10, None, 1000 + i, 5)
                .unwrap();
        }
        assert_eq!(store.count_message_log().unwrap(), 5, "应只保留最近 5 条");
        store.clear_message_log().unwrap();
        assert_eq!(store.count_message_log().unwrap(), 0);
    }
}
