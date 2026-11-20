//! AutoDJ deck. The playlist is the source of the queue; skip, move, and
//! clear change what the mount plays next. Settings are read from SQLite on
//! each call so the control API and this process stay on the same row.

use crate::engine::{QueueItem, QueueSnapshot, RepeatMode, Track, TrackSource};
use crate::error::Error;
use crate::media;
use crate::store::{self, Media, Mount, Store};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Library {
    store: Arc<Store>,
    root: PathBuf,
    tz_offset_min: i32,
    decks: Mutex<HashMap<String, Deck>>,
}

#[derive(Clone)]
struct Item {
    media_id: String,
    title: String,
    artist: String,
    path: PathBuf,
    mime: String,
    duration_ms: u64,
}

struct Deck {
    sig: String,
    upcoming: VecDeque<Item>,
    current: Option<Item>,
    history: VecDeque<Item>,
    started: Instant,
    gen: u64,
    autodj: bool,
    repeat: RepeatMode,
    shuffle: bool,
    held: bool,
    base: Vec<Item>,
    force_advance: bool,
    position_ms: u64,
    seek_gen: u64,
}

impl Deck {
    fn new() -> Self {
        Self {
            sig: String::new(),
            upcoming: VecDeque::new(),
            current: None,
            history: VecDeque::new(),
            started: Instant::now(),
            gen: 0,
            autodj: true,
            repeat: RepeatMode::All,
            shuffle: false,
            held: false,
            base: Vec::new(),
            force_advance: false,
            position_ms: 0,
            seek_gen: 0,
        }
    }
}

impl Library {
    pub fn new(store: Arc<Store>, root: impl Into<PathBuf>, tz_offset_min: i32) -> Self {
        Self { store, root: root.into(), tz_offset_min, decks: Mutex::new(HashMap::new()) }
    }

    fn with_deck<T>(&self, mount_id: &str, f: impl FnOnce(&mut Deck) -> Result<T, Error>) -> Result<T, Error> {
        let mut decks = self.decks.lock().expect("deck");
        if !decks.contains_key(mount_id) {
            decks.insert(mount_id.to_string(), Deck::new());
        }
        let deck = decks.get_mut(mount_id).expect("deck");
        self.sync(deck, mount_id)?;
        f(deck)
    }

    fn sync(&self, deck: &mut Deck, mount_id: &str) -> Result<(), Error> {
        let mount = self.store.mount(mount_id)?.ok_or(Error::NotFound)?;
        deck.autodj = mount.autodj;
        deck.repeat = RepeatMode::parse(&mount.repeat).unwrap_or(RepeatMode::All);
        deck.shuffle = mount.shuffle;
        let unix = store::now_unix() + self.tz_offset_min as i64 * 60;
        let (pid, tracks) = self.store.tracks_for_mount(mount_id, unix)?;
        let ids: Vec<&str> = tracks.iter().map(|m| m.id.as_str()).collect();
        let sig = format!(
            "{:?}|{}|{:?}|{:?}|{}|{}",
            pid,
            ids.join(","),
            mount.intro_media_id,
            mount.jingle_media_id,
            mount.jingle_every,
            mount.shuffle as u8
        );
        if deck.sig != sig {
            deck.sig = sig;
            deck.held = false;
            deck.base = self.expand(&mount, &tracks)?;
            refill(deck);
        }
        Ok(())
    }

    fn expand(&self, mount: &Mount, tracks: &[Media]) -> Result<Vec<Item>, Error> {
        let mut out = Vec::new();
        if let Some(id) = mount.intro_media_id.as_deref() {
            if let Some(item) = self.item_by_id(&mount.station_id, id)? {
                out.push(item);
            }
        }
        let jingle = match mount.jingle_media_id.as_deref() {
            Some(id) if mount.jingle_every > 0 => self.item_by_id(&mount.station_id, id)?,
            _ => None,
        };
        for (i, track) in tracks.iter().enumerate() {
            out.push(self.item_from_media(track));
            if let Some(jingle) = &jingle {
                if mount.jingle_every > 0 && ((i as i64 + 1) % mount.jingle_every) == 0 {
                    out.push(jingle.clone());
                }
            }
        }
        Ok(out)
    }

    fn item_by_id(&self, station_id: &str, id: &str) -> Result<Option<Item>, Error> {
        let Some(media) = self.store.media(id)? else { return Ok(None) };
        if media.station_id != station_id {
            return Ok(None);
        }
        Ok(Some(self.item_from_media(&media)))
    }

    fn item_from_media(&self, media: &Media) -> Item {
        let path = self.root.join(&media.id);
        let duration_ms = std::fs::read(&path).map(|b| media::duration_ms(&b, &media.mime)).unwrap_or(0);
        Item {
            media_id: media.id.clone(),
            title: if media.title.is_empty() { media.filename.clone() } else { media.title.clone() },
            artist: media.artist.clone(),
            path,
            mime: media.mime.clone(),
            duration_ms,
        }
    }
}

fn refill(deck: &mut Deck) {
    let mut items = deck.base.clone();
    if deck.shuffle {
        shuffle_items(&mut items);
    }
    deck.upcoming = VecDeque::from(items);
}

fn shuffle_items(items: &mut [Item]) {
    if items.len() < 2 {
        return;
    }
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xA11CE)
        | 1;
    for i in (1..items.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (state >> 33) as usize % (i + 1);
        items.swap(i, j);
    }
}

fn to_track(item: &Item) -> Track {
    Track {
        title: item.title.clone(),
        artist: item.artist.clone(),
        path: item.path.clone(),
        mime: item.mime.clone(),
        media_id: item.media_id.clone(),
        duration_ms: item.duration_ms,
    }
}

fn position_now(deck: &Deck) -> u64 {
    if deck.current.is_none() {
        return 0;
    }
    let pos = deck.position_ms.saturating_add(deck.started.elapsed().as_millis() as u64);
    let dur = deck.current.as_ref().map(|it| it.duration_ms).unwrap_or(0);
    if dur == 0 { pos } else { pos.min(dur) }
}

fn arm(deck: &mut Deck, item: Item) {
    deck.current = Some(item);
    deck.started = Instant::now();
    deck.position_ms = 0;
}

fn to_queue(item: &Item, index: usize) -> QueueItem {
    QueueItem {
        index,
        media_id: item.media_id.clone(),
        title: item.title.clone(),
        artist: item.artist.clone(),
        duration_ms: item.duration_ms,
    }
}

impl TrackSource for Library {
    fn next(&self, mount_id: &str) -> Option<Track> {
        self.with_deck(mount_id, |deck| {
            if !deck.autodj {
                return Ok(None);
            }
            let advance = deck.force_advance;
            deck.force_advance = false;
            if !advance && deck.repeat == RepeatMode::One {
                if let Some(cur) = deck.current.clone() {
                    deck.position_ms = 0;
                    deck.started = Instant::now();
                    return Ok(Some(to_track(&cur)));
                }
            }
            if deck.upcoming.is_empty() {
                if deck.held || deck.repeat != RepeatMode::All {
                    return Ok(None);
                }
                refill(deck);
                if deck.upcoming.is_empty() {
                    return Ok(None);
                }
            }
            if let Some(cur) = deck.current.take() {
                deck.history.push_back(cur);
                while deck.history.len() > 30 {
                    deck.history.pop_front();
                }
            }
            let item = deck.upcoming.pop_front().expect("upcoming");
            arm(deck, item.clone());
            Ok(Some(to_track(&item)))
        }).ok().flatten()
    }

    fn play_generation(&self, mount_id: &str) -> u64 {
        self.decks.lock().expect("deck").get(mount_id).map(|d| d.gen).unwrap_or(0)
    }

    fn play_head(&self, mount_id: &str) -> Option<crate::engine::PlayHead> {
        let decks = self.decks.lock().expect("deck");
        let deck = decks.get(mount_id)?;
        let cur = deck.current.as_ref()?;
        Some(crate::engine::PlayHead {
            elapsed_ms: position_now(deck),
            duration_ms: cur.duration_ms,
            title: cur.title.clone(),
            artist: cur.artist.clone(),
            media_id: cur.media_id.clone(),
        })
    }

    fn queue_snapshot(&self, mount_id: &str) -> Result<QueueSnapshot, Error> {
        self.with_deck(mount_id, |deck| {
            Ok(QueueSnapshot {
                now: deck.current.as_ref().map(|it| to_queue(it, 0)),
                elapsed_ms: position_now(deck),
                duration_ms: deck.current.as_ref().map(|it| it.duration_ms).unwrap_or(0),
                upcoming: deck.upcoming.iter().enumerate().map(|(i, it)| to_queue(it, i)).collect(),
                history: deck.history.iter().rev().take(20).enumerate().map(|(i, it)| to_queue(it, i)).collect(),
            })
        })
    }

    fn skip(&self, mount_id: &str) -> Result<(), Error> {
        self.with_deck(mount_id, |deck| {
            deck.force_advance = true;
            deck.gen = deck.gen.wrapping_add(1);
            Ok(())
        })
    }

    fn previous(&self, mount_id: &str) -> Result<(), Error> {
        self.with_deck(mount_id, |deck| {
            if let Some(prev) = deck.history.pop_back() {
                if let Some(cur) = deck.current.take() {
                    deck.upcoming.push_front(cur);
                }
                deck.upcoming.push_front(prev);
            } else if let Some(cur) = deck.current.clone() {
                deck.upcoming.push_front(cur);
            }
            deck.force_advance = true;
            deck.gen = deck.gen.wrapping_add(1);
            Ok(())
        })
    }

    fn seek(&self, mount_id: &str, position_ms: u64) -> Result<u64, Error> {
        self.with_deck(mount_id, |deck| {
            let dur = deck.current.as_ref().ok_or_else(|| Error::Invalid("nothing is playing".into()))?.duration_ms;
            let pos = if dur == 0 { 0 } else { position_ms.min(dur) };
            deck.position_ms = pos;
            deck.started = Instant::now();
            deck.seek_gen = deck.seek_gen.wrapping_add(1);
            Ok(pos)
        })
    }

    fn seek_generation(&self, mount_id: &str) -> u64 {
        self.decks.lock().expect("deck").get(mount_id).map(|d| d.seek_gen).unwrap_or(0)
    }

    fn seek_offset_ms(&self, mount_id: &str) -> u64 {
        self.decks.lock().expect("deck").get(mount_id).map(|d| d.position_ms).unwrap_or(0)
    }

    fn jump(&self, mount_id: &str, media_id: &str) -> Result<(), Error> {
        if media_id.is_empty() {
            return Err(Error::Invalid("media_id required".into()));
        }
        let mount = self.store.mount(mount_id)?.ok_or(Error::NotFound)?;
        let item = self.item_by_id(&mount.station_id, media_id)?.ok_or(Error::NotFound)?;
        self.with_deck(mount_id, |deck| {
            deck.upcoming.push_front(item);
            deck.force_advance = true;
            deck.position_ms = 0;
            deck.gen = deck.gen.wrapping_add(1);
            deck.seek_gen = deck.seek_gen.wrapping_add(1);
            Ok(())
        })
    }

    fn play_file(&self, mount_id: &str, track: Track, jump: bool) -> Result<(), Error> {
        if track.media_id.is_empty() || track.path.as_os_str().is_empty() {
            return Err(Error::Invalid("recording required".into()));
        }
        let item = Item {
            media_id: track.media_id,
            title: if track.title.is_empty() { "Recording".into() } else { track.title },
            artist: track.artist,
            path: track.path,
            mime: if track.mime.is_empty() { "audio/mpeg".into() } else { track.mime },
            duration_ms: track.duration_ms,
        };
        self.with_deck(mount_id, |deck| {
            deck.upcoming.push_front(item);
            deck.held = false;
            if jump {
                deck.force_advance = true;
                deck.position_ms = 0;
                deck.gen = deck.gen.wrapping_add(1);
                deck.seek_gen = deck.seek_gen.wrapping_add(1);
            }
            Ok(())
        })
    }

    fn play_next(&self, mount_id: &str, media_id: &str) -> Result<(), Error> {
        if media_id.is_empty() {
            return Err(Error::Invalid("media_id required".into()));
        }
        let mount = self.store.mount(mount_id)?.ok_or(Error::NotFound)?;
        let item = self.item_by_id(&mount.station_id, media_id)?.ok_or(Error::NotFound)?;
        self.with_deck(mount_id, |deck| {
            deck.upcoming.push_front(item);
            Ok(())
        })
    }

    fn queue_next(&self, mount_id: &str, media_id: &str) -> Result<(), Error> {
        if media_id.is_empty() {
            return self.skip(mount_id);
        }
        let mount = self.store.mount(mount_id)?.ok_or(Error::NotFound)?;
        let item = self.item_by_id(&mount.station_id, media_id)?.ok_or(Error::NotFound)?;
        self.with_deck(mount_id, |deck| {
            deck.upcoming.push_front(item);
            deck.held = false;
            Ok(())
        })
    }

    fn queue_move(&self, mount_id: &str, from: usize, to: usize) -> Result<(), Error> {
        self.with_deck(mount_id, |deck| {
            if from >= deck.upcoming.len() || to >= deck.upcoming.len() {
                return Err(Error::Invalid("bad position".into()));
            }
            let item = deck.upcoming.remove(from).ok_or_else(|| Error::Invalid("bad position".into()))?;
            deck.upcoming.insert(to, item);
            Ok(())
        })
    }

    fn queue_remove(&self, mount_id: &str, index: usize) -> Result<(), Error> {
        self.with_deck(mount_id, |deck| {
            if index >= deck.upcoming.len() {
                return Err(Error::Invalid("bad position".into()));
            }
            deck.upcoming.remove(index);
            Ok(())
        })
    }

    fn queue_clear(&self, mount_id: &str) -> Result<(), Error> {
        self.with_deck(mount_id, |deck| {
            deck.upcoming.clear();
            deck.held = true;
            Ok(())
        })
    }
}

pub fn root_contains(root: &Path, path: &Path) -> bool {
    crate::media::inside_root(root, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Mount;

    fn deck_fixture(shuffle: bool, repeat: &str) -> (Library, String) {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("radio-deck-{}-{}-{}", std::process::id(), shuffle as u8, n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let store = Arc::new(Store::open(&dir.join("r.db")).unwrap());
        let station = store.create_station("Night", "night", "", "jazz").unwrap();
        let playlist = store.create_playlist(&station.id, "fallback").unwrap();
        for (title, artist) in [("Alpha", "A"), ("Bravo", "B"), ("Charlie", "C")] {
            let id = crate::auth::new_id();
            store.create_media(&Media {
                id: id.clone(),
                station_id: station.id.clone(),
                filename: format!("{title}.mp3"),
                mime: "audio/mpeg".into(),
                bytes: 32,
                title: title.into(),
                artist: artist.into(),
                created_at: store::now_ms(),
            }).unwrap();
            store.add_playlist_item(&playlist.id, &id).unwrap();
            std::fs::write(dir.join("media").join(&id), vec![0u8; 128]).unwrap();
        }
        let mount_id = crate::auth::new_id();
        store.create_mount(&Mount {
            id: mount_id.clone(),
            station_id: station.id,
            path: "/live".into(),
            source_user: "source".into(),
            source_password_hash: "x".into(),
            format: "mp3".into(),
            bitrate: 128_000,
            max_listeners: 10,
            max_bitrate: 320_000,
            allow_takeover: true,
            fallback_playlist_id: Some(playlist.id),
            created_at: store::now_ms(),
            crossfade_ms: 250,
            autodj: true,
            shuffle,
            repeat: repeat.into(),
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
            record: false,
            record_max_ms: 3_600_000,
        }).unwrap();
        let lib = Library::new(store, dir.join("media"), 0);
        (lib, mount_id)
    }

    #[test]
    fn shuffle_off_plays_sequentially_and_repeats() {
        let (lib, id) = deck_fixture(false, "all");
        let titles: Vec<_> = (0..4).map(|_| lib.next(&id).unwrap().title).collect();
        assert_eq!(titles, vec!["Alpha", "Bravo", "Charlie", "Alpha"]);
    }

    #[test]
    fn skip_advances_and_queue_move_reorders() {
        let (lib, id) = deck_fixture(false, "all");
        let snap = lib.queue_snapshot(&id).unwrap();
        assert_eq!(
            snap.upcoming.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Bravo", "Charlie"]
        );
        lib.queue_move(&id, 0, 2).unwrap();
        let snap = lib.queue_snapshot(&id).unwrap();
        assert_eq!(
            snap.upcoming.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Bravo", "Charlie", "Alpha"]
        );
        lib.queue_remove(&id, 0).unwrap();
        let snap = lib.queue_snapshot(&id).unwrap();
        assert_eq!(snap.upcoming[0].title, "Charlie");

        let (lib, id) = deck_fixture(false, "all");
        let first = lib.next(&id).unwrap();
        assert_eq!(first.title, "Alpha");
        let before = lib.play_generation(&id);
        lib.skip(&id).unwrap();
        assert_ne!(lib.play_generation(&id), before);
        let second = lib.next(&id).unwrap();
        assert_eq!(second.title, "Bravo");
        lib.previous(&id).unwrap();
        let back = lib.next(&id).unwrap();
        assert_eq!(back.title, "Alpha");

        lib.queue_clear(&id).unwrap();
        assert!(lib.queue_snapshot(&id).unwrap().upcoming.is_empty());
        assert!(lib.next(&id).is_none(), "a cleared queue stays empty until it is refilled");
    }

    #[test]
    fn seek_clamps_to_duration() {
        let (lib, id) = deck_fixture(false, "all");
        let first = lib.next(&id).unwrap();
        assert_eq!(first.title, "Alpha");
        assert_eq!(lib.seek(&id, 100_000).unwrap(), 8_000);
        assert_eq!(lib.seek_offset_ms(&id), 8_000);
        assert_eq!(lib.seek(&id, 100).unwrap(), 100);
        let head = lib.play_head(&id).unwrap();
        assert!(head.elapsed_ms >= 100 && head.elapsed_ms < 8_000);
    }

    #[test]
    fn jump_changes_the_current_track() {
        let (lib, id) = deck_fixture(false, "all");
        let first = lib.next(&id).unwrap();
        assert_eq!(first.title, "Alpha");
        let charlie = lib.queue_snapshot(&id).unwrap().upcoming.iter().find(|i| i.title == "Charlie").unwrap().media_id.clone();
        lib.jump(&id, &charlie).unwrap();
        let now = lib.next(&id).unwrap();
        assert_eq!(now.title, "Charlie");
    }

    #[test]
    fn play_next_orders_the_upcoming_track() {
        let (lib, id) = deck_fixture(false, "all");
        let first = lib.next(&id).unwrap();
        assert_eq!(first.title, "Alpha");
        let snap = lib.queue_snapshot(&id).unwrap();
        let charlie = snap.upcoming.iter().find(|i| i.title == "Charlie").unwrap().media_id.clone();
        lib.play_next(&id, &charlie).unwrap();
        let snap = lib.queue_snapshot(&id).unwrap();
        assert_eq!(snap.now.as_ref().unwrap().title, "Alpha");
        assert_eq!(snap.upcoming[0].title, "Charlie");
        assert_eq!(snap.upcoming[1].title, "Bravo");
        let next = lib.next(&id).unwrap();
        assert_eq!(next.title, "Charlie");
    }

    #[test]
    fn repeat_one_replays_until_skip() {
        let (lib, id) = deck_fixture(false, "one");
        assert_eq!(lib.next(&id).unwrap().title, "Alpha");
        assert_eq!(lib.next(&id).unwrap().title, "Alpha");
        lib.skip(&id).unwrap();
        assert_eq!(lib.next(&id).unwrap().title, "Bravo");
    }

    #[test]
    fn a_recording_can_be_queued_or_jumped() {
        let (lib, id) = deck_fixture(false, "all");
        let path = std::env::temp_dir().join(format!("rec-jump-{id}"));
        std::fs::write(&path, b"recorded-bytes").unwrap();
        let queued = crate::engine::Track {
            title: "Recording".into(),
            artist: String::new(),
            path: path.clone(),
            mime: "audio/mpeg".into(),
            media_id: "r".repeat(32),
            duration_ms: 1_500,
        };
        lib.play_file(&id, queued, false).unwrap();
        let snap = lib.queue_snapshot(&id).unwrap();
        assert_eq!(snap.upcoming[0].title, "Recording");
        assert_eq!(snap.upcoming[0].media_id, "r".repeat(32));
        let now = crate::engine::Track {
            title: "Live take".into(),
            artist: String::new(),
            path,
            mime: "audio/mpeg".into(),
            media_id: "n".repeat(32),
            duration_ms: 1_500,
        };
        lib.play_file(&id, now, true).unwrap();
        assert_eq!(lib.next(&id).unwrap().title, "Live take");
    }
}
