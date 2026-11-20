use crate::auth;
use crate::error::Error;
use crate::schedule::{self, Window};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub disabled: bool,
    pub perm_stations: bool,
    pub perm_mounts: bool,
    pub perm_playlists: bool,
    pub perm_library: bool,
    pub perm_import: bool,
    pub perm_recordings: bool,
    pub perm_schedules: bool,
    pub perm_bots: bool,
    pub perm_listeners: bool,
    pub perm_takeover: bool,
    /// `None` is unlimited. `Some(0)` means that thing is not allowed.
    pub max_stations: Option<i64>,
    pub max_mounts: Option<i64>,
    pub max_playlists: Option<i64>,
    pub max_tracks: Option<i64>,
    pub max_library_bytes: Option<i64>,
    pub max_bots: Option<i64>,
    pub max_recordings: Option<i64>,
    pub max_recording_bytes: Option<i64>,
}

impl User {
    /// The first admin is the owner. Older databases still say `admin`.
    pub fn is_owner(&self) -> bool {
        self.role == "owner" || self.role == "admin"
    }

    pub fn open_account() -> Self {
        Self {
            id: String::new(),
            username: String::new(),
            password_hash: String::new(),
            role: "user".into(),
            disabled: false,
            perm_stations: true,
            perm_mounts: true,
            perm_playlists: true,
            perm_library: true,
            perm_import: true,
            perm_recordings: true,
            perm_schedules: true,
            perm_bots: true,
            perm_listeners: true,
            perm_takeover: true,
            max_stations: None,
            max_mounts: None,
            max_playlists: None,
            max_tracks: None,
            max_library_bytes: None,
            max_bots: None,
            max_recordings: None,
            max_recording_bytes: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Usage {
    pub stations: i64,
    pub mounts: i64,
    pub playlists: i64,
    pub tracks: i64,
    pub library_bytes: i64,
    pub bots: i64,
    pub recordings: i64,
    pub recording_bytes: i64,
}

pub struct OwnedFiles {
    pub media_ids: Vec<String>,
    pub recordings: Vec<(String, String)>,
    pub mount_paths: Vec<String>,
    pub bot_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub genre: String,
    pub created_at: i64,
    /// Public Icecast and HLS. The mixer keeps running when this is off.
    pub radio_enabled: bool,
    pub user_id: String,
}

#[derive(Clone, Debug)]
pub struct Mount {
    pub id: String,
    pub station_id: String,
    pub path: String,
    pub source_user: String,
    pub source_password_hash: String,
    pub format: String,
    pub bitrate: i64,
    pub max_listeners: i64,
    pub max_bitrate: i64,
    pub allow_takeover: bool,
    pub fallback_playlist_id: Option<String>,
    pub created_at: i64,
    pub crossfade_ms: i64,
    pub autodj: bool,
    pub shuffle: bool,
    /// "off", "one", or "all".
    pub repeat: String,
    pub jingle_every: i64,
    pub jingle_media_id: Option<String>,
    pub intro_media_id: Option<String>,
    pub gain_db: f64,
    pub sample_rate: i64,
    pub channels: i64,
    pub buffer_ms: i64,
    pub hls_segment_ms: i64,
    /// `ffmpeg`, `native`, or `opus`.
    pub encoder: String,
    pub relay_url: String,
    /// Persisted. When true, the mount keeps writing the encoded stream to disk.
    pub record: bool,
    pub record_max_ms: i64,
}

#[derive(Clone, Debug)]
pub struct Recording {
    pub id: String,
    pub mount_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: i64,
    pub format: String,
    pub bytes: i64,
    pub filename: String,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct Media {
    pub id: String,
    pub station_id: String,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub title: String,
    pub artist: String,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub id: String,
    pub station_id: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct PlaylistItem {
    pub id: String,
    pub playlist_id: String,
    pub media_id: String,
    pub position: i64,
    pub title: String,
    pub artist: String,
    pub filename: String,
}

#[derive(Clone, Debug)]
pub struct Schedule {
    pub id: String,
    pub mount_id: String,
    pub playlist_id: String,
    pub days_mask: i64,
    pub start_min: i64,
    pub end_min: i64,
    pub priority: i64,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Audit {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

/// A TeamSpeak 3 bot. It is not owned by a station: `station_id` is the station
/// feeding it right now, or empty. Passwords and the identity stay in this row.
/// HTTP callers must not copy those fields out.
#[derive(Clone)]
pub struct Output {
    pub id: String,
    pub station_id: String,
    pub mount_id: String,
    pub host: String,
    pub voice_port: i64,
    pub server_password: String,
    pub nickname: String,
    pub phonetic: String,
    pub away: String,
    pub channel_password: String,
    pub default_channel_id: Option<i64>,
    pub identity: String,
    pub volume: i64,
    pub enabled: bool,
    pub auto_reconnect: bool,
    pub prefix: String,
    pub commands: String,
    pub gate: String,
    pub gate_group: String,
    pub repeat_mode: String,
    pub created_at: i64,
    pub user_id: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db")
    }

    pub fn count_users(&self) -> Result<i64, Error> {
        let c = self.lock();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        Ok(n)
    }

    pub fn create_user(&self, username: &str, password_hash: &str, role: &str) -> Result<User, Error> {
        let id = auth::new_id();
        let c = self.lock();
        c.execute(
            "INSERT INTO users (id, username, password_hash, role, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![id, username, password_hash, role, now_ms()],
        )?;
        let mut user = User::open_account();
        user.id = id;
        user.username = username.into();
        user.password_hash = password_hash.into();
        user.role = role.into();
        Ok(user)
    }

    pub fn user_by_name(&self, username: &str) -> Result<Option<User>, Error> {
        let c = self.lock();
        let sql = format!("SELECT {USER_COLS} FROM users WHERE username=?1");
        let row = c.query_row(&sql, params![username], map_user).optional()?;
        Ok(row)
    }

    pub fn user_by_id(&self, id: &str) -> Result<Option<User>, Error> {
        let c = self.lock();
        let sql = format!("SELECT {USER_COLS} FROM users WHERE id=?1");
        Ok(c.query_row(&sql, params![id], map_user).optional()?)
    }

    pub fn list_users(&self) -> Result<Vec<User>, Error> {
        let c = self.lock();
        let sql = format!("SELECT {USER_COLS} FROM users ORDER BY username");
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([], map_user)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_account(&self, user: &User) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE users SET disabled=?2,
             perm_stations=?3, perm_mounts=?4, perm_playlists=?5, perm_library=?6, perm_import=?7,
             perm_recordings=?8, perm_schedules=?9, perm_bots=?10, perm_listeners=?11, perm_takeover=?12,
             max_stations=?13, max_mounts=?14, max_playlists=?15, max_tracks=?16, max_library_bytes=?17,
             max_bots=?18, max_recordings=?19, max_recording_bytes=?20
             WHERE id=?1 AND role NOT IN ('owner','admin')",
            params![
                user.id, user.disabled as i64,
                user.perm_stations as i64, user.perm_mounts as i64, user.perm_playlists as i64,
                user.perm_library as i64, user.perm_import as i64, user.perm_recordings as i64,
                user.perm_schedules as i64, user.perm_bots as i64, user.perm_listeners as i64, user.perm_takeover as i64,
                user.max_stations, user.max_mounts, user.max_playlists, user.max_tracks, user.max_library_bytes,
                user.max_bots, user.max_recordings, user.max_recording_bytes,
            ],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn set_password_hash(&self, id: &str, password_hash: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("UPDATE users SET password_hash=?2 WHERE id=?1", params![id, password_hash])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE users SET disabled=?2 WHERE id=?1 AND role NOT IN ('owner','admin')",
            params![id, disabled as i64],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    /// Rows created before accounts existed belong to the first admin.
    pub fn claim_unowned(&self, user_id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("UPDATE stations SET user_id=?1 WHERE user_id=''", params![user_id])?;
        c.execute("UPDATE bots SET user_id=?1 WHERE user_id=''", params![user_id])?;
        Ok(())
    }

    pub fn usage(&self, user_id: &str) -> Result<Usage, Error> {
        let c = self.lock();
        let row = c.query_row(
            "SELECT
               (SELECT COUNT(*) FROM stations WHERE user_id=?1),
               (SELECT COUNT(*) FROM mounts m JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1),
               (SELECT COUNT(*) FROM playlists p JOIN stations s ON s.id=p.station_id WHERE s.user_id=?1),
               (SELECT COUNT(*) FROM media m JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1),
               (SELECT COALESCE(SUM(m.bytes),0) FROM media m JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1),
               (SELECT COUNT(*) FROM bots WHERE user_id=?1),
               (SELECT COUNT(*) FROM recordings r JOIN mounts m ON m.id=r.mount_id JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1),
               (SELECT COALESCE(SUM(r.bytes),0) FROM recordings r JOIN mounts m ON m.id=r.mount_id JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1)",
            params![user_id],
            |r| Ok(Usage {
                stations: r.get(0)?, mounts: r.get(1)?, playlists: r.get(2)?, tracks: r.get(3)?,
                library_bytes: r.get(4)?, bots: r.get(5)?, recordings: r.get(6)?, recording_bytes: r.get(7)?,
            }),
        )?;
        Ok(row)
    }

    /// Refuse a new library file when this account is at its track or byte cap.
    /// An empty owner id is unlimited, so older rows and the owner are not capped.
    pub fn allow_track(&self, user_id: &str, add_bytes: i64, add_count: i64) -> Result<(), Error> {
        if user_id.is_empty() { return Ok(()); }
        let user = self.user_by_id(user_id)?.unwrap_or_else(User::open_account);
        if user.is_owner() || user.id.is_empty() { return Ok(()); }
        let usage = self.usage(user_id)?;
        if add_count > 0 {
            if let Some(max) = user.max_tracks {
                if usage.tracks >= max || usage.tracks.saturating_add(add_count) > max {
                    return Err(Error::Forbidden(format!("library track limit reached: {} of {max}", usage.tracks)));
                }
            }
        }
        if let Some(max) = user.max_library_bytes {
            if usage.library_bytes >= max || usage.library_bytes.saturating_add(add_bytes.max(0)) > max {
                return Err(Error::Forbidden(format!("library byte limit reached: {} of {max}", usage.library_bytes)));
            }
        }
        Ok(())
    }

    pub fn allow_recording(&self, user_id: &str) -> Result<(), Error> {
        if user_id.is_empty() { return Ok(()); }
        let user = self.user_by_id(user_id)?.unwrap_or_else(User::open_account);
        if user.is_owner() || user.id.is_empty() { return Ok(()); }
        let usage = self.usage(user_id)?;
        if let Some(max) = user.max_recordings {
            if usage.recordings >= max {
                return Err(Error::Forbidden(format!("recording limit reached: {} of {max}", usage.recordings)));
            }
        }
        if let Some(max) = user.max_recording_bytes {
            if usage.recording_bytes >= max {
                return Err(Error::Forbidden(format!("recording byte limit reached: {} of {max}", usage.recording_bytes)));
            }
        }
        Ok(())
    }

    pub fn owned_files(&self, user_id: &str) -> Result<OwnedFiles, Error> {
        let c = self.lock();
        let media_ids = {
            let mut stmt = c.prepare("SELECT m.id FROM media m JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1")?;
            let rows = stmt.query_map(params![user_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };
        let recordings = {
            let mut stmt = c.prepare(
                "SELECT r.mount_id, r.filename FROM recordings r
                 JOIN mounts m ON m.id=r.mount_id JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1",
            )?;
            let rows = stmt.query_map(params![user_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<Vec<(String, String)>, _>>()?
        };
        let mount_paths = {
            let mut stmt = c.prepare("SELECT m.path FROM mounts m JOIN stations s ON s.id=m.station_id WHERE s.user_id=?1")?;
            let rows = stmt.query_map(params![user_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };
        let bot_ids = {
            let mut stmt = c.prepare("SELECT id FROM bots WHERE user_id=?1")?;
            let rows = stmt.query_map(params![user_id], |r| r.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };
        Ok(OwnedFiles { media_ids, recordings, mount_paths, bot_ids })
    }

    pub fn delete_user(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        let role: String = c.query_row("SELECT role FROM users WHERE id=?1", params![id], |r| r.get(0)).optional()?.ok_or(Error::NotFound)?;
        if role == "owner" || role == "admin" {
            return Err(Error::Invalid("the owner cannot be deleted".into()));
        }
        c.execute(
            "UPDATE bots SET station_id='', mount_id='' WHERE user_id<>?1 AND station_id IN (SELECT id FROM stations WHERE user_id=?1)",
            params![id],
        )?;
        c.execute("DELETE FROM stations WHERE user_id=?1", params![id])?;
        c.execute("DELETE FROM bots WHERE user_id=?1", params![id])?;
        c.execute("DELETE FROM users WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn playlist(&self, id: &str) -> Result<Option<Playlist>, Error> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT id, station_id, name, created_at FROM playlists WHERE id=?1",
            params![id],
            |r| Ok(Playlist { id: r.get(0)?, station_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? }),
        ).optional()?)
    }

    pub fn playlists_for(&self, user_id: &str) -> Result<Vec<Playlist>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT p.id, p.station_id, p.name, p.created_at FROM playlists p
             JOIN stations s ON s.id=p.station_id WHERE s.user_id=?1 ORDER BY p.name",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok(Playlist { id: r.get(0)?, station_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn schedule(&self, id: &str) -> Result<Option<Schedule>, Error> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT id, mount_id, playlist_id, days_mask, start_min, end_min, priority, enabled FROM schedules WHERE id=?1",
            params![id],
            map_sched,
        ).optional()?)
    }

    pub fn recordings_for(&self, user_id: &str) -> Result<Vec<Recording>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT r.id, r.mount_id, r.started_at, r.ended_at, r.duration_ms, r.format, r.bytes, r.filename, r.error
             FROM recordings r JOIN mounts m ON m.id=r.mount_id JOIN stations s ON s.id=m.station_id
             WHERE s.user_id=?1 ORDER BY r.started_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id], map_recording)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn create_token(&self, user_id: &str, name: &str, token_hash: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO tokens (id, user_id, token_hash, name, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![auth::new_id(), user_id, token_hash, name, now_ms()],
        )?;
        Ok(())
    }

    pub fn revoke_user_tokens(&self, user_id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("UPDATE tokens SET revoked_at=?1 WHERE user_id=?2 AND revoked_at IS NULL", params![now_ms(), user_id])?;
        Ok(())
    }

    pub fn user_by_token(&self, token_hash: &str) -> Result<Option<User>, Error> {
        Ok(self.authenticate_token(token_hash)?.map(|hit| hit.account))
    }

    /// Look up a live token by its hash. Revoked and expired rows are absent.
    /// The hash is never copied into the result. Last-used is touched on this
    /// same connection, at most once every 30 seconds.
    pub fn authenticate_token(&self, token_hash: &str) -> Result<Option<TokenHit>, Error> {
        let c = self.lock();
        let now = now_ms();
        let sql = format!(
            "SELECT {TOKEN_COLS} FROM tokens WHERE token_hash=?1 AND revoked_at IS NULL
             AND (expires_at IS NULL OR expires_at > ?2)"
        );
        let Some(token) = c.query_row(&sql, params![token_hash, now], map_token).optional()? else {
            return Ok(None);
        };
        let Some(account) = load_user(&c, &token.user_id)? else {
            return Ok(None);
        };
        let act_as = if token.kind == "api" && token.scope == "user" && !token.act_as.is_empty() {
            load_user(&c, &token.act_as)?
        } else {
            None
        };
        if token.last_used_at.unwrap_or(0) + 30_000 < now {
            let _ = c.execute("UPDATE tokens SET last_used_at=?1 WHERE id=?2", params![now, token.id]);
        }
        Ok(Some(TokenHit { token, account, act_as }))
    }

    pub fn insert_api_token(&self, row: &TokenRow, token_hash: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO tokens (id, user_id, token_hash, name, created_at, prefix, expires_at, kind, scope, act_as,
             perm_stations, perm_mounts, perm_playlists, perm_library, perm_import, perm_recordings, perm_schedules, perm_bots, perm_listeners, perm_takeover)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'api',?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            params![
                row.id, row.user_id, token_hash, row.name, row.created_at, row.prefix, row.expires_at, row.scope, row.act_as,
                row.perm_stations as i64, row.perm_mounts as i64, row.perm_playlists as i64, row.perm_library as i64,
                row.perm_import as i64, row.perm_recordings as i64, row.perm_schedules as i64, row.perm_bots as i64,
                row.perm_listeners as i64, row.perm_takeover as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_api_tokens(&self) -> Result<Vec<TokenRow>, Error> {
        let c = self.lock();
        let sql = format!("SELECT {TOKEN_COLS} FROM tokens WHERE kind='api' AND revoked_at IS NULL ORDER BY created_at DESC");
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([], map_token)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn api_token(&self, id: &str) -> Result<Option<TokenRow>, Error> {
        let c = self.lock();
        let sql = format!("SELECT {TOKEN_COLS} FROM tokens WHERE id=?1 AND kind='api' AND revoked_at IS NULL");
        Ok(c.query_row(&sql, params![id], map_token).optional()?)
    }

    pub fn save_api_token(&self, row: &TokenRow) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE tokens SET name=?2, expires_at=?3, scope=?4, act_as=?5,
             perm_stations=?6, perm_mounts=?7, perm_playlists=?8, perm_library=?9, perm_import=?10,
             perm_recordings=?11, perm_schedules=?12, perm_bots=?13, perm_listeners=?14, perm_takeover=?15
             WHERE id=?1 AND kind='api' AND revoked_at IS NULL",
            params![
                row.id, row.name, row.expires_at, row.scope, row.act_as,
                row.perm_stations as i64, row.perm_mounts as i64, row.perm_playlists as i64, row.perm_library as i64,
                row.perm_import as i64, row.perm_recordings as i64, row.perm_schedules as i64, row.perm_bots as i64,
                row.perm_listeners as i64, row.perm_takeover as i64,
            ],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn rotate_api_token(&self, id: &str, token_hash: &str, prefix: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE tokens SET token_hash=?2, prefix=?3 WHERE id=?1 AND kind='api' AND revoked_at IS NULL",
            params![id, token_hash, prefix],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn revoke_api_token(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE tokens SET revoked_at=?2 WHERE id=?1 AND kind='api' AND revoked_at IS NULL",
            params![id, now_ms()],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn revoke_session_tokens(&self, user_id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "UPDATE tokens SET revoked_at=?1 WHERE user_id=?2 AND revoked_at IS NULL AND kind='session'",
            params![now_ms(), user_id],
        )?;
        Ok(())
    }

    pub fn create_station(&self, user_id: &str, name: &str, slug: &str, description: &str, genre: &str) -> Result<Station, Error> {
        let id = auth::new_id();
        let created = now_ms();
        let c = self.lock();
        c.execute(
            "INSERT INTO stations (id, name, slug, description, genre, created_at, user_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![id, name, slug, description, genre, created, user_id],
        )?;
        Ok(Station {
            id, name: name.into(), slug: slug.into(), description: description.into(), genre: genre.into(),
            created_at: created, radio_enabled: true, user_id: user_id.into(),
        })
    }

    pub fn list_stations(&self) -> Result<Vec<Station>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, name, slug, description, genre, created_at, radio_enabled, user_id FROM stations ORDER BY name")?;
        let rows = stmt.query_map([], map_station)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn station(&self, id: &str) -> Result<Option<Station>, Error> {
        let c = self.lock();
        Ok(c.query_row("SELECT id, name, slug, description, genre, created_at, radio_enabled, user_id FROM stations WHERE id=?1", params![id], map_station).optional()?)
    }

    pub fn station_by_slug(&self, slug: &str) -> Result<Option<Station>, Error> {
        let c = self.lock();
        Ok(c.query_row("SELECT id, name, slug, description, genre, created_at, radio_enabled, user_id FROM stations WHERE slug=?1", params![slug], map_station).optional()?)
    }

    pub fn delete_station(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("UPDATE bots SET station_id='', mount_id='' WHERE station_id=?1", params![id])?;
        c.execute("DELETE FROM stations WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn set_radio(&self, id: &str, on: bool) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("UPDATE stations SET radio_enabled=?2 WHERE id=?1", params![id, on as i64])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>, Error> {
        let c = self.lock();
        Ok(c.query_row("SELECT value FROM settings WHERE key=?1", params![key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn create_mount(&self, m: &Mount) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO mounts (id, station_id, path, source_user, source_password_hash, format, bitrate, max_listeners, max_bitrate, allow_takeover, fallback_playlist_id, created_at, crossfade_ms, autodj, shuffle, repeat, jingle_every, jingle_media_id, intro_media_id, gain_db, sample_rate, channels, buffer_ms, hls_segment_ms, repeat_mode, encoder, relay_url, record, record_max_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)",
            params![m.id, m.station_id, m.path, m.source_user, m.source_password_hash, m.format, m.bitrate, m.max_listeners, m.max_bitrate, m.allow_takeover as i64, m.fallback_playlist_id, m.created_at, m.crossfade_ms, m.autodj as i64, m.shuffle as i64, repeat_int(&m.repeat), m.jingle_every, m.jingle_media_id, m.intro_media_id, m.gain_db, m.sample_rate, m.channels, m.buffer_ms, m.hls_segment_ms, normalize_repeat(&m.repeat), m.encoder, m.relay_url, m.record as i64, m.record_max_ms],
        )?;
        Ok(())
    }

    pub fn update_mount(&self, m: &Mount) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "UPDATE mounts SET path=?2, source_user=?3, source_password_hash=?4, format=?5, bitrate=?6, max_listeners=?7, max_bitrate=?8, allow_takeover=?9, fallback_playlist_id=?10, crossfade_ms=?11, autodj=?12, shuffle=?13, repeat=?14, jingle_every=?15, jingle_media_id=?16, intro_media_id=?17, gain_db=?18, sample_rate=?19, channels=?20, buffer_ms=?21, hls_segment_ms=?22, repeat_mode=?23, encoder=?24, relay_url=?25, record=?26, record_max_ms=?27 WHERE id=?1",
            params![m.id, m.path, m.source_user, m.source_password_hash, m.format, m.bitrate, m.max_listeners, m.max_bitrate, m.allow_takeover as i64, m.fallback_playlist_id, m.crossfade_ms, m.autodj as i64, m.shuffle as i64, repeat_int(&m.repeat), m.jingle_every, m.jingle_media_id, m.intro_media_id, m.gain_db, m.sample_rate, m.channels, m.buffer_ms, m.hls_segment_ms, normalize_repeat(&m.repeat), m.encoder, m.relay_url, m.record as i64, m.record_max_ms],
        )?;
        Ok(())
    }

    pub fn list_mounts(&self, station_id: Option<&str>) -> Result<Vec<Mount>, Error> {
        let c = self.lock();
        let sql = MOUNT_SQL;
        if let Some(sid) = station_id {
            let mut stmt = c.prepare(&format!("{sql} WHERE station_id=?1 ORDER BY path"))?;
            let rows = stmt.query_map(params![sid], map_mount)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        } else {
            let mut stmt = c.prepare(&format!("{sql} ORDER BY path"))?;
            let rows = stmt.query_map([], map_mount)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        }
    }

    pub fn mount(&self, id: &str) -> Result<Option<Mount>, Error> {
        let c = self.lock();
        Ok(c.query_row(&format!("{MOUNT_SQL} WHERE id=?1"), params![id], map_mount).optional()?)
    }

    pub fn delete_mount(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("DELETE FROM mounts WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn create_media(&self, m: &Media) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO media (id, station_id, filename, mime, bytes, title, artist, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![m.id, m.station_id, m.filename, m.mime, m.bytes, m.title, m.artist, m.created_at],
        )?;
        Ok(())
    }

    pub fn list_media(&self, station_id: &str) -> Result<Vec<Media>, Error> {
        self.search_media(station_id, "")
    }

    pub fn search_media(&self, station_id: &str, query: &str) -> Result<Vec<Media>, Error> {
        let c = self.lock();
        let q = query.trim();
        if q.is_empty() {
            let mut stmt = c.prepare("SELECT id, station_id, filename, mime, bytes, title, artist, created_at FROM media WHERE station_id=?1 ORDER BY created_at DESC")?;
            let rows = stmt.query_map(params![station_id], map_media)?;
            return Ok(rows.collect::<Result<Vec<_>, _>>()?);
        }
        let like = format!("%{}%", q.replace('%', "").replace('_', ""));
        let mut stmt = c.prepare(
            "SELECT id, station_id, filename, mime, bytes, title, artist, created_at FROM media
             WHERE station_id=?1 AND (title LIKE ?2 COLLATE NOCASE OR artist LIKE ?2 COLLATE NOCASE OR filename LIKE ?2 COLLATE NOCASE)
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![station_id, like], map_media)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn update_media_tags(&self, id: &str, title: &str, artist: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("UPDATE media SET title=?2, artist=?3 WHERE id=?1", params![id, title, artist])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn replace_media(&self, id: &str, filename: &str, mime: &str, bytes: i64, title: &str, artist: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE media SET filename=?2, mime=?3, bytes=?4, title=?5, artist=?6 WHERE id=?1",
            params![id, filename, mime, bytes, title, artist],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn media(&self, id: &str) -> Result<Option<Media>, Error> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT id, station_id, filename, mime, bytes, title, artist, created_at FROM media WHERE id=?1",
            params![id],
            map_media,
        ).optional()?)
    }

    pub fn delete_media(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("DELETE FROM media WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn create_playlist(&self, station_id: &str, name: &str) -> Result<Playlist, Error> {
        let id = auth::new_id();
        let created = now_ms();
        let c = self.lock();
        c.execute(
            "INSERT INTO playlists (id, station_id, name, created_at) VALUES (?1,?2,?3,?4)",
            params![id, station_id, name, created],
        )?;
        Ok(Playlist { id, station_id: station_id.into(), name: name.into(), created_at: created })
    }

    pub fn list_playlists(&self, station_id: &str) -> Result<Vec<Playlist>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, station_id, name, created_at FROM playlists WHERE station_id=?1 ORDER BY name")?;
        let rows = stmt.query_map(params![station_id], |r| {
            Ok(Playlist { id: r.get(0)?, station_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn playlist_items(&self, playlist_id: &str) -> Result<Vec<PlaylistItem>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT i.id, i.playlist_id, i.media_id, i.position, m.title, m.artist, m.filename
             FROM playlist_items i JOIN media m ON m.id=i.media_id
             WHERE i.playlist_id=?1 ORDER BY i.position",
        )?;
        let rows = stmt.query_map(params![playlist_id], |r| {
            Ok(PlaylistItem {
                id: r.get(0)?, playlist_id: r.get(1)?, media_id: r.get(2)?, position: r.get(3)?,
                title: r.get(4)?, artist: r.get(5)?, filename: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn add_playlist_item(&self, playlist_id: &str, media_id: &str) -> Result<(), Error> {
        let c = self.lock();
        let pos: i64 = c.query_row(
            "SELECT COALESCE(MAX(position),0)+1 FROM playlist_items WHERE playlist_id=?1",
            params![playlist_id],
            |r| r.get(0),
        )?;
        c.execute(
            "INSERT INTO playlist_items (id, playlist_id, media_id, position) VALUES (?1,?2,?3,?4)",
            params![auth::new_id(), playlist_id, media_id, pos],
        )?;
        Ok(())
    }

    pub fn reorder_playlist(&self, playlist_id: &str, from: usize, to: usize) -> Result<(), Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id FROM playlist_items WHERE playlist_id=?1 ORDER BY position, id")?;
        let mut ids: Vec<String> = stmt.query_map(params![playlist_id], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        if from >= ids.len() || to >= ids.len() {
            return Err(Error::Invalid("bad position".into()));
        }
        let item = ids.remove(from);
        ids.insert(to, item);
        for (i, id) in ids.iter().enumerate() {
            c.execute("UPDATE playlist_items SET position=?1 WHERE id=?2", params![i as i64 + 1, id])?;
        }
        Ok(())
    }

    pub fn delete_playlist_item(&self, playlist_id: &str, item_id: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("DELETE FROM playlist_items WHERE playlist_id=?1 AND id=?2", params![playlist_id, item_id])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn delete_playlist(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("DELETE FROM playlists WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn add_schedule(&self, s: &Schedule) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO schedules (id, mount_id, playlist_id, days_mask, start_min, end_min, priority, enabled) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![s.id, s.mount_id, s.playlist_id, s.days_mask, s.start_min, s.end_min, s.priority, s.enabled as i64],
        )?;
        Ok(())
    }

    pub fn update_schedule(&self, id: &str, enabled: Option<bool>, priority: Option<i64>) -> Result<Schedule, Error> {
        let c = self.lock();
        let mut row = c.query_row(
            "SELECT id, mount_id, playlist_id, days_mask, start_min, end_min, priority, enabled FROM schedules WHERE id=?1",
            params![id],
            map_sched,
        ).optional()?.ok_or(Error::NotFound)?;
        if let Some(v) = enabled { row.enabled = v; }
        if let Some(v) = priority { row.priority = v; }
        c.execute(
            "UPDATE schedules SET enabled=?2, priority=?3 WHERE id=?1",
            params![id, row.enabled as i64, row.priority],
        )?;
        Ok(row)
    }

    pub fn list_schedules(&self, mount_id: &str) -> Result<Vec<Schedule>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, mount_id, playlist_id, days_mask, start_min, end_min, priority, enabled FROM schedules WHERE mount_id=?1 ORDER BY priority DESC")?;
        let rows = stmt.query_map(params![mount_id], map_sched)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Playlist to play on this mount at `unix_secs`, then its tracks in order.
    pub fn tracks_for_mount(&self, mount_id: &str, unix_secs: i64) -> Result<(Option<String>, Vec<Media>), Error> {
        let mount = self.mount(mount_id)?.ok_or(Error::NotFound)?;
        let schedules = self.list_schedules(mount_id)?;
        let windows: Vec<Window> = schedules.iter().map(|s| Window {
            playlist_id: s.playlist_id.clone(),
            days_mask: s.days_mask as i32,
            start_min: s.start_min as i32,
            end_min: s.end_min as i32,
            priority: s.priority as i32,
            enabled: s.enabled,
        }).collect();
        let chosen = schedule::select(&windows, unix_secs).or(mount.fallback_playlist_id);
        let Some(pid) = chosen else {
            return Ok((None, Vec::new()));
        };
        let items = self.playlist_items(&pid)?;
        let mut tracks = Vec::new();
        for item in items {
            if let Some(m) = self.media(&item.media_id)? {
                tracks.push(m);
            }
        }
        Ok((Some(pid), tracks))
    }

    pub fn insert_recording(&self, r: &Recording) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO recordings (id, mount_id, started_at, ended_at, duration_ms, format, bytes, filename, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![r.id, r.mount_id, r.started_at, r.ended_at, r.duration_ms, r.format, r.bytes, r.filename, r.error],
        )?;
        Ok(())
    }

    pub fn finish_recording(&self, id: &str, ended_at: i64, duration_ms: i64, bytes: i64, error: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "UPDATE recordings SET ended_at=?2, duration_ms=?3, bytes=?4, error=?5 WHERE id=?1 AND ended_at IS NULL",
            params![id, ended_at, duration_ms, bytes, error],
        )?;
        Ok(())
    }

    pub fn list_recordings(&self, mount_id: &str) -> Result<Vec<Recording>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id, mount_id, started_at, ended_at, duration_ms, format, bytes, filename, error
             FROM recordings WHERE mount_id=?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![mount_id], map_recording)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn recording(&self, id: &str) -> Result<Option<Recording>, Error> {
        let c = self.lock();
        Ok(c.query_row(
            "SELECT id, mount_id, started_at, ended_at, duration_ms, format, bytes, filename, error FROM recordings WHERE id=?1",
            params![id],
            map_recording,
        ).optional()?)
    }

    pub fn open_recordings(&self, mount_id: &str) -> Result<Vec<Recording>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id, mount_id, started_at, ended_at, duration_ms, format, bytes, filename, error
             FROM recordings WHERE mount_id=?1 AND ended_at IS NULL",
        )?;
        let rows = stmt.query_map(params![mount_id], map_recording)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn delete_recording_row(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("DELETE FROM recordings WHERE id=?1", params![id])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn delete_mount_recordings(&self, mount_id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("DELETE FROM recordings WHERE mount_id=?1", params![mount_id])?;
        Ok(())
    }

    pub fn recording_bytes(&self) -> Result<i64, Error> {
        let c = self.lock();
        let n: i64 = c.query_row("SELECT COALESCE(SUM(bytes), 0) FROM recordings", [], |r| r.get(0))?;
        Ok(n)
    }

    pub fn audit(&self, actor: &str, action: &str, detail: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO audit (at, actor, action, detail) VALUES (?1,?2,?3,?4)",
            params![now_ms(), actor, action, detail],
        )?;
        Ok(())
    }

    pub fn insert_output(&self, row: &Output) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO bots (id, station_id, mount_id, host, voice_port, server_password, nickname, phonetic, away, channel_password, default_channel_id, identity, volume, enabled, auto_reconnect, prefix, commands, gate, gate_group, repeat_mode, created_at, user_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                row.id, row.station_id, row.mount_id, row.host, row.voice_port, row.server_password,
                row.nickname, row.phonetic, row.away, row.channel_password, row.default_channel_id,
                row.identity, row.volume, row.enabled as i64, row.auto_reconnect as i64, row.prefix,
                row.commands, row.gate, row.gate_group, row.repeat_mode, row.created_at, row.user_id,
            ],
        )?;
        Ok(())
    }

    pub fn save_output(&self, row: &Output) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE bots SET station_id=?2, mount_id=?3, host=?4, voice_port=?5, server_password=?6, nickname=?7, phonetic=?8, away=?9, channel_password=?10, default_channel_id=?11, volume=?12, enabled=?13, auto_reconnect=?14, prefix=?15, commands=?16, gate=?17, gate_group=?18, repeat_mode=?19 WHERE id=?1",
            params![
                row.id, row.station_id, row.mount_id, row.host, row.voice_port, row.server_password,
                row.nickname, row.phonetic, row.away, row.channel_password, row.default_channel_id,
                row.volume, row.enabled as i64, row.auto_reconnect as i64, row.prefix, row.commands,
                row.gate, row.gate_group, row.repeat_mode,
            ],
        )?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn output(&self, id: &str) -> Result<Option<Output>, Error> {
        let c = self.lock();
        let row = c.query_row(&format!("{BOT_SQL} WHERE id=?1"), params![id], map_output).optional()?;
        Ok(row)
    }

    pub fn list_outputs(&self, station_id: Option<&str>) -> Result<Vec<Output>, Error> {
        let c = self.lock();
        let mut rows = if let Some(station_id) = station_id {
            let mut stmt = c.prepare(&format!("{BOT_SQL} WHERE station_id=?1 ORDER BY created_at"))?;
            let mapped = stmt.query_map(params![station_id], map_output)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = c.prepare(&format!("{BOT_SQL} ORDER BY created_at"))?;
            let mapped = stmt.query_map([], map_output)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        rows.sort_by(|a, b| a.nickname.cmp(&b.nickname).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    pub fn set_output_identity(&self, id: &str, identity: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("UPDATE bots SET identity=?2 WHERE id=?1", params![id, identity])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn set_output_volume(&self, id: &str, volume: i64) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("UPDATE bots SET volume=?2 WHERE id=?1", params![id, volume])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn delete_output(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        let n = c.execute("DELETE FROM bots WHERE id=?1", params![id])?;
        if n == 0 { return Err(Error::NotFound); }
        Ok(())
    }

    pub fn list_audit(&self, limit: i64) -> Result<Vec<Audit>, Error> {
        let limit = limit.clamp(1, 500);
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, at, actor, action, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(Audit { id: r.get(0)?, at: r.get(1)?, actor: r.get(2)?, action: r.get(3)?, detail: r.get(4)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

const TOKEN_COLS: &str = "id, user_id, name, prefix, created_at, expires_at, last_used_at, kind, scope, act_as, perm_stations, perm_mounts, perm_playlists, perm_library, perm_import, perm_recordings, perm_schedules, perm_bots, perm_listeners, perm_takeover";

fn map_token(r: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRow> {
    Ok(TokenRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        name: r.get(2)?,
        prefix: r.get(3)?,
        created_at: r.get(4)?,
        expires_at: r.get(5)?,
        last_used_at: r.get(6)?,
        kind: r.get(7)?,
        scope: r.get(8)?,
        act_as: r.get(9)?,
        perm_stations: r.get::<_, i64>(10)? != 0,
        perm_mounts: r.get::<_, i64>(11)? != 0,
        perm_playlists: r.get::<_, i64>(12)? != 0,
        perm_library: r.get::<_, i64>(13)? != 0,
        perm_import: r.get::<_, i64>(14)? != 0,
        perm_recordings: r.get::<_, i64>(15)? != 0,
        perm_schedules: r.get::<_, i64>(16)? != 0,
        perm_bots: r.get::<_, i64>(17)? != 0,
        perm_listeners: r.get::<_, i64>(18)? != 0,
        perm_takeover: r.get::<_, i64>(19)? != 0,
    })
}

fn load_user(c: &Connection, id: &str) -> Result<Option<User>, Error> {
    let sql = format!("SELECT {USER_COLS} FROM users WHERE id=?1");
    Ok(c.query_row(&sql, params![id], map_user).optional()?)
}

const USER_COLS: &str = "id, username, password_hash, role, disabled, perm_stations, perm_mounts, perm_playlists, perm_library, perm_import, perm_recordings, perm_schedules, perm_bots, perm_listeners, perm_takeover, max_stations, max_mounts, max_playlists, max_tracks, max_library_bytes, max_bots, max_recordings, max_recording_bytes";
#[derive(Clone, Debug)]
pub struct TokenRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prefix: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub kind: String,
    pub scope: String,
    pub act_as: String,
    pub perm_stations: bool,
    pub perm_mounts: bool,
    pub perm_playlists: bool,
    pub perm_library: bool,
    pub perm_import: bool,
    pub perm_recordings: bool,
    pub perm_schedules: bool,
    pub perm_bots: bool,
    pub perm_listeners: bool,
    pub perm_takeover: bool,
}

pub struct TokenHit {
    pub token: TokenRow,
    pub account: User,
    pub act_as: Option<User>,
}

fn map_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        role: r.get(3)?,
        disabled: r.get::<_, i64>(4)? != 0,
        perm_stations: r.get::<_, i64>(5)? != 0,
        perm_mounts: r.get::<_, i64>(6)? != 0,
        perm_playlists: r.get::<_, i64>(7)? != 0,
        perm_library: r.get::<_, i64>(8)? != 0,
        perm_import: r.get::<_, i64>(9)? != 0,
        perm_recordings: r.get::<_, i64>(10)? != 0,
        perm_schedules: r.get::<_, i64>(11)? != 0,
        perm_bots: r.get::<_, i64>(12)? != 0,
        perm_listeners: r.get::<_, i64>(13)? != 0,
        perm_takeover: r.get::<_, i64>(14)? != 0,
        max_stations: r.get(15)?,
        max_mounts: r.get(16)?,
        max_playlists: r.get(17)?,
        max_tracks: r.get(18)?,
        max_library_bytes: r.get(19)?,
        max_bots: r.get(20)?,
        max_recordings: r.get(21)?,
        max_recording_bytes: r.get(22)?,
    })
}
fn map_station(r: &rusqlite::Row<'_>) -> rusqlite::Result<Station> {
    Ok(Station {
        id: r.get(0)?, name: r.get(1)?, slug: r.get(2)?, description: r.get(3)?, genre: r.get(4)?, created_at: r.get(5)?,
        radio_enabled: r.get::<_, i64>(6).unwrap_or(1) != 0,
        user_id: r.get(7).unwrap_or_default(),
    })
}
fn map_mount(r: &rusqlite::Row<'_>) -> rusqlite::Result<Mount> {
    Ok(Mount {
        id: r.get(0)?, station_id: r.get(1)?, path: r.get(2)?, source_user: r.get(3)?,
        source_password_hash: r.get(4)?, format: r.get(5)?, bitrate: r.get(6)?, max_listeners: r.get(7)?,
        max_bitrate: r.get(8)?, allow_takeover: r.get::<_, i64>(9)? != 0, fallback_playlist_id: r.get(10)?, created_at: r.get(11)?,
        crossfade_ms: r.get(12)?, autodj: r.get::<_, i64>(13)? != 0, shuffle: r.get::<_, i64>(14)? != 0,
        repeat: read_repeat(r)?, jingle_every: r.get(16)?, jingle_media_id: r.get(17)?, intro_media_id: r.get(18)?,
        gain_db: r.get(19).unwrap_or(0.0), sample_rate: r.get(20).unwrap_or(44_100),
        channels: r.get(21).unwrap_or(2), buffer_ms: r.get(22).unwrap_or(250),
        hls_segment_ms: r.get(23).unwrap_or(1_000),
        encoder: r.get(25).unwrap_or_else(|_| "ffmpeg".into()),
        relay_url: r.get(26).unwrap_or_else(|_| String::new()),
        record: r.get::<_, i64>(27).unwrap_or(0) != 0,
        record_max_ms: r.get(28).unwrap_or(3_600_000),
    })
}

fn read_repeat(r: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    if let Ok(mode) = r.get::<_, String>(24) {
        if matches!(mode.as_str(), "off" | "one" | "all") {
            return Ok(mode);
        }
    }
    let legacy: i64 = r.get(15).unwrap_or(1);
    Ok(match legacy {
        0 => "off".into(),
        2 => "one".into(),
        _ => "all".into(),
    })
}

pub fn normalize_repeat(mode: &str) -> String {
    match mode {
        "off" | "one" | "all" => mode.to_string(),
        _ => "all".into(),
    }
}

fn repeat_int(mode: &str) -> i64 {
    match mode {
        "off" => 0,
        "one" => 2,
        _ => 1,
    }
}
fn map_recording(r: &rusqlite::Row<'_>) -> rusqlite::Result<Recording> {
    Ok(Recording {
        id: r.get(0)?,
        mount_id: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        duration_ms: r.get(4)?,
        format: r.get(5)?,
        bytes: r.get(6)?,
        filename: r.get(7)?,
        error: r.get(8)?,
    })
}
fn map_output(r: &rusqlite::Row<'_>) -> rusqlite::Result<Output> {
    Ok(Output {
        id: r.get(0)?,
        station_id: r.get(1)?,
        mount_id: r.get(2)?,
        host: r.get(3)?,
        voice_port: r.get(4)?,
        server_password: r.get(5)?,
        nickname: r.get(6)?,
        phonetic: r.get(7)?,
        away: r.get(8)?,
        channel_password: r.get(9)?,
        default_channel_id: r.get(10)?,
        identity: r.get(11)?,
        volume: r.get(12)?,
        enabled: r.get::<_, i64>(13)? != 0,
        auto_reconnect: r.get::<_, i64>(14)? != 0,
        prefix: r.get(15)?,
        commands: r.get(16)?,
        gate: r.get(17)?,
        gate_group: r.get(18)?,
        repeat_mode: r.get(19)?,
        created_at: r.get(20)?,
        user_id: r.get(21).unwrap_or_default(),
    })
}

const BOT_SQL: &str = "SELECT id, station_id, mount_id, host, voice_port, server_password, nickname, phonetic, away, channel_password, default_channel_id, identity, volume, enabled, auto_reconnect, prefix, commands, gate, gate_group, repeat_mode, created_at, user_id FROM bots";

fn map_media(r: &rusqlite::Row<'_>) -> rusqlite::Result<Media> {
    Ok(Media { id: r.get(0)?, station_id: r.get(1)?, filename: r.get(2)?, mime: r.get(3)?, bytes: r.get(4)?, title: r.get(5)?, artist: r.get(6)?, created_at: r.get(7)? })
}
fn map_sched(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?, mount_id: r.get(1)?, playlist_id: r.get(2)?, days_mask: r.get(3)?,
        start_min: r.get(4)?, end_min: r.get(5)?, priority: r.get(6)?, enabled: r.get::<_, i64>(7)? != 0,
    })
}

const MOUNT_SQL: &str = "SELECT id, station_id, path, source_user, source_password_hash, format, bitrate, max_listeners, max_bitrate, allow_takeover, fallback_playlist_id, created_at, crossfade_ms, autodj, shuffle, repeat, jingle_every, jingle_media_id, intro_media_id, gain_db, sample_rate, channels, buffer_ms, hls_segment_ms, repeat_mode, encoder, relay_url, record, record_max_ms FROM mounts";

fn migrate(conn: &Connection) -> Result<(), Error> {
    let alters = [
        "ALTER TABLE mounts ADD COLUMN crossfade_ms INTEGER NOT NULL DEFAULT 250",
        "ALTER TABLE mounts ADD COLUMN autodj INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE mounts ADD COLUMN shuffle INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE mounts ADD COLUMN repeat INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE mounts ADD COLUMN jingle_every INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE mounts ADD COLUMN jingle_media_id TEXT",
        "ALTER TABLE mounts ADD COLUMN intro_media_id TEXT",
        "ALTER TABLE schedules ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE mounts ADD COLUMN gain_db REAL NOT NULL DEFAULT 0",
        "ALTER TABLE mounts ADD COLUMN sample_rate INTEGER NOT NULL DEFAULT 44100",
        "ALTER TABLE mounts ADD COLUMN channels INTEGER NOT NULL DEFAULT 2",
        "ALTER TABLE mounts ADD COLUMN buffer_ms INTEGER NOT NULL DEFAULT 250",
        "ALTER TABLE mounts ADD COLUMN hls_segment_ms INTEGER NOT NULL DEFAULT 1000",
        "ALTER TABLE mounts ADD COLUMN encoder TEXT NOT NULL DEFAULT 'ffmpeg'",
        "ALTER TABLE mounts ADD COLUMN relay_url TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE mounts ADD COLUMN record INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE mounts ADD COLUMN record_max_ms INTEGER NOT NULL DEFAULT 3600000",
    ];
    for sql in alters {
        let _ = conn.execute(sql, []);
    }
    if conn.execute("ALTER TABLE mounts ADD COLUMN repeat_mode TEXT NOT NULL DEFAULT 'all'", []).is_ok() {
        let _ = conn.execute("UPDATE mounts SET repeat_mode='off' WHERE repeat=0", []);
    }
    let _ = conn.execute("ALTER TABLE stations ADD COLUMN radio_enabled INTEGER NOT NULL DEFAULT 1", []);
    let _ = conn.execute("ALTER TABLE stations ADD COLUMN user_id TEXT NOT NULL DEFAULT ''", []);
    for sql in [
        "ALTER TABLE users ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE users ADD COLUMN perm_stations INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_mounts INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_playlists INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_library INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_import INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_recordings INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_schedules INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_bots INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_listeners INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN perm_takeover INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE users ADD COLUMN max_stations INTEGER",
        "ALTER TABLE users ADD COLUMN max_mounts INTEGER",
        "ALTER TABLE users ADD COLUMN max_playlists INTEGER",
        "ALTER TABLE users ADD COLUMN max_tracks INTEGER",
        "ALTER TABLE users ADD COLUMN max_library_bytes INTEGER",
        "ALTER TABLE users ADD COLUMN max_bots INTEGER",
        "ALTER TABLE users ADD COLUMN max_recordings INTEGER",
        "ALTER TABLE users ADD COLUMN max_recording_bytes INTEGER",
    ] {
        let _ = conn.execute(sql, []);
    }
    for sql in [
        "ALTER TABLE tokens ADD COLUMN prefix TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE tokens ADD COLUMN expires_at INTEGER",
        "ALTER TABLE tokens ADD COLUMN last_used_at INTEGER",
        "ALTER TABLE tokens ADD COLUMN kind TEXT NOT NULL DEFAULT 'session'",
        "ALTER TABLE tokens ADD COLUMN scope TEXT NOT NULL DEFAULT 'user'",
        "ALTER TABLE tokens ADD COLUMN act_as TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE tokens ADD COLUMN perm_stations INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_mounts INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_playlists INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_library INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_import INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_recordings INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_schedules INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_bots INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_listeners INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE tokens ADD COLUMN perm_takeover INTEGER NOT NULL DEFAULT 1",
    ] {
        let _ = conn.execute(sql, []);
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bots (
          id TEXT PRIMARY KEY,
          station_id TEXT NOT NULL DEFAULT '',
          mount_id TEXT NOT NULL DEFAULT '',
          host TEXT NOT NULL,
          voice_port INTEGER NOT NULL DEFAULT 9987,
          server_password TEXT NOT NULL DEFAULT '',
          nickname TEXT NOT NULL,
          phonetic TEXT NOT NULL DEFAULT '',
          away TEXT NOT NULL DEFAULT '',
          channel_password TEXT NOT NULL DEFAULT '',
          default_channel_id INTEGER,
          identity TEXT NOT NULL DEFAULT '',
          volume INTEGER NOT NULL DEFAULT 80,
          enabled INTEGER NOT NULL DEFAULT 0,
          auto_reconnect INTEGER NOT NULL DEFAULT 1,
          prefix TEXT NOT NULL DEFAULT '!',
          commands TEXT NOT NULL DEFAULT 'play,queue,skip,stop,np,vol,radio,help',
          gate TEXT NOT NULL DEFAULT 'everyone',
          gate_group TEXT NOT NULL DEFAULT '',
          repeat_mode TEXT NOT NULL DEFAULT 'off',
          created_at INTEGER NOT NULL
        );",
    )?;
    let copied: i64 = conn.query_row("SELECT COUNT(*) FROM bots", [], |r| r.get(0)).unwrap_or(0);
    if copied == 0 {
        let _ = conn.execute(
            "INSERT INTO bots (id, station_id, mount_id, host, voice_port, server_password, nickname, channel_password, default_channel_id, identity, volume, enabled, created_at)
             SELECT id, station_id, mount_id, host, voice_port, server_password, nickname, channel_password, channel_id, identity, volume, enabled, created_at FROM outputs",
            [],
        );
    }
    let _ = conn.execute("ALTER TABLE bots ADD COLUMN user_id TEXT NOT NULL DEFAULT ''", []);
    if let Ok(owner) = conn.query_row(
        "SELECT id FROM users ORDER BY created_at LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    ) {
        let _ = conn.execute("UPDATE users SET role='user' WHERE role='admin' AND id<>?1", params![owner]);
        let _ = conn.execute("UPDATE users SET role='owner' WHERE id=?1", params![owner]);
        let _ = conn.execute("UPDATE stations SET user_id=?1 WHERE user_id=''", params![owner]);
        let _ = conn.execute("UPDATE bots SET user_id=?1 WHERE user_id=''", params![owner]);
    }
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY,
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  role TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS stations (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  slug TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  genre TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlists (
  id TEXT PRIMARY KEY,
  station_id TEXT NOT NULL REFERENCES stations(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mounts (
  id TEXT PRIMARY KEY,
  station_id TEXT NOT NULL REFERENCES stations(id) ON DELETE CASCADE,
  path TEXT NOT NULL UNIQUE,
  source_user TEXT NOT NULL,
  source_password_hash TEXT NOT NULL,
  format TEXT NOT NULL,
  bitrate INTEGER NOT NULL,
  max_listeners INTEGER NOT NULL,
  max_bitrate INTEGER NOT NULL,
  allow_takeover INTEGER NOT NULL,
  fallback_playlist_id TEXT,
  created_at INTEGER NOT NULL,
  crossfade_ms INTEGER NOT NULL DEFAULT 250,
  autodj INTEGER NOT NULL DEFAULT 1,
  shuffle INTEGER NOT NULL DEFAULT 0,
  repeat INTEGER NOT NULL DEFAULT 1,
  jingle_every INTEGER NOT NULL DEFAULT 0,
  jingle_media_id TEXT,
  intro_media_id TEXT,
  gain_db REAL NOT NULL DEFAULT 0,
  sample_rate INTEGER NOT NULL DEFAULT 44100,
  channels INTEGER NOT NULL DEFAULT 2,
  buffer_ms INTEGER NOT NULL DEFAULT 250,
  hls_segment_ms INTEGER NOT NULL DEFAULT 1000,
  repeat_mode TEXT NOT NULL DEFAULT 'all',
  encoder TEXT NOT NULL DEFAULT 'ffmpeg',
  relay_url TEXT NOT NULL DEFAULT '',
  record INTEGER NOT NULL DEFAULT 0,
  record_max_ms INTEGER NOT NULL DEFAULT 3600000
);
CREATE TABLE IF NOT EXISTS recordings (
  id TEXT PRIMARY KEY,
  mount_id TEXT NOT NULL REFERENCES mounts(id) ON DELETE CASCADE,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  format TEXT NOT NULL,
  bytes INTEGER NOT NULL DEFAULT 0,
  filename TEXT NOT NULL,
  error TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS media (
  id TEXT PRIMARY KEY,
  station_id TEXT NOT NULL REFERENCES stations(id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  mime TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlist_items (
  id TEXT PRIMARY KEY,
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  media_id TEXT NOT NULL REFERENCES media(id) ON DELETE CASCADE,
  position INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS schedules (
  id TEXT PRIMARY KEY,
  mount_id TEXT NOT NULL REFERENCES mounts(id) ON DELETE CASCADE,
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  days_mask INTEGER NOT NULL,
  start_min INTEGER NOT NULL,
  end_min INTEGER NOT NULL,
  priority INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  detail TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS outputs (
  id TEXT PRIMARY KEY,
  station_id TEXT NOT NULL REFERENCES stations(id) ON DELETE CASCADE,
  mount_id TEXT NOT NULL REFERENCES mounts(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  host TEXT NOT NULL,
  voice_port INTEGER NOT NULL DEFAULT 9987,
  server_password TEXT NOT NULL DEFAULT '',
  nickname TEXT NOT NULL,
  channel_id INTEGER,
  channel_name TEXT NOT NULL DEFAULT '',
  channel_password TEXT NOT NULL DEFAULT '',
  identity TEXT NOT NULL DEFAULT '',
  volume INTEGER NOT NULL DEFAULT 80,
  enabled INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tokens_hash ON tokens(token_hash);
CREATE INDEX IF NOT EXISTS idx_recordings_mount ON recordings(mount_id);
CREATE INDEX IF NOT EXISTS idx_outputs_station ON outputs(station_id);
"#;
