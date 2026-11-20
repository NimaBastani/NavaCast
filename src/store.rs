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
}

#[derive(Clone, Debug)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub genre: String,
    pub created_at: i64,
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

#[derive(Clone, Debug)]
pub struct PlayCount {
    pub media_id: String,
    pub title: String,
    pub artist: String,
    pub plays: i64,
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
        Ok(User { id, username: username.into(), password_hash: password_hash.into(), role: role.into() })
    }

    pub fn user_by_name(&self, username: &str) -> Result<Option<User>, Error> {
        let c = self.lock();
        let row = c.query_row(
            "SELECT id, username, password_hash, role FROM users WHERE username=?1",
            params![username],
            map_user,
        ).optional()?;
        Ok(row)
    }

    pub fn list_users(&self) -> Result<Vec<User>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, username, password_hash, role FROM users ORDER BY username")?;
        let rows = stmt.query_map([], map_user)?;
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
        let c = self.lock();
        let row = c.query_row(
            "SELECT u.id, u.username, u.password_hash, u.role
             FROM tokens t JOIN users u ON u.id=t.user_id
             WHERE t.token_hash=?1 AND t.revoked_at IS NULL",
            params![token_hash],
            map_user,
        ).optional()?;
        Ok(row)
    }

    pub fn create_station(&self, name: &str, slug: &str, description: &str, genre: &str) -> Result<Station, Error> {
        let id = auth::new_id();
        let created = now_ms();
        let c = self.lock();
        c.execute(
            "INSERT INTO stations (id, name, slug, description, genre, created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, name, slug, description, genre, created],
        )?;
        Ok(Station { id, name: name.into(), slug: slug.into(), description: description.into(), genre: genre.into(), created_at: created })
    }

    pub fn list_stations(&self) -> Result<Vec<Station>, Error> {
        let c = self.lock();
        let mut stmt = c.prepare("SELECT id, name, slug, description, genre, created_at FROM stations ORDER BY name")?;
        let rows = stmt.query_map([], map_station)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn station(&self, id: &str) -> Result<Option<Station>, Error> {
        let c = self.lock();
        Ok(c.query_row("SELECT id, name, slug, description, genre, created_at FROM stations WHERE id=?1", params![id], map_station).optional()?)
    }

    pub fn station_by_slug(&self, slug: &str) -> Result<Option<Station>, Error> {
        let c = self.lock();
        Ok(c.query_row("SELECT id, name, slug, description, genre, created_at FROM stations WHERE slug=?1", params![slug], map_station).optional()?)
    }

    pub fn delete_station(&self, id: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute("DELETE FROM stations WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn create_mount(&self, m: &Mount) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO mounts (id, station_id, path, source_user, source_password_hash, format, bitrate, max_listeners, max_bitrate, allow_takeover, fallback_playlist_id, created_at, crossfade_ms, autodj, shuffle, repeat, jingle_every, jingle_media_id, intro_media_id, gain_db, sample_rate, channels, buffer_ms, hls_segment_ms, repeat_mode, encoder, relay_url)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
            params![m.id, m.station_id, m.path, m.source_user, m.source_password_hash, m.format, m.bitrate, m.max_listeners, m.max_bitrate, m.allow_takeover as i64, m.fallback_playlist_id, m.created_at, m.crossfade_ms, m.autodj as i64, m.shuffle as i64, repeat_int(&m.repeat), m.jingle_every, m.jingle_media_id, m.intro_media_id, m.gain_db, m.sample_rate, m.channels, m.buffer_ms, m.hls_segment_ms, normalize_repeat(&m.repeat), m.encoder, m.relay_url],
        )?;
        Ok(())
    }

    pub fn update_mount(&self, m: &Mount) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "UPDATE mounts SET path=?2, source_user=?3, source_password_hash=?4, format=?5, bitrate=?6, max_listeners=?7, max_bitrate=?8, allow_takeover=?9, fallback_playlist_id=?10, crossfade_ms=?11, autodj=?12, shuffle=?13, repeat=?14, jingle_every=?15, jingle_media_id=?16, intro_media_id=?17, gain_db=?18, sample_rate=?19, channels=?20, buffer_ms=?21, hls_segment_ms=?22, repeat_mode=?23, encoder=?24, relay_url=?25 WHERE id=?1",
            params![m.id, m.path, m.source_user, m.source_password_hash, m.format, m.bitrate, m.max_listeners, m.max_bitrate, m.allow_takeover as i64, m.fallback_playlist_id, m.crossfade_ms, m.autodj as i64, m.shuffle as i64, repeat_int(&m.repeat), m.jingle_every, m.jingle_media_id, m.intro_media_id, m.gain_db, m.sample_rate, m.channels, m.buffer_ms, m.hls_segment_ms, normalize_repeat(&m.repeat), m.encoder, m.relay_url],
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

    /// Raise the stored listener peak. A lower count leaves the peak in place.
    /// Listener ticks are not audited.
    pub fn note_peak(&self, mount_id: &str, listeners: i64) -> Result<i64, Error> {
        let listeners = listeners.max(0);
        let c = self.lock();
        c.execute(
            "INSERT INTO listener_peaks (mount_id, peak) VALUES (?1, ?2)
             ON CONFLICT(mount_id) DO UPDATE SET peak = MAX(listener_peaks.peak, excluded.peak)",
            params![mount_id, listeners],
        )?;
        let peak: i64 = c.query_row(
            "SELECT peak FROM listener_peaks WHERE mount_id=?1",
            params![mount_id],
            |r| r.get(0),
        )?;
        Ok(peak)
    }

    pub fn peak_listeners(&self, mount_id: &str) -> Result<i64, Error> {
        let c = self.lock();
        let peak = c.query_row(
            "SELECT peak FROM listener_peaks WHERE mount_id=?1",
            params![mount_id],
            |r| r.get(0),
        ).optional()?;
        Ok(peak.unwrap_or(0))
    }

    /// Count one play that AutoDJ or a jump already started. Empty ids are ignored.
    pub fn note_play(&self, mount_id: &str, media_id: &str, title: &str, artist: &str) -> Result<(), Error> {
        if media_id.is_empty() {
            return Ok(());
        }
        let title: String = title.chars().take(200).collect();
        let artist: String = artist.chars().take(200).collect();
        let c = self.lock();
        c.execute(
            "INSERT INTO track_plays (mount_id, media_id, title, artist, plays) VALUES (?1,?2,?3,?4,1)
             ON CONFLICT(mount_id, media_id) DO UPDATE SET
               plays = track_plays.plays + 1,
               title = excluded.title,
               artist = excluded.artist",
            params![mount_id, media_id, title, artist],
        )?;
        Ok(())
    }

    pub fn top_tracks(&self, mount_id: &str, limit: i64) -> Result<Vec<PlayCount>, Error> {
        let limit = limit.clamp(1, 20);
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT media_id, title, artist, plays FROM track_plays
             WHERE mount_id=?1 ORDER BY plays DESC, title ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![mount_id, limit], |r| {
            Ok(PlayCount { media_id: r.get(0)?, title: r.get(1)?, artist: r.get(2)?, plays: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn audit(&self, actor: &str, action: &str, detail: &str) -> Result<(), Error> {
        let c = self.lock();
        c.execute(
            "INSERT INTO audit (at, actor, action, detail) VALUES (?1,?2,?3,?4)",
            params![now_ms(), actor, action, detail],
        )?;
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

fn map_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User { id: r.get(0)?, username: r.get(1)?, password_hash: r.get(2)?, role: r.get(3)? })
}
fn map_station(r: &rusqlite::Row<'_>) -> rusqlite::Result<Station> {
    Ok(Station { id: r.get(0)?, name: r.get(1)?, slug: r.get(2)?, description: r.get(3)?, genre: r.get(4)?, created_at: r.get(5)? })
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
fn map_media(r: &rusqlite::Row<'_>) -> rusqlite::Result<Media> {
    Ok(Media { id: r.get(0)?, station_id: r.get(1)?, filename: r.get(2)?, mime: r.get(3)?, bytes: r.get(4)?, title: r.get(5)?, artist: r.get(6)?, created_at: r.get(7)? })
}
fn map_sched(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?, mount_id: r.get(1)?, playlist_id: r.get(2)?, days_mask: r.get(3)?,
        start_min: r.get(4)?, end_min: r.get(5)?, priority: r.get(6)?, enabled: r.get::<_, i64>(7)? != 0,
    })
}

const MOUNT_SQL: &str = "SELECT id, station_id, path, source_user, source_password_hash, format, bitrate, max_listeners, max_bitrate, allow_takeover, fallback_playlist_id, created_at, crossfade_ms, autodj, shuffle, repeat, jingle_every, jingle_media_id, intro_media_id, gain_db, sample_rate, channels, buffer_ms, hls_segment_ms, repeat_mode, encoder, relay_url FROM mounts";

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
    ];
    for sql in alters {
        let _ = conn.execute(sql, []);
    }
    if conn.execute("ALTER TABLE mounts ADD COLUMN repeat_mode TEXT NOT NULL DEFAULT 'all'", []).is_ok() {
        let _ = conn.execute("UPDATE mounts SET repeat_mode='off' WHERE repeat=0", []);
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
  relay_url TEXT NOT NULL DEFAULT ''
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
CREATE TABLE IF NOT EXISTS listener_peaks (
  mount_id TEXT PRIMARY KEY REFERENCES mounts(id) ON DELETE CASCADE,
  peak INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS track_plays (
  mount_id TEXT NOT NULL REFERENCES mounts(id) ON DELETE CASCADE,
  media_id TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  plays INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (mount_id, media_id)
);
CREATE INDEX IF NOT EXISTS idx_tokens_hash ON tokens(token_hash);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn mount_row(id: &str, station_id: &str, path: &str) -> Mount {
        Mount {
            id: id.into(),
            station_id: station_id.into(),
            path: path.into(),
            source_user: "source".into(),
            source_password_hash: "x".into(),
            format: "mp3".into(),
            bitrate: 128_000,
            max_listeners: 10,
            max_bitrate: 0,
            allow_takeover: false,
            fallback_playlist_id: None,
            created_at: now_ms(),
            crossfade_ms: 0,
            autodj: true,
            shuffle: false,
            repeat: "all".into(),
            jingle_every: 0,
            jingle_media_id: None,
            intro_media_id: None,
            gain_db: 0.0,
            sample_rate: 44_100,
            channels: 2,
            buffer_ms: 250,
            hls_segment_ms: 1_000,
            encoder: "ffmpeg".into(),
            relay_url: String::new(),
        }
    }

    fn open_mount() -> (Store, String, String) {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("radio-stats-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("r.db")).unwrap();
        let station = store.create_station("Night", &format!("night-{n}"), "", "jazz").unwrap();
        let a = auth::new_id();
        let b = auth::new_id();
        store.create_mount(&mount_row(&a, &station.id, "/live")).unwrap();
        store.create_mount(&mount_row(&b, &station.id, "/late")).unwrap();
        (store, a, b)
    }

    #[test]
    fn peak_only_rises() {
        let (store, mount, other) = open_mount();
        assert_eq!(store.note_peak(&mount, 2).unwrap(), 2);
        assert_eq!(store.note_peak(&mount, 5).unwrap(), 5);
        assert_eq!(store.note_peak(&mount, 1).unwrap(), 5);
        assert_eq!(store.note_peak(&mount, 5).unwrap(), 5);
        assert_eq!(store.peak_listeners(&mount).unwrap(), 5);
        assert_eq!(store.note_peak(&other, 1).unwrap(), 1);
        assert_eq!(store.peak_listeners(&mount).unwrap(), 5);
        assert!(store.list_audit(20).unwrap().is_empty());
    }

    #[test]
    fn top_tracks_rank_by_plays() {
        let (store, mount, other) = open_mount();
        store.note_play(&mount, "b", "Bravo", "B").unwrap();
        store.note_play(&mount, "a", "Alpha", "A").unwrap();
        store.note_play(&mount, "a", "Alpha", "A").unwrap();
        store.note_play(&mount, "c", "Charlie", "C").unwrap();
        store.note_play(&mount, "c", "Charlie", "C").unwrap();
        store.note_play(&mount, "c", "Charlie", "C").unwrap();
        store.note_play(&mount, "", "skip", "").unwrap();
        store.note_play(&other, "a", "Alpha", "A").unwrap();

        let top = store.top_tracks(&mount, 10).unwrap();
        let ranked: Vec<_> = top.iter().map(|t| (t.title.as_str(), t.artist.as_str(), t.plays)).collect();
        assert_eq!(ranked, vec![("Charlie", "C", 3), ("Alpha", "A", 2), ("Bravo", "B", 1)]);
        assert_eq!(store.top_tracks(&other, 10).unwrap().len(), 1);
        assert_eq!(store.top_tracks(&mount, 1).unwrap().len(), 1);
        assert_eq!(store.top_tracks(&mount, 1).unwrap()[0].title, "Charlie");
        assert!(store.list_audit(20).unwrap().is_empty());
    }
}
