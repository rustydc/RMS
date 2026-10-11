//! Random Mix (`Slim::Plugin::RandomPlay`): play random songs, albums, artists or years
//! from the library, optionally limited to some genres.
//!
//! `randomplay <tracks|albums|contributors|year>` starts a mix.  In continuous mode (the
//! default) it keeps going: when a new song starts, played songs beyond `oldtracks` are
//! removed and the queue is topped up to `newtracks` upcoming songs (song and year mixes) or
//! with the next album / artist when the last one is playing.  The mix ends with
//! `randomplay disable`, when the queue is replaced (`playlist clear / play / loadtracks…`,
//! as in Lyrion) or, more generally, once none of the mix's songs is left in the queue.
//!
//! Prefs (Lyrion's names): `plugin.randomplay:newtracks`, `oldtracks`, `continuous`,
//! `exclude_genres` (genre names left out of every mix).

use crate::control::dispatch::{self, DispatchTable, Handler};
use crate::control::request::{Request, Source};
use crate::notify::Notification;
use crate::player::Player;
use crate::server::Server;
use crate::strings::s;
use parking_lot::Mutex;
use rms_library::mix::Seed;
use serde_json::{json, Value};
use slimproto::MacAddress;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{debug, info};

pub const PREF_NEWTRACKS: &str = "plugin.randomplay:newtracks";
pub const PREF_OLDTRACKS: &str = "plugin.randomplay:oldtracks";
pub const PREF_CONTINUOUS: &str = "plugin.randomplay:continuous";
pub const PREF_EXCLUDE_GENRES: &str = "plugin.randomplay:exclude_genres";

/// The source of the plugin's own commands, which it doesn't react to.
const SOURCE: Source = Source::Plugin("randomplay");

/// Playlist commands that end a mix (`$stopcommands`).
const STOP_COMMANDS: &[&str] = &[
    "clear",
    "loadtracks",
    "playtracks",
    "load",
    "play",
    "loadalbum",
    "playalbum",
];

/// Seconds before topping up when enough songs are still queued (`_addTracksLater`).
const ADD_LATER_SECS: u64 = 10;

/// What a mix plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Contributor,
    Year,
}

impl Kind {
    /// CLI mode (`tracks`, `albums`, `contributors`, `artists`, `year`) or `randomplay://` type.
    pub fn parse(mode: &str) -> Option<Kind> {
        match mode.to_ascii_lowercase().as_str() {
            "track" | "tracks" => Some(Kind::Track),
            "album" | "albums" => Some(Kind::Album),
            "contributor" | "contributors" | "artist" | "artists" => Some(Kind::Contributor),
            "year" | "years" => Some(Kind::Year),
            _ => None,
        }
    }

    /// Lyrion's type name, as `randomplayisactive` reports it.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Album => "album",
            Kind::Contributor => "contributor",
            Kind::Year => "year",
        }
    }

    fn cli(self) -> &'static str {
        match self {
            Kind::Track => "tracks",
            Kind::Album => "albums",
            Kind::Contributor => "contributors",
            Kind::Year => "year",
        }
    }

    /// "Song Mix", "Album Mix", …
    fn title(self) -> String {
        s(&format!("PLUGIN_RANDOM_{}", self.name().to_uppercase()))
    }

    /// Song and year mixes add songs a few at a time; album and artist mixes a whole item.
    fn by_song(self) -> bool {
        matches!(self, Kind::Track | Kind::Year)
    }

    const ALL: [Kind; 4] = [Kind::Track, Kind::Album, Kind::Contributor, Kind::Year];
}

/// A sync group's running mix.
struct Mix {
    kind: Kind,
    /// Genres named by `genres:` / `genre_id:`; replace the excluded-genres pref when set.
    include_genres: Vec<String>,
    /// Shuffled ids still to play: songs (song and year mixes), albums or artists.
    pending: Vec<i64>,
    /// Shuffled years still to play (year mixes).
    years: Vec<i64>,
    /// Songs the mix added.  Once none of them is in the queue, the mix is over.
    added: HashSet<i64>,
}

/// Running mixes by sync group master, and their pending top-ups.
#[derive(Default)]
pub struct RandomPlay {
    mixes: Mutex<HashMap<MacAddress, Mix>>,
    timers: Mutex<HashMap<MacAddress, JoinHandle<()>>>,
}

impl RandomPlay {
    /// The mix a sync group is playing (`active`).
    pub fn active(&self, master: MacAddress) -> Option<Kind> {
        self.mixes.lock().get(&master).map(|m| m.kind)
    }

    fn cancel_timer(&self, master: MacAddress) {
        if let Some(t) = self.timers.lock().remove(&master) {
            t.abort();
        }
    }
}

fn master_of(p: &Arc<Player>) -> Arc<Player> {
    p.controller().lock().master().unwrap_or_else(|| p.clone())
}

fn new_tracks(server: &Server) -> usize {
    server.prefs.server.get_i64(PREF_NEWTRACKS).clamp(1, 100) as usize
}

/// Played songs to keep before the current one; `None` (an empty pref) keeps them all.
fn old_tracks(server: &Server) -> Option<usize> {
    match server.prefs.server.get(PREF_OLDTRACKS) {
        Some(Value::String(v)) if v.trim().is_empty() => None,
        Some(Value::Null) => None,
        _ => Some(server.prefs.server.get_i64(PREF_OLDTRACKS).max(0) as usize),
    }
}

fn continuous(server: &Server) -> bool {
    server.prefs.server.get_i64(PREF_CONTINUOUS) != 0
}

/// The genre ids a mix is limited to (`getFilteredGenres`): the named ones, else all but the
/// excluded ones; no limit when that is every genre or none.
fn genre_ids(server: &Server, include: &[String]) -> Vec<i64> {
    let all = server.library.genres();
    let enabled: Vec<i64> = if include.is_empty() {
        let excluded: HashSet<String> = server
            .prefs
            .server
            .get_string_list(PREF_EXCLUDE_GENRES)
            .into_iter()
            .collect();
        all.iter()
            .filter(|(_, name)| !excluded.contains(name))
            .map(|(id, _)| *id)
            .collect()
    } else {
        all.iter()
            .filter(|(_, name)| include.contains(name))
            .map(|(id, _)| *id)
            .collect()
    };
    if enabled.len() == all.len() {
        Vec::new()
    } else {
        enabled
    }
}

/// Take up to `n` ids from the mix's shuffled list, refilling it once when it runs out
/// (`findAndAdd` / `getIdList`).  Year mixes move on to the next random year.
async fn take(server: &Arc<Server>, master: MacAddress, n: usize) -> Vec<i64> {
    let mut out = Vec::new();
    let mut refilled = false;
    loop {
        let (kind, include) = {
            let mut mixes = server.randomplay.mixes.lock();
            let Some(mix) = mixes.get_mut(&master) else {
                return out;
            };
            let k = (n - out.len()).min(mix.pending.len());
            out.extend(mix.pending.drain(..k));
            if out.len() == n || refilled {
                return out;
            }
            (mix.kind, mix.include_genres.clone())
        };
        refilled = true;
        let srv = server.clone();
        let years_left = server
            .randomplay
            .mixes
            .lock()
            .get(&master)
            .map(|m| m.years.clone())
            .unwrap_or_default();
        let (pending, years) = tokio::task::spawn_blocking(move || {
            let genres = genre_ids(&srv, &include);
            let lib = &srv.library;
            match kind {
                Kind::Track => (lib.shuffled_tracks(&genres, None), years_left),
                Kind::Album => (lib.shuffled_albums(&genres), years_left),
                Kind::Contributor => (lib.shuffled_artists(&genres), years_left),
                Kind::Year => {
                    let mut years = years_left;
                    if years.is_empty() {
                        years = lib.shuffled_years(&genres);
                    }
                    match years.pop() {
                        Some(y) => (lib.shuffled_tracks(&genres, Some(y)), years),
                        None => (Vec::new(), years),
                    }
                }
            }
        })
        .await
        .unwrap_or_default();
        let mut mixes = server.randomplay.mixes.lock();
        let Some(mix) = mixes.get_mut(&master) else {
            return out;
        };
        mix.pending = pending;
        mix.years = years;
    }
}

/// Add `n` items of the mix to the queue, or replace the queue with them.  Returns the
/// number of songs added.
async fn find_and_add(server: &Arc<Server>, master: &Arc<Player>, n: usize, add: bool) -> usize {
    let Some(kind) = server.randomplay.active(master.id) else {
        return 0;
    };
    let ids = take(server, master.id, n).await;
    let lib = server.library.clone();
    let tracks: Vec<i64> = tokio::task::spawn_blocking(move || {
        ids.into_iter()
            .flat_map(|id| match kind {
                Kind::Track | Kind::Year => vec![id],
                Kind::Album => lib.seed_tracks(Seed::Album(id)),
                Kind::Contributor => lib.seed_tracks(Seed::Artist(id)),
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    if tracks.is_empty() {
        info!("{}: Random Mix: no {} found", master.id, kind.cli());
        return 0;
    }
    // recorded before the songs are queued: their newsong must find them
    if let Some(mix) = server.randomplay.mixes.lock().get_mut(&master.id) {
        mix.added.extend(&tracks);
    }
    let ids: Vec<String> = tracks.iter().map(|id| id.to_string()).collect();
    let what = format!("track.id={}", ids.join(","));
    let cmd = if add { "addtracks" } else { "loadtracks" };
    exec(server, master, &["playlist", cmd, &what]).await;
    tracks.len()
}

/// `playRandom`: start a mix (`add` = append rather than replace the queue), or top it up.
async fn play_random(server: &Arc<Server>, master: &Arc<Player>, add: bool, top_up: bool) {
    let Some(kind) = server.randomplay.active(master.id) else {
        return;
    };
    let (count, index) = {
        let c = master.controller();
        let c = c.lock();
        (c.playlist.len(), c.streaming_song_index())
    };
    let remaining = count as i64 - index as i64 - 1;
    let newtracks = new_tracks(server);
    let n = if kind.by_song() {
        if !add {
            newtracks
        } else if remaining < newtracks as i64 {
            newtracks - remaining.max(0) as usize
        } else {
            debug!("{}: Random Mix: {remaining} songs remaining", master.id);
            0
        }
    } else if !top_up || remaining <= 0 {
        1
    } else {
        0
    };
    if n == 0 {
        return;
    }
    let added = find_and_add(server, master, n, add).await;
    // one song queued and nothing to follow it: add some more
    if added > 0
        && kind.by_song()
        && continuous(server)
        && master.controller().lock().playlist.len() == 1
    {
        find_and_add(server, master, n, true).await;
    }
    if added > 0 && (!top_up || !kind.by_song()) {
        let title = if kind == Kind::Track {
            kind.title()
        } else {
            s(&format!(
                "PLUGIN_RANDOM_{}_ITEM",
                kind.name().to_uppercase()
            ))
        };
        let first = if add {
            "ADDING_TO_PLAYLIST"
        } else {
            "NOW_PLAYING"
        };
        crate::control::commands::show_briefly(server, master, vec![s(first), title], 2.0);
    }
}

/// Start a mix on a player's sync group (`randomplay <mode>`).
pub async fn start(
    server: &Arc<Server>,
    p: &Arc<Player>,
    kind: Kind,
    include_genres: Vec<String>,
    add: bool,
) {
    let master = master_of(p);
    server.randomplay.cancel_timer(master.id);
    server.randomplay.mixes.lock().insert(
        master.id,
        Mix {
            kind,
            include_genres,
            pending: Vec::new(),
            years: Vec::new(),
            added: HashSet::new(),
        },
    );
    info!("{}: Random Mix: starting {} mix", master.id, kind.name());
    play_random(server, &master, add, false).await;
    if !continuous(server) {
        server.randomplay.mixes.lock().remove(&master.id);
    }
}

/// End a sync group's mix; `announce` shows "Random Mix Stopped".
pub fn stop(server: &Arc<Server>, master: &Arc<Player>, announce: bool) {
    server.randomplay.cancel_timer(master.id);
    if server.randomplay.mixes.lock().remove(&master.id).is_none() {
        return;
    }
    info!("{}: Random Mix: stopped", master.id);
    if announce {
        crate::control::commands::show_briefly(
            server,
            master,
            vec![s("PLUGIN_RANDOMPLAY"), s("PLUGIN_RANDOM_DISABLED")],
            3.0,
        );
    }
}

async fn exec(server: &Arc<Server>, p: &Arc<Player>, tokens: &[&str]) -> Request {
    let tokens = tokens.iter().map(|t| t.to_string()).collect();
    dispatch::execute_tokens(server, Some(p.id), tokens, SOURCE).await
}

// ---- following the queue ---------------------------------------------------------------

/// `commandCallback`: keep running mixes going, and end them when the queue is replaced.
pub async fn run(server: Arc<Server>) {
    let mut rx = server.notifier.subscribe();
    loop {
        let n = match rx.recv().await {
            Ok(n) => n,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        };
        on_notification(&server, &n);
    }
}

fn on_notification(server: &Arc<Server>, n: &Notification) {
    if n.source == SOURCE || !matches!(n.first(), "playlist" | "playlistcontrol") {
        return;
    }
    let Some(p) = n.player.and_then(|m| server.player(&m)) else {
        return;
    };
    let master = master_of(&p);
    if server.randomplay.active(master.id).is_none() {
        return;
    }
    // `playlist play randomplay://…` (alarms, favorites) is what started this mix
    if n.word(2).starts_with("randomplay:") {
        return;
    }
    let replaced = match n.first() {
        "playlist" => STOP_COMMANDS.contains(&n.second()),
        _ => n.words.iter().any(|w| w == "cmd:load"),
    };
    if replaced {
        info!(
            "{}: Random Mix: ending, the queue was replaced ({} {})",
            master.id,
            n.first(),
            n.second()
        );
        stop(server, &master, true);
        return;
    }
    let (index, count, has_mix_songs) = {
        let c = master.controller();
        let c = c.lock();
        let mixes = server.randomplay.mixes.lock();
        let added = mixes.get(&master.id).map(|m| &m.added);
        let has = c.playlist.iter().any(|(_, e)| {
            e.track_id
                .is_some_and(|id| added.is_some_and(|a| a.contains(&id)))
        });
        (c.streaming_song_index(), c.playlist.len(), has)
    };
    let new_song = n.second() == "newsong";
    let deleted_ahead =
        n.second() == "delete" && n.word(2).parse::<usize>().is_ok_and(|i| i > index);
    if !new_song && !deleted_ahead {
        return;
    }
    if !has_mix_songs {
        info!(
            "{}: Random Mix: ending, none of its songs is queued",
            master.id
        );
        stop(server, &master, true);
        return;
    }
    let (server, task_server) = (server.clone(), server.clone());
    let newtracks = new_tracks(&server);
    let later = count.saturating_sub(index) > newtracks / 2;
    let id = master.id;
    let task = tokio::spawn(async move {
        let server = task_server;
        if new_song {
            if let Some(keep) = old_tracks(&server) {
                if index > keep {
                    debug!("{id}: Random Mix: removing {} played songs", index - keep);
                    for _ in 0..index - keep {
                        exec(&server, &master, &["playlist", "delete", "0"]).await;
                    }
                }
            }
        }
        if later {
            tokio::time::sleep(std::time::Duration::from_secs(ADD_LATER_SECS)).await;
        }
        play_random(&server, &master, true, true).await;
    });
    let mut timers = server.randomplay.timers.lock();
    if let Some(old) = timers.insert(id, task) {
        old.abort();
    }
}

// ---- commands ------------------------------------------------------------------------------

/// The genre commands edit a server pref: unlike Lyrion they don't need a player.
pub fn register(t: &mut DispatchTable) {
    t.add(
        &["randomplay", "_mode"],
        1,
        false,
        true,
        Handler::Async(|s, r| Box::pin(randomplay(s, r))),
    );
    t.add(
        &["randomplayisactive"],
        1,
        true,
        false,
        Handler::Sync(is_active),
    );
    t.add(
        &["randomplaygenrelist", "_index", "_quantity"],
        0,
        true,
        false,
        Handler::Sync(genre_list),
    );
    t.add(
        &["randomplaychoosegenre", "_genre", "_value"],
        0,
        false,
        false,
        Handler::Sync(choose_genre),
    );
    t.add(
        &["randomplaygenreselectall", "_value"],
        0,
        false,
        false,
        Handler::Sync(select_all_genres),
    );
}

/// `randomplay <tracks|albums|contributors|artists|year|disable> [genres:<names>]
/// [genre_id:<id>] [add:1]`.
async fn randomplay(server: Arc<Server>, mut r: Request) -> Request {
    let Some(p) = crate::control::commands::player(&server, &r) else {
        r.bad_dispatch();
        return r;
    };
    let mode = r.param("_mode").unwrap_or("").to_string();
    if mode == "disable" {
        stop(&server, &master_of(&p), false);
        r.set_done();
        return r;
    }
    let Some(kind) = Kind::parse(&mode) else {
        r.bad_params();
        return r;
    };
    let mut include = Vec::new();
    if let Some(g) = r.param_nonempty("genres") {
        let g = percent_encoding::percent_decode_str(g).decode_utf8_lossy();
        include = g
            .split(',')
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
    } else if let Some(id) = r.param("genre_id").and_then(|v| v.parse::<i64>().ok()) {
        if let Some((_, name)) = server.library.genres().into_iter().find(|(g, _)| *g == id) {
            include.push(name);
        }
    }
    let add = r.param("add").is_some_and(|v| v == "1");
    start(&server, &p, kind, include, add).await;
    r.set_done();
    r
}

/// `randomplayisactive`: the running mix type, or "".
fn is_active(server: &Arc<Server>, r: &mut Request) {
    let Some(p) = crate::control::commands::player(server, r) else {
        return r.bad_dispatch();
    };
    let kind = server.randomplay.active(master_of(&p).id);
    r.add_result("_randomplayisactive", kind.map(Kind::name).unwrap_or(""));
    r.set_done();
}

fn excluded_genres(server: &Server) -> Vec<String> {
    server.prefs.server.get_string_list(PREF_EXCLUDE_GENRES)
}

fn set_excluded_genres(server: &Server, mut names: Vec<String>) {
    names.sort();
    names.dedup();
    server.prefs.server.set(PREF_EXCLUDE_GENRES, json!(names));
}

/// `randomplaygenrelist`: "Choose Genres", a checkbox per genre.
fn genre_list(server: &Arc<Server>, r: &mut Request) {
    let excluded: HashSet<String> = excluded_genres(server).into_iter().collect();
    let all_none = |text: &str, value: i64| {
        json!({ "text": s(text), "nextWindow": "refresh",
            "actions": { "go": { "player": 0, "cmd": ["randomplaygenreselectall", value] } } })
    };
    let mut menu = vec![
        all_none("PLUGIN_RANDOM_SELECT_ALL", 1),
        all_none("PLUGIN_RANDOM_SELECT_NONE", 0),
    ];
    for (_, name) in server.library.genres() {
        menu.push(json!({
            "text": name,
            "checkbox": (!excluded.contains(&name)) as i64,
            "actions": {
                "on": { "player": 0, "cmd": ["randomplaychoosegenre", name, 1] },
                "off": { "player": 0, "cmd": ["randomplaychoosegenre", name, 0] },
            },
        }));
    }
    crate::control::jive::slice_and_ship(r, menu);
}

/// `randomplaychoosegenre <name> <0|1>`.
fn choose_genre(server: &Arc<Server>, r: &mut Request) {
    let (Some(genre), Some(value)) = (r.param("_genre"), r.param("_value")) else {
        return r.bad_params();
    };
    let mut excluded = excluded_genres(server);
    excluded.retain(|g| g != genre);
    if value == "0" {
        excluded.push(genre.to_string());
    }
    set_excluded_genres(server, excluded);
    r.set_done();
}

/// `randomplaygenreselectall <0|1>`.
fn select_all_genres(server: &Arc<Server>, r: &mut Request) {
    let excluded = if r.param("_value") == Some("0") {
        server
            .library
            .genres()
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    } else {
        Vec::new()
    };
    set_excluded_genres(server, excluded);
    r.set_done();
}

// ---- menus ---------------------------------------------------------------------------------

/// The Random Mix node in My Music, as Lyrion's plugin registers it.
pub fn jive_menu() -> Vec<Value> {
    let play = |kind: Kind| json!({ "player": 0, "cmd": ["randomplay", kind.cli()] });
    let mut menu = vec![json!({
        "text": s("PLUGIN_RANDOMPLAY"), "weight": 60, "id": "randomplay", "node": "myMusic",
        "isANode": 1, "windowStyle": "text_list", "window": { "titleStyle": "random" },
    })];
    let ids = [
        "randomtracks",
        "randomalbums",
        "randomartists",
        "randomyears",
    ];
    let weights = [10, 20, 30, 40];
    for ((kind, id), weight) in Kind::ALL.into_iter().zip(ids).zip(weights) {
        menu.push(json!({
            "text": kind.title(), "id": id, "weight": weight, "style": "itemplay",
            "nextWindow": "nowPlaying", "node": "randomplay",
            "actions": { "play": play(kind), "go": play(kind) },
        }));
    }
    menu.push(json!({
        "text": s("PLUGIN_RANDOM_CHOOSE_GENRES"), "id": "randomchoosegenres", "weight": 50,
        "window": { "titleStyle": "random" }, "node": "randomplay",
        "actions": { "go": { "player": 0, "cmd": ["randomplaygenrelist"] } },
    }));
    let disable = json!({ "player": 0, "cmd": ["randomplay", "disable"] });
    menu.push(json!({
        "text": s("PLUGIN_RANDOM_DISABLE"), "id": "randomdisable", "weight": 100,
        "style": "itemplay", "nextWindow": "refresh", "node": "randomplay",
        "actions": { "play": disable, "go": disable },
    }));
    menu
}

/// Alarm playlists (`getAlarmPlaylists`): (title, url).
pub fn alarm_playlists() -> Vec<(String, String)> {
    Kind::ALL
        .into_iter()
        .map(|k| (k.title(), format!("randomplay://{}", k.name())))
        .collect()
}

/// `randomplay://<type>[?genres=…]` → (kind, genres) (`ProtocolHandler::overridePlayback`).
pub fn parse_url(url: &str) -> Option<(Kind, Vec<String>)> {
    let rest = url.strip_prefix("randomplay://")?;
    let (kind, query) = rest.split_once('?').unwrap_or((rest, ""));
    let kind = Kind::parse(kind.trim_end_matches('/'))?;
    let genres = query
        .split('&')
        .filter_map(|kv| kv.strip_prefix("genres="))
        .flat_map(|g| {
            percent_encoding::percent_decode_str(g)
                .decode_utf8_lossy()
                .split(',')
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty())
                .collect::<Vec<_>>()
        })
        .collect();
    Some((kind, genres))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_urls() {
        assert_eq!(Kind::parse("tracks"), Some(Kind::Track));
        assert_eq!(Kind::parse("artists"), Some(Kind::Contributor));
        assert_eq!(Kind::parse("contributors"), Some(Kind::Contributor));
        assert_eq!(Kind::parse("year"), Some(Kind::Year));
        assert_eq!(Kind::parse("works"), None);
        assert_eq!(parse_url("randomplay://album"), Some((Kind::Album, vec![])));
        assert_eq!(
            parse_url("randomplay://track?genres=Rock,Hip%20Hop"),
            Some((Kind::Track, vec!["Rock".into(), "Hip Hop".into()]))
        );
        assert_eq!(parse_url("file:///x.flac"), None);
    }

    #[test]
    fn jive_menu_items() {
        let m = jive_menu();
        assert_eq!(m[0]["id"], "randomplay");
        assert_eq!(m[0]["node"], "myMusic");
        let ids: Vec<&str> = m.iter().map(|i| i["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "randomplay",
                "randomtracks",
                "randomalbums",
                "randomartists",
                "randomyears",
                "randomchoosegenres",
                "randomdisable"
            ]
        );
        assert_eq!(
            m[3]["actions"]["go"]["cmd"],
            json!(["randomplay", "contributors"])
        );
    }
}
