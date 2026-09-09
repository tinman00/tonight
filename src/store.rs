//! SQLite 本地存储（design.md §7.2 store）。
//! Connection 包在 Mutex 里（rusqlite 非 Sync，包一层让 &Store 可跨线程/跨 await 共享）；
//! 按请求独立开连接的用法不变。数据文件默认在 data/（已 gitignore）。

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::models::{Achievement, AchievementCategory, AppDetail, OwnedGame, Tag};
use crate::steam_local::InstalledApp;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS owned_games (
    app_id              INTEGER PRIMARY KEY,
    name                TEXT NOT NULL,
    playtime_min        INTEGER NOT NULL,
    playtime_2weeks_min INTEGER NOT NULL DEFAULT 0,
    last_played         INTEGER,                -- unix 秒；从未玩过为 NULL
    updated_at          INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS achievements (
    app_id      INTEGER NOT NULL,
    api_name    TEXT NOT NULL,
    achieved    INTEGER NOT NULL,
    unlock_time INTEGER,
    PRIMARY KEY (app_id, api_name)
);
CREATE TABLE IF NOT EXISTS install_state (
    app_id       INTEGER PRIMARY KEY,
    size_bytes   INTEGER NOT NULL,
    needs_update INTEGER NOT NULL,
    update_bytes INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS skipped_apps (
    app_id     INTEGER PRIMARY KEY,
    reason     TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS global_achievements (      -- Tier C：全球完成度（= 难度信号）
    app_id  INTEGER NOT NULL,
    api_name TEXT NOT NULL,
    percent REAL NOT NULL,                             -- 0–100
    PRIMARY KEY (app_id, api_name)
);
CREATE TABLE IF NOT EXISTS app_details (              -- Tier D：商店详情（长期缓存）
    app_id     INTEGER PRIMARY KEY,
    app_type   TEXT NOT NULL,                          -- game / dlc / application ...
    genres     TEXT NOT NULL,                          -- JSON: [{id,description}]
    categories TEXT NOT NULL,                          -- JSON: [{id,description}]
    storage_gb REAL,                                   -- 存储空间需求（商店页解析，NULL=未知）
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS achievement_categories (   -- C4：成就类型（LLM 分析，永久缓存）
    app_id   INTEGER NOT NULL,
    api_name TEXT NOT NULL,
    category TEXT NOT NULL,
    PRIMARY KEY (app_id, api_name)
);
CREATE TABLE IF NOT EXISTS annotations (              -- 修正层（design.md §7.4，永远覆盖自动层）
    app_id     INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL,                          -- idle_mark / taste_exclude / taste_note
    status     TEXT NOT NULL,                          -- proposed / confirmed / rejected / user_added
    note       TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_log (                -- R6：LLM 用量记账（原始值全量落库）
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                INTEGER NOT NULL,
    purpose           TEXT NOT NULL,                   -- ach_type_analysis / intent / cards ...
    model             TEXT NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    cost_cny          REAL,                            -- NULL = 价格未配置，仅计 token
    note              TEXT
);
CREATE TABLE IF NOT EXISTS store_tags (               -- 商店页用户投票标签（一次性长期缓存）
    app_id     INTEGER PRIMARY KEY,
    tags       TEXT NOT NULL,                          -- JSON 数组（中文，热度序）
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS game_position_llm (        -- 可选：LLM 游戏定位（一游戏一次，永久缓存）
    app_id     INTEGER PRIMARY KEY,
    achiever   REAL NOT NULL,
    explorer   REAL NOT NULL,
    killer     REAL NOT NULL,
    socializer REAL NOT NULL,
    evidence   TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS feedback_events (          -- P1-d：行为反馈事件（只影响运行时评分，不回写基础画像）
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id     INTEGER NOT NULL,
    kind       TEXT NOT NULL,                          -- impression/launch/dismiss/skip/download
    ts         INTEGER NOT NULL,
    session_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_feedback_app ON feedback_events(app_id, ts);
CREATE TABLE IF NOT EXISTS manual_overrides (        -- 手动覆盖层（§7.4 修正层家族，永远覆盖自动判定）
    app_id     INTEGER NOT NULL,
    kind       TEXT NOT NULL,                          -- game(强制视为游戏) / depth(手动深度档)
    value      TEXT NOT NULL,                          -- force / 深度中文档位（auto=删除由应用层处理）
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, kind)
);
"#;

pub struct Store {
    conn: Mutex<Connection>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok();
        conn.execute_batch(SCHEMA)?;
        // 增量迁移：旧库补 storage_gb 列（新库由 SCHEMA 直接建）
        conn.execute_batch("ALTER TABLE app_details ADD COLUMN storage_gb REAL").ok();
        // 增量迁移：深度档"长期在线"→"暂离"、"已通关"→"已完成"（v0.35/0.36 更名，存量标注直接改写）
        conn.execute_batch(
            "UPDATE manual_overrides SET value = '暂离' WHERE kind = 'depth' AND value = '长期在线';
             UPDATE manual_overrides SET value = '已完成' WHERE kind = 'depth' AND value = '已通关'",
        )
        .ok();
        Ok(Store { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    pub fn upsert_owned_games(&self, games: &[OwnedGame]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "INSERT INTO owned_games (app_id, name, playtime_min, playtime_2weeks_min, last_played, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(app_id) DO UPDATE SET
                name = excluded.name,
                playtime_min = excluded.playtime_min,
                playtime_2weeks_min = excluded.playtime_2weeks_min,
                last_played = excluded.last_played,
                updated_at = excluded.updated_at",
        )?;
        let ts = now();
        for g in games {
            stmt.execute(params![
                g.app_id,
                g.name,
                g.playtime_min,
                g.playtime_2weeks_min,
                g.last_played.map(|t| t as i64),
                ts
            ])?;
        }
        Ok(())
    }

    /// 有时长的游戏（Tier B 的拉取范围），按时长降序。
    pub fn played_games(&self) -> rusqlite::Result<Vec<OwnedGame>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT app_id, name, playtime_min, playtime_2weeks_min, last_played
             FROM owned_games WHERE playtime_min > 0 ORDER BY playtime_min DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(OwnedGame {
                app_id: r.get(0)?,
                name: r.get(1)?,
                playtime_min: r.get(2)?,
                playtime_2weeks_min: r.get(3)?,
                last_played: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
            })
        })?;
        rows.collect()
    }

    #[allow(dead_code)] // profile/recommend 子命令与测试使用
    pub fn owned_count(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM owned_games", [], |r| r.get(0))
    }

    pub fn upsert_achievements(&self, app_id: u32, achs: &[Achievement]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        // 成就集合可能变化（官方新增成就），整组替换最简单可靠
        conn.execute("DELETE FROM achievements WHERE app_id = ?1", params![app_id])?;
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO achievements (app_id, api_name, achieved, unlock_time)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for a in achs {
            stmt.execute(params![
                app_id,
                a.api_name,
                a.achieved as i32,
                a.unlock_time.map(|t| t as i64)
            ])?;
        }
        Ok(())
    }

    /// 全量替换本地安装状态（E 层每次启动刷新）。
    pub fn replace_install_state(&self, apps: &[InstalledApp]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM install_state", [])?;
        let mut stmt = conn.prepare_cached(
            "INSERT INTO install_state (app_id, size_bytes, needs_update, update_bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let ts = now();
        for a in apps {
            stmt.execute(params![
                a.app_id,
                a.size_bytes as i64,
                a.needs_update as i32,
                a.update_bytes as i64,
                ts
            ])?;
        }
        Ok(())
    }

    pub fn mark_skipped(&self, app_id: u32, reason: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO skipped_apps (app_id, reason, updated_at) VALUES (?1, ?2, ?3)",
            params![app_id, reason, now()],
        )?;
        Ok(())
    }

    pub fn skipped_apps(&self) -> rusqlite::Result<Vec<(u32, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT app_id, reason FROM skipped_apps")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

    pub fn meta_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(v) => Ok(Some(v?)),
            None => Ok(None),
        }
    }

    // ===== Tier C：全球成就完成度 =====

    pub fn upsert_global_achievements(&self, app_id: u32, rows: &[(String, f32)]) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO global_achievements (app_id, api_name, percent) VALUES (?1, ?2, ?3)",
        )?;
        for (name, percent) in rows {
            stmt.execute(params![app_id, name, percent])?;
        }
        Ok(())
    }

    pub fn global_achievements(&self, app_id: u32) -> rusqlite::Result<Vec<(String, f32)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT api_name, percent FROM global_achievements WHERE app_id = ?1")?;
        let rows = stmt.query_map(params![app_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f32>(1)?))
        })?;
        rows.collect()
    }

    pub fn global_count(&self, app_id: u32) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM global_achievements WHERE app_id = ?1",
            params![app_id],
            |r| r.get(0),
        )
    }

    /// 玩过、有成就、但还没有全球完成度数据的游戏（Tier C 待拉清单）。
    pub fn played_without_global(&self) -> rusqlite::Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT o.app_id FROM owned_games o
             WHERE o.playtime_min > 0
               AND EXISTS(SELECT 1 FROM achievements a WHERE a.app_id = o.app_id)
               AND NOT EXISTS(SELECT 1 FROM global_achievements g WHERE g.app_id = o.app_id)
             ORDER BY o.app_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?;
        rows.collect()
    }

    /// 全库尚无全球成就数据的游戏（含未开封候选——候选侧定位需要；跳过已知无成就的应用）。
    pub fn owned_without_global(&self) -> rusqlite::Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT app_id FROM owned_games
             WHERE NOT EXISTS(SELECT 1 FROM global_achievements g WHERE g.app_id = owned_games.app_id)
               AND app_id NOT IN (SELECT app_id FROM skipped_apps)
             ORDER BY app_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?;
        rows.collect()
    }

    // ===== Tier D：商店详情 =====

    pub fn upsert_app_detail(&self, app_id: u32, d: &AppDetail) -> rusqlite::Result<()> {
        let genres = serde_json::to_string(
            &d.genres.iter().map(|t| (t.id, &t.description)).collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let categories = serde_json::to_string(
            &d.categories.iter().map(|t| (t.id, &t.description)).collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO app_details (app_id, app_type, genres, categories, storage_gb, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![app_id, d.app_type, genres, categories, d.storage_gb, now()],
        )?;
        Ok(())
    }

    pub fn app_detail(&self, app_id: u32) -> rusqlite::Result<Option<AppDetail>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT app_type, genres, categories, storage_gb FROM app_details WHERE app_id = ?1",
            params![app_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                ))
            },
        );
        let Ok((app_type, genres, categories, storage_gb)) = row else {
            return Ok(None);
        };
        let parse_tags = |json: &str| -> Vec<Tag> {
            serde_json::from_str::<Vec<(i64, String)>>(json)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, description)| Tag { id, description })
                .collect()
        };
        Ok(Some(AppDetail {
            app_type,
            genres: parse_tags(&genres),
            categories: parse_tags(&categories),
            storage_gb,
        }))
    }

    /// 尚无商店详情的库内游戏（Tier D 待拉清单）。
    pub fn owned_without_details(&self) -> rusqlite::Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT app_id FROM owned_games WHERE app_id NOT IN (SELECT app_id FROM app_details) ORDER BY app_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?;
        rows.collect()
    }

    /// 玩过且有成就的游戏（成就类型分析范围，Tier F）。
    pub fn played_with_achievements(&self) -> rusqlite::Result<Vec<OwnedGame>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT o.app_id, o.name, o.playtime_min, o.playtime_2weeks_min, o.last_played
             FROM owned_games o
             WHERE o.playtime_min > 0
               AND EXISTS(SELECT 1 FROM achievements a WHERE a.app_id = o.app_id)
             ORDER BY o.playtime_min DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(OwnedGame {
                app_id: r.get(0)?,
                name: r.get(1)?,
                playtime_min: r.get(2)?,
                playtime_2weeks_min: r.get(3)?,
                last_played: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
            })
        })?;
        rows.collect()
    }

    // ===== C4：成就类型 =====

    pub fn achievement_count(&self, app_id: u32) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM achievements WHERE app_id = ?1",
            params![app_id],
            |r| r.get(0),
        )
    }

    pub fn categorized_count(&self, app_id: u32) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM achievement_categories WHERE app_id = ?1",
            params![app_id],
            |r| r.get(0),
        )
    }

    pub fn upsert_achievement_categories(
        &self,
        app_id: u32,
        rows: &[(String, AchievementCategory)],
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO achievement_categories (app_id, api_name, category) VALUES (?1, ?2, ?3)",
        )?;
        for (name, cat) in rows {
            stmt.execute(params![app_id, name, cat.as_str()])?;
        }
        Ok(())
    }

    pub fn achievement_categories(&self, app_id: u32) -> rusqlite::Result<Vec<(String, AchievementCategory)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT api_name, category FROM achievement_categories WHERE app_id = ?1",
        )?;
        let rows = stmt.query_map(params![app_id], |r| {
            let name: String = r.get(0)?;
            let cat: String = r.get(1)?;
            Ok((name, AchievementCategory::parse(&cat).unwrap_or(AchievementCategory::Other)))
        })?;
        rows.collect()
    }

    /// 清空全部成就分类（分类规则升级时失效重分析用）。
    pub fn clear_achievement_categories(&self) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM achievement_categories", []).map_err(|e| e)
    }

    // ===== 玩家成就读取（画像用） =====

    pub fn achievements(&self, app_id: u32) -> rusqlite::Result<Vec<Achievement>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT api_name, achieved, unlock_time FROM achievements WHERE app_id = ?1",
        )?;
        let rows = stmt.query_map(params![app_id], |r| {
            Ok(Achievement {
                api_name: r.get(0)?,
                achieved: r.get::<_, i32>(1)? != 0,
                unlock_time: r.get::<_, Option<i64>>(2)?.map(|t| t as u64),
            })
        })?;
        rows.collect()
    }

    /// 全部库内游戏（含未玩），按 app_id 升序。
    pub fn all_owned_games(&self) -> rusqlite::Result<Vec<OwnedGame>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT app_id, name, playtime_min, playtime_2weeks_min, last_played FROM owned_games ORDER BY app_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(OwnedGame {
                app_id: r.get(0)?,
                name: r.get(1)?,
                playtime_min: r.get(2)?,
                playtime_2weeks_min: r.get(3)?,
                last_played: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
            })
        })?;
        rows.collect()
    }

    /// 已安装应用：app_id → 是否需要更新。
    pub fn installed_map(&self) -> rusqlite::Result<std::collections::HashMap<u32, bool>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT app_id, needs_update FROM install_state")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i32>(1)? != 0))
        })?;
        rows.collect()
    }

    // ===== 修正层标注 =====

    /// 写入注水提议；若该应用已有 confirmed/rejected/user_added 标注则不覆盖（修正层优先）。
    pub fn propose_idle(&self, app_id: u32, note: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT status FROM annotations WHERE app_id = ?1",
                params![app_id],
                |r| r.get(0),
            )
            .ok();
        match existing.as_deref() {
            Some("confirmed") | Some("rejected") | Some("user_added") => {}
            _ => {
                conn.execute(
                    "INSERT OR REPLACE INTO annotations (app_id, kind, status, note, updated_at)
                     VALUES (?1, 'idle_mark', 'proposed', ?2, ?3)",
                    params![app_id, note, now()],
                )?;
            }
        }
        Ok(())
    }

    pub fn set_annotation_status(&self, app_id: u32, status: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE annotations SET status = ?2, updated_at = ?3 WHERE app_id = ?1",
            params![app_id, status, now()],
        )?;
        Ok(())
    }

    /// 手动标注注水（库存页入口）：confirmed=确认注水 / rejected=未注水。
    /// 与 set_annotation_status 的区别：机器从未提议过的游戏也能直接标（upsert，note 记来源）。
    pub fn set_idle_mark(&self, app_id: u32, status: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO annotations (app_id, kind, status, note, updated_at)
             VALUES (?1, 'idle_mark', ?2, '手动标注', ?3)
             ON CONFLICT(app_id) DO UPDATE SET
               status = excluded.status,
               note = CASE WHEN annotations.note IS NULL OR annotations.note = ''
                           THEN '手动标注' ELSE annotations.note END,
               updated_at = excluded.updated_at",
            params![app_id, status, now()],
        )?;
        Ok(())
    }

    /// 写入口味排除标注（按标签内容去重，修正层）。
    pub fn propose_taste_exclude(&self, tag: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO annotations (app_id, kind, status, note, updated_at)
             SELECT 0, 'taste_exclude', 'user_added', ?1, ?2
             WHERE NOT EXISTS (SELECT 1 FROM annotations WHERE kind='taste_exclude' AND note = ?1 AND status != 'rejected')",
            params![tag, now()],
        )?;
        Ok(())
    }

    /// 获取当前生效的口味排除标签列表（修正层）。
    pub fn taste_excludes(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT note FROM annotations WHERE kind = 'taste_exclude' AND status = 'user_added'",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// 撤销口味排除（按标签内容）。
    pub fn revoke_taste_exclude(&self, tag: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE annotations SET status = 'rejected', updated_at = ?2 WHERE kind = 'taste_exclude' AND note = ?1",
            params![tag, now()],
        )?;
        Ok(())
    }

    /// 修正层全量：app_id → (kind, status, note)
    pub fn annotations(&self) -> rusqlite::Result<Vec<(u32, String, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT app_id, kind, status, note FROM annotations")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect()
    }

    // ===== 商店用户标签 / LLM 游戏定位（三源定位） =====

    pub fn upsert_store_tags(&self, app_id: u32, tags: &[String]) -> rusqlite::Result<()> {
        let json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO store_tags (app_id, tags, updated_at) VALUES (?1, ?2, ?3)",
            params![app_id, json, now()],
        )?;
        Ok(())
    }

    pub fn store_tags(&self, app_id: u32) -> rusqlite::Result<Option<Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let Ok(json) = conn.query_row(
            "SELECT tags FROM store_tags WHERE app_id = ?1",
            params![app_id],
            |r| r.get::<_, String>(0),
        ) else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&json).unwrap_or_default()))
    }

    /// 尚未抓取用户标签的游戏（排除已知非游戏，避免给工具软件发请求）。
    pub fn owned_without_tags(&self) -> rusqlite::Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT app_id FROM owned_games
             WHERE app_id NOT IN (SELECT app_id FROM store_tags)
               AND app_id NOT IN (SELECT app_id FROM app_details WHERE app_type != 'game')
             ORDER BY app_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?;
        rows.collect()
    }

    /// 已有标签但缺存储信息的游戏（预约下载回填用：只对这些重抓商店页）。
    pub fn owned_without_storage(&self) -> rusqlite::Result<Vec<u32>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT o.app_id FROM owned_games o
             WHERE EXISTS (SELECT 1 FROM store_tags t WHERE t.app_id = o.app_id AND t.tags != '[]')
               AND NOT EXISTS (SELECT 1 FROM app_details d WHERE d.app_id = o.app_id AND d.storage_gb IS NOT NULL)
               AND NOT EXISTS (SELECT 1 FROM app_details d2 WHERE d2.app_id = o.app_id AND d2.app_type != 'game')
             ORDER BY o.app_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u32))?;
        rows.collect()
    }

    pub fn upsert_game_position(
        &self,
        app_id: u32,
        axes: (f64, f64, f64, f64),
        evidence: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO game_position_llm
             (app_id, achiever, explorer, killer, socializer, evidence, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![app_id, axes.0, axes.1, axes.2, axes.3, evidence, now()],
        )?;
        Ok(())
    }

    /// LLM 定位缓存：(四维, 证据)。
    pub fn game_position(&self, app_id: u32) -> rusqlite::Result<Option<([f64; 4], String)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT achiever, explorer, killer, socializer, evidence FROM game_position_llm WHERE app_id = ?1",
            params![app_id],
            |r| {
                Ok((
                    [
                        r.get::<_, f64>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, f64>(3)?,
                    ],
                    r.get::<_, String>(4)?,
                ))
            },
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ===== R6：用量记账 =====

    pub fn record_usage(
        &self,
        purpose: &str,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_cny: Option<f64>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO usage_log (ts, purpose, model, prompt_tokens, completion_tokens, cost_cny)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now(), purpose, model, prompt_tokens as i64, completion_tokens as i64, cost_cny],
        )?;
        Ok(())
    }

    /// (调用次数, 输入 tokens, 输出 tokens, 累计费用)；费用任一次未配置则为 None
    pub fn usage_summary(&self) -> rusqlite::Result<(i64, i64, i64, Option<f64>)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0),
                    (SELECT CASE WHEN COUNT(*) = SUM(cost_cny IS NOT NULL) THEN SUM(cost_cny) ELSE NULL END FROM usage_log)
             FROM usage_log",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, Option<f64>>(3)?,
                ))
            },
        )
    }

    /// 今日（北京时间自然日）已消耗的 LLM 费用（R6 预算硬停用）。
    pub fn today_usage_cny(&self) -> rusqlite::Result<f64> {
        let now = now();
        // 北京时间日界：UTC+8 的自然日起点
        let day_start = (now + 8 * 3600) / 86_400 * 86_400 - 8 * 3600;
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(cost_cny), 0.0) FROM usage_log WHERE ts >= ?1 AND cost_cny IS NOT NULL",
            params![day_start],
            |r| r.get(0),
        )
    }

    /// 价格未知的调用数（模型不在内置表且未配置价格）：这些调用只计 token 不计费，
    /// 界面明示数量避免"今日 ¥0"被误读为完全免费。
    pub fn unpriced_call_count(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM usage_log WHERE cost_cny IS NULL",
            [],
            |r| r.get(0),
        )
    }

    // ===== P1-d：行为反馈（事件层 + 疲劳 + 即玩自适应，§7.9②）=====

    /// 记录行为事件。impression 每卡每会话只记一次（轮播中心位去重）。
    pub fn record_feedback(&self, app_id: u32, kind: &str, session_id: &str) -> rusqlite::Result<()> {
        if !matches!(kind, "impression" | "launch" | "dismiss" | "skip" | "download") {
            return Err(rusqlite::Error::InvalidParameterName(kind.to_string()));
        }
        let conn = self.conn.lock().unwrap();
        if kind == "impression" {
            let seen: i64 = conn.query_row(
                "SELECT COUNT(*) FROM feedback_events WHERE app_id = ?1 AND kind = 'impression' AND session_id = ?2",
                params![app_id, session_id],
                |r| r.get(0),
            )?;
            if seen > 0 {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO feedback_events (app_id, kind, ts, session_id) VALUES (?1, ?2, ?3, ?4)",
            params![app_id, kind, now(), session_id],
        )?;
        Ok(())
    }

    /// 最近一次 launch 事件时间（探索模式的换批计数在启动后清零）。
    pub fn last_launch_ts(&self) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT MAX(ts) FROM feedback_events WHERE kind = 'launch'")?;
        let mut rows = stmt.query_map([], |r| r.get::<_, Option<i64>>(0))?;
        match rows.next() {
            Some(v) => Ok(v?),
            None => Ok(None),
        }
    }

    /// 每款游戏的疲劳状态（近 14 天窗口；launch 之后产生的 skip/dismiss 才计数 = "启动清零"）。
    /// penalty = −min(0.20, 0.06×N)；launch 一周内 boost = +0.05。
    pub fn fatigue_map(&self) -> rusqlite::Result<std::collections::HashMap<u32, (f64, f64)>> {
        let t = now();
        let cutoff = t - 14 * 86_400;
        let conn = self.conn.lock().unwrap();
        // 窗口内事件
        let events: Vec<(u32, String, i64)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT app_id, kind, ts FROM feedback_events WHERE ts >= ?1 AND kind IN ('skip','dismiss','launch')",
            )?;
            let rows = stmt.query_map(params![cutoff], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        // 窗口外的历史 launch 也要清零作用（老启动仍早于窗口内全部 skip）
        let last_launch_all: std::collections::HashMap<u32, i64> = {
            let mut stmt = conn.prepare_cached(
                "SELECT app_id, MAX(ts) FROM feedback_events WHERE kind = 'launch' GROUP BY app_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)?)))?;
            rows.collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?
        };
        let mut neg_since_launch: std::collections::HashMap<u32, i64> = std::collections::HashMap::new();
        for (app_id, kind, ts) in &events {
            if (kind == "skip" || kind == "dismiss") && ts > &last_launch_all.get(app_id).copied().unwrap_or(0) {
                *neg_since_launch.entry(*app_id).or_insert(0) += 1;
            }
        }
        let mut out = std::collections::HashMap::new();
        let mut apps: std::collections::HashSet<u32> = last_launch_all.keys().copied().collect();
        apps.extend(neg_since_launch.keys().copied());
        for app_id in apps {
            let n = neg_since_launch.get(&app_id).copied().unwrap_or(0);
            let penalty = -(0.06 * n as f64).min(0.20);
            let boost = match last_launch_all.get(&app_id) {
                Some(ts) if t - ts <= 7 * 86_400 => 0.05,
                _ => 0.0,
            };
            out.insert(app_id, (penalty, boost));
        }
        Ok(out)
    }

    /// 事件计数（画像页行为反馈区）。
    pub fn feedback_event_counts(&self) -> rusqlite::Result<std::collections::BTreeMap<String, i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT kind, COUNT(*) FROM feedback_events GROUP BY kind",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect()
    }

    /// 重置行为学习：清空事件表，install_affinity 回到 0.5。
    pub fn reset_feedback(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM feedback_events", [])?;
        drop(conn);
        self.meta_set("install_affinity", "0.5")
    }

    /// 即玩自适应系数（0–1，起始 0.5）：启动已安装 +0.15、"没想法"信号 −0.2。
    pub fn install_affinity(&self) -> f64 {
        self.meta_get("install_affinity")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.5)
            .clamp(0.0, 1.0)
    }

    pub fn bump_install_affinity(&self, delta: f64) -> rusqlite::Result<f64> {
        let v = (self.install_affinity() + delta).clamp(0.0, 1.0);
        self.meta_set("install_affinity", &format!("{v:.3}"))?;
        Ok(v)
    }

    // ===== 手动覆盖层（§7.4 修正层家族：强制视为游戏 / 手动深度档）=====

    /// 写入覆盖（INSERT OR REPLACE）；value 为空或 "auto" = 清除该覆盖（恢复自动判定）。
    pub fn set_override(&self, app_id: u32, kind: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("auto") {
            conn.execute(
                "DELETE FROM manual_overrides WHERE app_id = ?1 AND kind = ?2",
                params![app_id, kind],
            )?;
        } else {
            conn.execute(
                "INSERT OR REPLACE INTO manual_overrides (app_id, kind, value, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![app_id, kind, value.trim(), now()],
            )?;
        }
        Ok(())
    }

    /// 全量覆盖：(app_id, kind, value)。
    pub fn overrides(&self) -> rusqlite::Result<Vec<(u32, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT app_id, kind, value FROM manual_overrides")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_games_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_owned_games(&[OwnedGame {
                app_id: 105600,
                name: "Terraria".into(),
                playtime_min: 147 * 60,
                playtime_2weeks_min: 26 * 60,
                last_played: Some(1_750_000_000),
            }])
            .unwrap();
        let played = store.played_games().unwrap();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].name, "Terraria");
        assert_eq!(played[0].last_played, Some(1_750_000_000));

        // 零时长游戏不出现在 played 列表
        store
            .upsert_owned_games(&[OwnedGame {
                app_id: 1,
                name: "Never".into(),
                playtime_min: 0,
                playtime_2weeks_min: 0,
                last_played: None,
            }])
            .unwrap();
        assert_eq!(store.owned_count().unwrap(), 2);
        assert_eq!(store.played_games().unwrap().len(), 1);
    }

    #[test]
    fn achievements_replace() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_achievements(
                1,
                &[
                    Achievement { api_name: "A".into(), achieved: true, unlock_time: Some(5) },
                    Achievement { api_name: "B".into(), achieved: false, unlock_time: None },
                ],
            )
            .unwrap();
        // 第二次整组替换后只保留新的成就集合
        store
            .upsert_achievements(
                1,
                &[Achievement { api_name: "A".into(), achieved: true, unlock_time: Some(6) }],
            )
            .unwrap();
        let n: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM achievements WHERE app_id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn meta_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.meta_get("steamid").unwrap().is_none());
        store.meta_set("steamid", "76561198000000000").unwrap();
        assert_eq!(store.meta_get("steamid").unwrap().as_deref(), Some("76561198000000000"));
    }

    // ===== P1-d：行为反馈 =====

    /// 直接插入带指定时间戳的事件（绕过 record_feedback 的 now()，测试疲劳窗口）
    fn insert_event(store: &Store, app_id: u32, kind: &str, ts: i64) {
        store
            .conn()
            .execute(
                "INSERT INTO feedback_events (app_id, kind, ts, session_id) VALUES (?1, ?2, ?3, 't')",
                params![app_id, kind, ts],
            )
            .unwrap();
    }

    #[test]
    fn impression_deduped_per_session() {
        let store = Store::open_in_memory().unwrap();
        store.record_feedback(1, "impression", "s1").unwrap();
        store.record_feedback(1, "impression", "s1").unwrap(); // 同会话去重
        store.record_feedback(1, "impression", "s2").unwrap(); // 跨会话保留
        store.record_feedback(1, "launch", "s1").unwrap(); // 非 impression 不去重
        let counts = store.feedback_event_counts().unwrap();
        assert_eq!(counts.get("impression"), Some(&2));
        assert_eq!(counts.get("launch"), Some(&1));
        assert!(store.record_feedback(1, "hack", "s1").is_err()); // 非法 kind 拒绝
    }

    #[test]
    fn fatigue_penalty_launch_clears_and_boosts() {
        let store = Store::open_in_memory().unwrap();
        let now = now();
        // 3 次 skip → −0.18
        for _ in 0..3 {
            insert_event(&store, 10, "skip", now - 86_400);
        }
        // 5 次 dismiss → 封顶 −0.20
        for _ in 0..5 {
            insert_event(&store, 20, "dismiss", now - 86_400);
        }
        let map = store.fatigue_map().unwrap();
        assert!((map[&10].0 - (-0.18)).abs() < 1e-9);
        assert!((map[&20].0 - (-0.20)).abs() < 1e-9);

        // 游戏 10 启动：清零（老 skip 不再计）+ 一周内 +0.05
        insert_event(&store, 10, "launch", now - 3600);
        let map = store.fatigue_map().unwrap();
        assert!((map[&10].0 - 0.0).abs() < 1e-9);
        assert!((map[&10].1 - 0.05).abs() < 1e-9);

        // 启动之后的 skip 重新计数（不吃清零）
        insert_event(&store, 10, "skip", now - 1800);
        let map = store.fatigue_map().unwrap();
        assert!((map[&10].0 - (-0.06)).abs() < 1e-9);

        // 8 天前的 launch：清零但不 boost
        insert_event(&store, 20, "launch", now - 8 * 86_400);
        let map = store.fatigue_map().unwrap();
        assert!((map[&20].1 - 0.0).abs() < 1e-9);
    }

    #[test]
    fn affinity_clamped_and_reset() {
        let store = Store::open_in_memory().unwrap();
        assert!((store.install_affinity() - 0.5).abs() < 1e-9); // 起始 0.5
        assert!((store.bump_install_affinity(0.15).unwrap() - 0.65).abs() < 1e-9);
        assert!((store.bump_install_affinity(5.0).unwrap() - 1.0).abs() < 1e-9); // 上限
        assert!((store.bump_install_affinity(-0.2).unwrap() - 0.8).abs() < 1e-9);
        insert_event(&store, 1, "skip", now());
        store.reset_feedback().unwrap();
        assert!((store.install_affinity() - 0.5).abs() < 1e-9);
        assert_eq!(store.feedback_event_counts().unwrap().len(), 0); // 事件清空
    }
}
