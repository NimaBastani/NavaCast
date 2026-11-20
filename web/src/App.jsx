import React, { useEffect, useMemo, useRef, useState } from "react";
import Hls from "hls.js";
import { api } from "./api.js";
import { translate } from "./i18n.js";
import { StationOutputs, TeamSpeakPage } from "./teamspeak.jsx";
import { AccountUsage, UsersPage } from "./users.jsx";

const BITRATES = [64000, 96000, 128000, 160000, 192000, 256000, 320000];
const SAMPLE_RATES = [22050, 32000, 44100, 48000];
const QUALITY_KEYS = ["bitrate", "sample_rate", "channels", "crossfade_ms", "buffer_ms", "hls_segment_ms"];
const DAY_KEYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const NAV = [
  { id: "home", href: "#/", key: "overview" },
  { id: "stations", href: "#/stations", key: "stations" },
  { id: "teamspeak", href: "#/teamspeak", key: "teamspeak" },
  { id: "users", href: "#/users", key: "users" },
  { id: "library", href: "#/library", key: "library" },
  { id: "playlists", href: "#/playlists", key: "playlists" },
  { id: "schedules", href: "#/schedules", key: "schedule" },
  { id: "health", href: "#/health", key: "health" },
];

function route() {
  const hash = window.location.hash.replace(/^#/, "") || "/";
  const parts = hash.split("/").filter(Boolean);
  return { hash, parts };
}

function clock(ms) {
  const s = Math.max(0, Math.floor((Number(ms) || 0) / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function remainText(shown, duration) {
  if (!(duration > 0)) return "—";
  return `−${clock(Math.max(0, duration - shown))}`;
}

function fmtBytes(n) {
  const v = Number(n) || 0;
  if (v < 1024) return `${v} B`;
  if (v < 1048576) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / 1048576).toFixed(1)} MB`;
}

function tone(text) {
  const s = String(text || "?");
  return String(s.charCodeAt(0) % 4);
}

function modeLabel(mode, t) {
  if (mode === "source" || mode === true) return t("onAir");
  if (mode === "relay") return t("relayMode");
  if (mode === "idle") return t("idle");
  return t("autodj");
}

function modeClass(mode) {
  if (mode === "source" || mode === true) return "source";
  if (mode === "relay") return "relay";
  if (mode === "idle") return "idle";
  return "autodj";
}

function airMode(mode) {
  if (mode === "source" || mode === true) return "source";
  if (mode === "relay") return "relay";
  if (mode === "autodj") return "autodj";
  return "idle";
}

function collectEncoderIds(value, out) {
  if (Array.isArray(value)) {
    value.forEach((item) => collectEncoderIds(item, out));
    return;
  }
  if (value && typeof value === "object") {
    if (typeof value.id === "string") collectEncoderIds(value.id, out);
    else if (typeof value.name === "string") collectEncoderIds(value.name, out);
    return;
  }
  if (typeof value !== "string") return;
  const id = value.trim();
  if (!id || out.includes(id)) return;
  out.push(id);
}

function harvestEncoders(value, lists, ids, depth) {
  if (depth > 6 || !value || typeof value !== "object") return;
  if (Array.isArray(value)) {
    value.forEach((item) => harvestEncoders(item, lists, ids, depth + 1));
    return;
  }
  for (const [key, item] of Object.entries(value)) {
    if (/encoder/i.test(key)) {
      if (Array.isArray(item)) lists.push(item);
      else ids.push(item);
    }
    if (item && typeof item === "object") harvestEncoders(item, lists, ids, depth + 1);
  }
}

function encoderChoices({ health, livePayload, liveRows, mount }) {
  const lists = [];
  const ids = [];
  harvestEncoders(health, lists, ids, 0);
  harvestEncoders(livePayload, lists, ids, 0);
  harvestEncoders(liveRows, lists, ids, 0);
  harvestEncoders(mount, lists, ids, 0);
  const out = [];
  lists.forEach((list) => collectEncoderIds(list, out));
  const listed = out.length > 0;
  ids.forEach((id) => collectEncoderIds(id, out));
  if (!listed) {
    collectEncoderIds("ffmpeg", out);
    collectEncoderIds("native", out);
  }
  return out;
}

function encoderLabel(id, t) {
  if (id === "native") {
    const name = t("nativeEncoder");
    return name === id ? id : `${id} — ${name}`;
  }
  return id;
}

function finite(v) {
  if (v === null || v === undefined || v === "") return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function asBool(v, fallback) {
  if (v === undefined || v === null) return fallback;
  if (v === true || v === 1 || v === "1" || v === "true") return true;
  if (v === false || v === 0 || v === "0" || v === "false") return false;
  return fallback;
}

function normalizeRepeat(v) {
  if (v === "off" || v === "one" || v === "all") return v;
  if (v === false || v === 0 || v === "0" || v === "false") return "off";
  if (v === true || v === 1 || v === "1" || v === "true") return "all";
  return "all";
}

function asRows(data) {
  if (Array.isArray(data)) return data;
  if (Array.isArray(data?.mounts)) return data.mounts;
  return [];
}

function trackLabel(item) {
  const title = item?.title || item?.filename || "—";
  return item?.artist ? `${item.artist} — ${title}` : title;
}

function withCurrent(list, current) {
  const n = Number(current);
  const out = list.slice();
  if (Number.isFinite(n) && !out.includes(n)) out.unshift(n);
  return out.sort((a, b) => a - b);
}

function formatBr(n, t) {
  if (n >= 1000 && n % 1000 === 0) return `${n / 1000} ${t("kbps")}`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)} ${t("kbps")}`;
  return String(n);
}

function formatRate(n, t) {
  if (n % 1000 === 0) return `${n / 1000} ${t("khz")}`;
  return `${(n / 1000).toFixed(1)} ${t("khz")}`;
}

function takeNum(v) {
  if (v === "" || v === null || v === undefined) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function isField(el) {
  if (!el || el === document.body || el === document.documentElement) return false;
  const tag = el.tagName;
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
  return !!el.isContentEditable;
}

function pageFocused(section) {
  const active = document.activeElement;
  if (!active || active === document.body || active === document.documentElement) return true;
  return section.contains(active);
}

function dayList(mask, t) {
  const sep = document.documentElement.dir === "rtl" ? "، " : ", ";
  const names = DAY_KEYS.filter((_, i) => Number(mask) & (1 << i)).map((key) => t(key));
  return names.length ? names.join(sep) : t("none");
}

function readDeck(live, mount, queue) {
  const position = finite(live?.position_ms) ?? finite(queue?.elapsed_ms) ?? finite(live?.now?.elapsed_ms) ?? finite(queue?.now?.elapsed_ms) ?? 0;
  const duration = finite(live?.duration_ms) ?? finite(queue?.duration_ms) ?? finite(live?.now?.duration_ms) ?? finite(queue?.now?.duration_ms) ?? 0;
  return {
    shuffle: asBool(live?.shuffle ?? mount?.shuffle, false),
    repeat: normalizeRepeat(live?.repeat ?? mount?.repeat),
    gain_db: Math.round((finite(live?.gain_db ?? mount?.gain_db) ?? 0) * 10) / 10,
    bitrate: finite(live?.bitrate ?? mount?.bitrate) ?? 128000,
    sample_rate: finite(live?.sample_rate ?? mount?.sample_rate) ?? 44100,
    channels: finite(live?.channels ?? mount?.channels) ?? 2,
    crossfade_ms: finite(live?.crossfade_ms ?? mount?.crossfade_ms) ?? 0,
    buffer_ms: finite(live?.buffer_ms ?? mount?.buffer_ms),
    hls_segment_ms: finite(live?.hls_segment_ms ?? mount?.hls_segment_ms),
    autodj: asBool(live?.autodj ?? mount?.autodj, true),
    allow_takeover: asBool(live?.allow_takeover ?? mount?.allow_takeover, false),
    encoder: live?.encoder || mount?.encoder || "ffmpeg",
    position_ms: position,
    duration_ms: duration,
  };
}

function qualityFrom(settings) {
  return {
    bitrate: settings.bitrate,
    sample_rate: settings.sample_rate,
    channels: settings.channels,
    crossfade_ms: settings.crossfade_ms,
    buffer_ms: settings.buffer_ms ?? "",
    hls_segment_ms: settings.hls_segment_ms ?? "",
  };
}

function qualitySig(settings) {
  return QUALITY_KEYS.map((key) => settings[key] ?? "").join("|");
}

function pickNow(live, queue, mount) {
  const candidates = [live?.now, queue?.now, mount?.now];
  return candidates.find((row) => row && (row.title || row.artist)) || null;
}

function usePlayhead(positionMs, durationMs, epoch, interval) {
  const pos = Number.isFinite(Number(positionMs)) ? Number(positionMs) : 0;
  const dur = Number.isFinite(Number(durationMs)) ? Math.max(0, Number(durationMs)) : 0;
  const [anchor, setAnchor] = useState(() => ({ pos, at: Date.now() }));
  const [now, setNow] = useState(() => Date.now());
  const [hold, setHold] = useState(null);
  const ignoreUntil = useRef(0);
  const epochRef = useRef(epoch);
  const dragging = useRef(false);
  const posRef = useRef(pos);
  posRef.current = pos;

  useEffect(() => {
    const switched = epochRef.current !== epoch;
    epochRef.current = epoch;
    if (dragging.current) return;
    if (!switched && Date.now() < ignoreUntil.current) return;
    if (switched) ignoreUntil.current = 0;
    setHold(null);
    setAnchor({ pos, at: Date.now() });
  }, [pos, epoch]);

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), interval);
    return () => clearInterval(id);
  }, [interval]);

  const livePos = Math.max(0, anchor.pos + (now - anchor.at));
  const capped = dur > 0 ? Math.min(dur, livePos) : livePos;
  const shown = hold == null ? capped : Math.max(0, hold);

  function begin(ms) {
    dragging.current = true;
    setHold(ms);
  }
  function commitLocal(ms) {
    dragging.current = false;
    const next = Math.max(0, ms);
    ignoreUntil.current = Date.now() + 2500;
    setHold(null);
    setAnchor({ pos: next, at: Date.now() });
  }
  function rejectLocal() {
    dragging.current = false;
    ignoreUntil.current = 0;
    setHold(null);
    setAnchor({ pos: posRef.current, at: Date.now() });
  }
  return { shown, duration: dur, begin, commitLocal, rejectLocal };
}

export default function App() {
  const [lang, setLang] = useState(() => localStorage.getItem("navacast.lang") || "en");
  const [token, setToken] = useState(() => localStorage.getItem("radio.token") || "");
  const [user, setUser] = useState(() => localStorage.getItem("radio.user") || "");
  const [owner, setOwner] = useState(() => localStorage.getItem("radio.owner") === "1");
  const [nav, setNav] = useState(route());
  const t = (key) => translate(lang, key);

  useEffect(() => {
    const onHash = () => setNav(route());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    document.documentElement.lang = lang === "fa" ? "fa" : "en";
    document.documentElement.dir = lang === "fa" ? "rtl" : "ltr";
    document.title = translate(lang, "brand");
    localStorage.setItem("navacast.lang", lang);
  }, [lang]);

  function signIn(nextToken, username, isOwner) {
    localStorage.setItem("radio.token", nextToken);
    localStorage.setItem("radio.user", username);
    localStorage.setItem("radio.owner", isOwner ? "1" : "0");
    setToken(nextToken);
    setUser(username);
    setOwner(!!isOwner);
    window.location.hash = "#/";
  }

  useEffect(() => {
    if (!token) return undefined;
    let stop = false;
    api("/api/me", { token }).then((me) => {
      if (stop) return;
      setOwner(!!me.owner);
      localStorage.setItem("radio.owner", me.owner ? "1" : "0");
      if (me.username) setUser(me.username);
    }).catch(() => {});
    return () => { stop = true; };
  }, [token]);

  function signOut() {
    localStorage.removeItem("radio.token");
    localStorage.removeItem("radio.user");
    localStorage.removeItem("radio.owner");
    setToken("");
    setUser("");
    window.location.hash = "#/login";
  }

  const publicSlug = nav.parts[0] === "listen" ? nav.parts[1] : "";
  if (publicSlug) return <ListenPage slug={publicSlug} lang={lang} setLang={setLang} t={t} />;
  if (!token) return <Login lang={lang} setLang={setLang} t={t} onLogin={signIn} />;

  const page = nav.parts[0] || "home";
  const known = ["home", "stations", "teamspeak", "users", "library", "playlists", "schedules", "health"];
  return (
    <Shell page={page} user={user} owner={owner} lang={lang} setLang={setLang} onSignOut={signOut} t={t}>
      {page === "stations" && nav.parts[1] ? <StationDetail key={nav.parts[1]} id={nav.parts[1]} token={token} t={t} /> : null}
      {page === "stations" && !nav.parts[1] ? <Stations token={token} t={t} /> : null}
      {page === "teamspeak" ? <TeamSpeakPage token={token} t={t} botId={nav.parts[1] || ""} /> : null}
      {page === "users" ? <UsersPage token={token} t={t} userId={nav.parts[1] || ""} owner={owner} /> : null}
      {page === "library" ? <Library token={token} t={t} /> : null}
      {page === "playlists" ? <Playlists token={token} t={t} /> : null}
      {page === "schedules" ? <Schedules token={token} t={t} /> : null}
      {page === "health" ? <Health token={token} t={t} /> : null}
      {page === "home" ? <Overview token={token} t={t} /> : null}
      {!known.includes(page) ? <Notice phase="ready" empty emptyText={t("empty")} t={t} /> : null}
    </Shell>
  );
}

function Shell({ page, user, owner, lang, setLang, onSignOut, t, children }) {
  const [menu, setMenu] = useState(false);
  const menuRef = useRef(null);
  const items = NAV.filter((item) => item.id !== "users" || owner).map((item) => ({ ...item, label: t(item.key) }));
  const menuHot = menu || page === "playlists" || page === "schedules" || page === "health" || (page === "users" && !owner);

  useEffect(() => { setMenu(false); }, [page]);
  useEffect(() => {
    const onResize = () => { if (window.innerWidth > 880) setMenu(false); };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  useEffect(() => {
    if (!menu) return undefined;
    const frame = requestAnimationFrame(() => menuRef.current?.querySelector("a")?.focus());
    const onKey = (e) => { if (e.key === "Escape") setMenu(false); };
    window.addEventListener("keydown", onKey);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  function toggleLang() { setLang(lang === "en" ? "fa" : "en"); }
  const langLabel = lang === "en" ? "فارسی" : "English";

  return (
    <div className="app">
      <aside className="sidenav">
        <div className="brand-lock">
          <strong className="wordmark">{t("brand")}</strong>
          <span className="kicker">{t("kicker")}</span>
        </div>
        <nav aria-label={t("menu")}>
          {items.map((item) => (
            <a key={item.id} href={item.href} className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined}>{item.label}</a>
          ))}
        </nav>
        <div className="side-foot">
          <span className="who">{user}</span>
          <button type="button" className="ghost" onClick={toggleLang}>{langLabel}</button>
          <button type="button" className="ghost" onClick={onSignOut}>{t("logout")}</button>
        </div>
      </aside>
      <div className="column">
        <header className="topbar">
          <div className="brand-lock"><strong className="wordmark">{t("brand")}</strong></div>
          <button type="button" className="ghost" onClick={toggleLang}>{langLabel}</button>
        </header>
        {menu ? (
          <div className="menu-panel" id="app-menu" ref={menuRef}>
            <nav aria-label={t("menu")}>
              {items.map((item) => (
                <a key={item.id} href={item.href} className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined}>{item.label}</a>
              ))}
            </nav>
            <div className="menu-actions">
              <button type="button" className="ghost" onClick={toggleLang}>{langLabel}</button>
              <button type="button" className="ghost" onClick={onSignOut}>{t("logout")}</button>
              <button type="button" className="ghost" onClick={() => setMenu(false)}>{t("close")}</button>
            </div>
          </div>
        ) : null}
        <main className="stage" hidden={menu}>{children}</main>
      </div>
      <nav className={owner ? "tabbar with-users" : "tabbar"} aria-label={t("menu")}>
        {items.filter((item) => ["home", "stations", "teamspeak"].includes(item.id) || (owner && item.id === "users")).map((item) => (
          <a key={item.id} href={item.href} className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined}>{item.label}</a>
        ))}
        <button type="button" className={menuHot ? "active" : ""} aria-expanded={menu} aria-controls="app-menu" onClick={() => setMenu((open) => !open)}>
          {menu ? t("close") : t("menu")}
        </button>
      </nav>
    </div>
  );
}

function Notice({ phase, error, t, onRetry, empty, emptyText }) {
  if (phase === "loading") {
    return (
      <div className="state state-loading" role="status">
        <strong>{t("loading")}</strong>
      </div>
    );
  }
  if (phase === "error") {
    return (
      <div className="state state-error" role="alert">
        <strong>{t("error")}</strong>
        {error ? <p>{error}</p> : null}
        {onRetry ? <button type="button" className="ghost" onClick={onRetry}>{t("retry")}</button> : null}
      </div>
    );
  }
  if (empty) {
    return (
      <div className="state state-empty" role="status">
        <strong>{emptyText || t("empty")}</strong>
      </div>
    );
  }
  return null;
}

function Login({ lang, setLang, t, onLogin }) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function submit(e) {
    e.preventDefault();
    setError("");
    setBusy(true);
    try {
      const data = await api("/api/login", { method: "POST", body: { username, password } });
      onLogin(data.token, data.username, !!data.owner);
    } catch {
      setError(t("badLogin"));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="gate">
      <div className="gate-hero">
        <span className="kicker">{t("kicker")}</span>
        <h1>{t("brand")}</h1>
        <p>{t("signInHint")}</p>
      </div>
      <form onSubmit={submit}>
        <h2>{t("login")}</h2>
        <label className="field">{t("username")}<input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="username" required /></label>
        <label className="field">{t("password")}<input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" required /></label>
        {error ? <p className="err" role="alert">{error}</p> : null}
        <button type="submit" disabled={busy}>{t("login")}</button>
        <button type="button" className="ghost" onClick={() => setLang(lang === "en" ? "fa" : "en")}>{lang === "en" ? "فارسی" : "English"}</button>
      </form>
    </div>
  );
}

function TimeReadout({ shown, duration, t }) {
  return (
    <div className="times">
      <div className="time-block">
        <span className="time-kicker">{t("elapsed")}</span>
        <span className="num" dir="ltr">{clock(shown)}</span>
      </div>
      <div className="time-block end">
        <span className="time-kicker">{t("remaining")}</span>
        <span className="num" dir="ltr">{remainText(shown, duration)}</span>
      </div>
    </div>
  );
}

function LiveReadout({ row, label, t }) {
  const pos = finite(row.position_ms) ?? finite(row.now?.elapsed_ms) ?? 0;
  const dur = finite(row.duration_ms) ?? finite(row.now?.duration_ms) ?? 0;
  const play = usePlayhead(pos, dur, row.now?.media_id || row.now?.title || row.id, 1000);
  if (!(dur > 0) && !(pos > 0)) return null;
  const pct = dur > 0 ? Math.min(100, (play.shown / dur) * 100) : 0;
  return (
    <div className="mini">
      <div
        className="rail is-static"
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={Math.round(dur || 0)}
        aria-valuenow={Math.round(play.shown)}
      >
        <div className="rail-fill" style={{ width: `${pct}%` }} />
      </div>
      <span className="num" dir="ltr">{clock(play.shown)}{dur > 0 ? ` · ${remainText(play.shown, dur)}` : ""}</span>
    </div>
  );
}

function Overview({ token, t }) {
  const [live, setLive] = useState([]);
  const [health, setHealth] = useState(null);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  async function reload() {
    try {
      const [rows, hp] = await Promise.all([api("/api/live", { token }), api("/api/health", { token })]);
      setLive(asRows(rows));
      setHealth(hp);
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => {
    let stop = false;
    const tick = () => { if (!stop) reload(); };
    tick();
    const timer = setInterval(tick, 3000);
    return () => { stop = true; clearInterval(timer); };
  }, [token]);
  const listeners = live.reduce((n, row) => n + (row.listeners || 0), 0);
  const sources = live.filter((row) => row.mode === "source" || row.live).length;
  return (
    <section>
      <div className="page-head">
        <div>
          <h1>{t("overview")}</h1>
          <p>{health?.ok ? t("healthOk") : t("health")} · <a href="#/health">{t("health")}</a></p>
        </div>
      </div>
      <Notice phase={phase === "ready" ? "ready" : phase} error={error} t={t} onRetry={reload} />
      {phase === "ready" ? (
        <>
          <AccountUsage token={token} t={t} />
          <div className="metrics">
            <article className="metric"><span>{t("mountsCount")}</span><strong>{live.length}</strong></article>
            <article className="metric"><span>{t("totalListeners")}</span><strong>{listeners}</strong></article>
            <article className="metric"><span>{t("mountsLive")}</span><strong>{sources}</strong><em>{t("onAir")}</em></article>
            <article className="metric"><span>{t("uptime")}</span><strong>{health ? `${health.uptime_sec || 0}s` : "—"}</strong><em>{health?.ffmpeg ? "ffmpeg" : t("nativeEncoder")}</em></article>
          </div>
          {live.length === 0 ? <Notice phase="ready" empty emptyText={t("empty")} t={t} /> : (
            <div className="panel table-wrap">
              <table className="as-cards">
                <thead>
                  <tr>
                    <th>{t("path")}</th><th>{t("station")}</th><th>{t("mode")}</th><th>{t("nowPlaying")}</th>
                    <th>{t("listeners")}</th><th>{t("peak")}</th><th>{t("bytes")}</th><th>{t("uptime")}</th>
                  </tr>
                </thead>
                <tbody>
                  {live.map((row) => (
                    <tr key={row.id || row.path}>
                      <td data-label={t("path")}><a className="ltr" dir="ltr" href={`#/stations/${row.station_id}`}>{row.path}</a></td>
                      <td data-label={t("station")}>{row.station_name}</td>
                      <td data-label={t("mode")}><span className={`badge ${modeClass(row.mode || (row.live ? "source" : "autodj"))}`}>{modeLabel(row.mode || (row.live ? "source" : "autodj"), t)}</span></td>
                      <td data-label={t("nowPlaying")}>
                        {row.now?.artist ? `${row.now.artist} — ` : ""}{row.now?.title || "—"}
                        <LiveReadout row={row} label={t("nowPlaying")} t={t} />
                      </td>
                      <td data-label={t("listeners")}>{row.listeners || 0}</td>
                      <td data-label={t("peak")}>{row.peak_listeners || 0}</td>
                      <td data-label={t("bytes")}>{fmtBytes(row.bytes_sent)}</td>
                      <td data-label={t("uptime")}>{row.uptime_sec || 0}s</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      ) : null}
    </section>
  );
}

function Health({ token, t }) {
  const [health, setHealth] = useState(null);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  async function reload() {
    try {
      setHealth(await api("/api/health", { token }));
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => {
    let stop = false;
    const tick = () => { if (!stop) reload(); };
    tick();
    const timer = setInterval(tick, 3000);
    return () => { stop = true; clearInterval(timer); };
  }, [token]);
  const yesNo = (v) => (v ? t("yes") : t("no"));
  return (
    <section>
      <div className="page-head">
        <div>
          <h1>{t("health")}</h1>
          <p>{health?.ok ? t("healthOk") : t("health")}</p>
        </div>
      </div>
      <Notice phase={phase === "ready" ? "ready" : phase} error={error} t={t} onRetry={reload} />
      {phase === "ready" && health ? (
        <div className="metrics">
          <article className="metric"><span>{t("health")}</span><strong>{health.ok ? t("healthOk") : t("error")}</strong></article>
          <article className="metric"><span>{t("ffmpeg")}</span><strong>{yesNo(health.ffmpeg)}</strong></article>
          <article className="metric"><span>{t("nativeEncoder")}</span><strong>{yesNo(health.native !== false)}</strong></article>
          <article className="metric"><span>{t("pcm")}</span><strong>{yesNo(health.pcm)}</strong></article>
          <article className="metric"><span>{t("mountsCount")}</span><strong>{health.mounts ?? 0}</strong></article>
          <article className="metric"><span>{t("listeners")}</span><strong>{health.listeners ?? 0}</strong></article>
          <article className="metric"><span>{t("uptime")}</span><strong>{health.uptime_sec ?? 0}s</strong></article>
          <article className="metric"><span>{t("version")}</span><strong>{health.version || "—"}</strong></article>
        </div>
      ) : null}
    </section>
  );
}

function Stations({ token, t }) {
  const [rows, setRows] = useState([]);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [formError, setFormError] = useState("");
  const [form, setForm] = useState({ name: "", slug: "", genre: "", description: "" });
  async function reload() {
    try {
      setRows(await api("/api/stations", { token }));
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => { reload(); }, [token]);
  async function create(e) {
    e.preventDefault();
    setFormError("");
    try {
      await api("/api/stations", { token, method: "POST", body: form });
      setForm({ name: "", slug: "", genre: "", description: "" });
      reload();
    } catch (err) {
      setFormError(err.message || t("actionFailed"));
    }
  }
  return (
    <section>
      <div className="page-head"><h1>{t("stations")}</h1></div>
      <form className="grid" onSubmit={create}>
        <label className="field">{t("name")}<input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} required /></label>
        <label className="field">{t("slug")}<input value={form.slug} onChange={(e) => setForm({ ...form, slug: e.target.value })} required /></label>
        <label className="field">{t("genre")}<input value={form.genre} onChange={(e) => setForm({ ...form, genre: e.target.value })} /></label>
        <label className="field">{t("description")}<input value={form.description} onChange={(e) => setForm({ ...form, description: e.target.value })} /></label>
        <button type="submit">{t("createStation")}</button>
      </form>
      {formError ? <p className="err" role="alert">{formError}</p> : null}
      <Notice phase={phase} error={error} t={t} onRetry={reload} empty={phase === "ready" && rows.length === 0} emptyText={t("stationsEmpty")} />
      <div className="stack">
        {rows.map((s) => (
          <div className="station-row" key={s.id}>
            <a href={`#/stations/${s.id}`}><strong dir="auto">{s.name}</strong> <span className="muted ltr" dir="ltr">/{s.slug}</span></a>
            <a href={`#/listen/${s.slug}`}>{t("publicLink")}</a>
          </div>
        ))}
      </div>
    </section>
  );
}

function StationDetail({ id, token, t }) {
  const sectionRef = useRef(null);
  const playRef = useRef({ position: 0, duration: 0 });
  const mountIdRef = useRef("");
  const actRef = useRef({ skip() {}, seek() {} });
  const serverRef = useRef(null);
  const view = useRef(0);
  const gen = useRef(0);
  const [station, setStation] = useState(null);
  const [mounts, setMounts] = useState([]);
  const [liveRows, setLiveRows] = useState([]);
  const [catalog, setCatalog] = useState({ health: null, livePayload: null });
  const [relayHold, setRelayHold] = useState(null);
  const [liveProblem, setLiveProblem] = useState(false);
  const [playlists, setPlaylists] = useState([]);
  const [media, setMedia] = useState([]);
  const [queues, setQueues] = useState({});
  const [schedules, setSchedules] = useState({});
  const [active, setActive] = useState("");
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [note, setNote] = useState(null);
  const [optimistic, setOptimistic] = useState({});
  const [libQuery, setLibQuery] = useState("");
  const [form, setForm] = useState({ path: "/live", source_password: "", bitrate: 128000, max_listeners: 100, allow_takeover: true, fallback_playlist_id: "", crossfade_ms: 250, autodj: true, shuffle: false, repeat: true });
  const loaded = useRef(false);

  async function reload() {
    const mine = ++gen.current;
    try {
      const stations = await api("/api/stations", { token });
      const ms = await api(`/api/mounts?station=${id}`, { token });
      const lists = await api(`/api/playlists?station=${id}`, { token });
      const files = await api(`/api/media?station=${id}`, { token });
      let livePayload = null;
      let live = [];
      let liveDown = false;
      let health = null;
      try {
        livePayload = await api("/api/live", { token });
        live = asRows(livePayload);
      } catch {
        liveDown = true;
      }
      try {
        health = await api("/api/health", { token });
      } catch {
        health = null;
      }
      const q = {};
      const sch = {};
      for (const m of ms) {
        q[m.id] = await api(`/api/mounts/${m.id}/queue`, { token });
        sch[m.id] = await api(`/api/schedules?mount=${m.id}`, { token });
      }
      if (mine !== gen.current) return;
      setStation(stations.find((s) => s.id === id) || null);
      setMounts(ms);
      setPlaylists(lists);
      setMedia(files);
      setLiveRows(live);
      setCatalog({ health, livePayload });
      setLiveProblem(liveDown);
      setQueues(q);
      setSchedules(sch);
      setActive((cur) => (cur && ms.some((m) => m.id === cur) ? cur : ms[0]?.id || ""));
      loaded.current = true;
      setPhase("ready");
    } catch (err) {
      if (mine !== gen.current) return;
      if (!loaded.current) {
        setPhase("error");
        setError(err.message);
      } else {
        setNote({ tone: "error", text: err.message || t("actionFailed") });
      }
    }
  }

  useEffect(() => {
    let dead = false;
    const run = () => { if (!dead) reload(); };
    run();
    const timer = setInterval(run, 2000);
    return () => { dead = true; clearInterval(timer); gen.current += 1; };
  }, [id, token]);

  const mount = mounts.find((m) => m.id === active) || mounts[0] || null;
  const queue = mount ? (queues[mount.id] || { upcoming: [], history: [], now: null }) : null;
  const live = mount ? liveRows.find((row) => row.id === mount.id || row.path === mount.path) || null : null;
  const serverSettings = mount ? readDeck(live, mount, queue) : null;
  serverRef.current = serverSettings;
  const settings = serverSettings ? { ...serverSettings, ...optimistic } : null;
  const playhead = usePlayhead(settings?.position_ms || 0, settings?.duration_ms || 0, mount?.id || "", 200);
  playRef.current = { position: playhead.shown, duration: playhead.duration };
  mountIdRef.current = mount?.id || "";
  const serverSig = serverSettings ? `${mount?.id}|${qualitySig(serverSettings)}|${serverSettings.shuffle}|${serverSettings.repeat}|${serverSettings.gain_db}|${serverSettings.autodj}|${serverSettings.allow_takeover}|${serverSettings.encoder}|${mount?.relay_url || ""}|${serverSettings.position_ms}` : "";

  useEffect(() => {
    view.current += 1;
    setOptimistic({});
    setRelayHold(null);
  }, [mount?.id]);

  useEffect(() => {
    if (!relayHold || relayHold.mountId !== mount?.id) return;
    if ((mount?.relay_url || "") === relayHold.url) setRelayHold(null);
  }, [mount?.relay_url, mount?.id, relayHold]);

  useEffect(() => {
    setOptimistic((prev) => {
      const server = serverRef.current;
      if (!server) return prev;
      let changed = false;
      const next = { ...prev };
      for (const [key, value] of Object.entries(prev)) {
        if (Object.is(server[key], value)) {
          delete next[key];
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [serverSig]);

  useEffect(() => {
    if (phase !== "ready") return;
    const activeEl = document.activeElement;
    if (isField(activeEl)) return;
    sectionRef.current?.focus({ preventScroll: true });
  }, [phase, id]);

  useEffect(() => {
    function onKey(e) {
      if (e.repeat || e.metaKey || e.ctrlKey || e.altKey) return;
      const section = sectionRef.current;
      if (!section || !pageFocused(section)) return;
      if (isField(document.activeElement)) return;
      if (e.code === "Space") {
        const tag = document.activeElement?.tagName;
        if (tag === "BUTTON" || tag === "A" || tag === "SUMMARY") return;
        e.preventDefault();
        actRef.current.skip();
      } else if (e.code === "ArrowLeft" || e.code === "ArrowRight") {
        e.preventDefault();
        const dir = e.code === "ArrowRight" ? 1 : -1;
        const { position, duration } = playRef.current;
        let next = position + dir * 5000;
        if (next < 0) next = 0;
        if (duration > 0 && next > duration) next = duration;
        actRef.current.seek(next);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [id]);

  function fail(err) {
    setNote({ tone: "error", text: (err && err.message) || t("actionFailed") });
  }

  async function patch(partial) {
    const mountId = mount?.id;
    if (!mountId) return;
    const seen = view.current;
    setOptimistic((prev) => ({ ...prev, ...partial }));
    try {
      await api(`/api/mounts/${mountId}`, { token, method: "PATCH", body: partial });
      setNote(null);
    } catch (err) {
      if (view.current === seen) {
        setOptimistic((prev) => {
          const next = { ...prev };
          for (const key of Object.keys(partial)) delete next[key];
          return next;
        });
      }
      fail(err);
      throw err;
    }
  }

  async function seekTo(ms) {
    const mountId = mountIdRef.current;
    if (!mountId) return;
    const dur = playRef.current.duration;
    let next = Math.max(0, Math.round(ms));
    if (dur > 0) next = Math.min(dur, next);
    playhead.commitLocal(next);
    try {
      await api(`/api/mounts/${mountId}/seek`, { token, method: "POST", body: { position_ms: next } });
      setNote(null);
    } catch (err) {
      playhead.rejectLocal();
      fail(err);
    }
  }

  async function transport(action) {
    const mountId = mountIdRef.current;
    if (!mountId) return;
    try {
      await api(`/api/mounts/${mountId}/queue/${action}`, { token, method: "POST", body: {} });
      setNote(null);
      await reload();
    } catch (err) {
      fail(err);
    }
  }

  async function jump(mediaId) {
    if (!mediaId || !mount) return;
    try {
      await api(`/api/mounts/${mount.id}/jump`, { token, method: "POST", body: { media_id: mediaId } });
      setNote(null);
      await reload();
    } catch (err) {
      fail(err);
    }
  }

  async function playNext(mediaId) {
    if (!mediaId || !mount) return;
    try {
      await api(`/api/mounts/${mount.id}/play-next`, { token, method: "POST", body: { media_id: mediaId } });
      setNote(null);
      await reload();
    } catch (err) {
      fail(err);
    }
  }

  async function queueAct(action, body) {
    if (!mount) return;
    try {
      await api(`/api/mounts/${mount.id}/queue/${action}`, { token, method: "POST", body: body || {} });
      setNote(null);
      await reload();
    } catch (err) {
      fail(err);
    }
  }

  actRef.current = { skip: () => transport("skip"), seek: seekTo };

  async function setRelay(url) {
    const mountId = mount?.id;
    if (!mountId) return false;
    const next = String(url ?? "").trim();
    setRelayHold({ mountId, url: next });
    try {
      await api(`/api/mounts/${mountId}`, { token, method: "PATCH", body: { relay_url: next } });
      setNote(null);
      return true;
    } catch (err) {
      setRelayHold(null);
      fail(err);
      return false;
    }
  }

  async function createMount(e) {
    e.preventDefault();
    try {
      await api("/api/mounts", {
        token,
        method: "POST",
        body: { station_id: id, ...form, fallback_playlist_id: form.fallback_playlist_id || null },
      });
      setForm({ path: "/live", source_password: "", bitrate: 128000, max_listeners: 100, allow_takeover: true, fallback_playlist_id: "", crossfade_ms: 250, autodj: true, shuffle: false, repeat: true });
      setNote(null);
      await reload();
    } catch (err) {
      fail(err);
    }
  }

  function focusDeck(e) {
    const el = e.target;
    if (!(el instanceof Element)) return;
    if (el.closest("input, textarea, select, button, a, label, summary, [role='slider']")) return;
    sectionRef.current?.focus({ preventScroll: true });
  }

  if (phase !== "ready") {
    return <Notice phase={phase} error={error} t={t} onRetry={reload} />;
  }
  if (!station) return <Notice phase="ready" empty emptyText={t("stationMissing")} t={t} />;

  const now = pickNow(live, queue, mount);
  const nowId = now?.media_id || "";
  const mode = live?.mode || mount?.mode || (mount?.live ? "source" : "autodj");
  const storedRelay = mount?.relay_url || live?.relay_url || "";
  const relayUrl = relayHold && relayHold.mountId === mount?.id ? relayHold.url : storedRelay;
  const encoderList = encoderChoices({
    health: catalog.health,
    livePayload: catalog.livePayload,
    liveRows,
    mount,
  });
  const encoderIds = settings?.encoder && !encoderList.includes(settings.encoder)
    ? [settings.encoder, ...encoderList]
    : encoderList;
  const query = libQuery.trim().toLowerCase();
  const libraryRows = media.filter((item) => {
    if (!query) return true;
    return `${item.artist || ""} ${item.title || ""} ${item.filename || ""}`.toLowerCase().includes(query);
  });

  return (
    <section
      className="station"
      ref={sectionRef}
      tabIndex={-1}
      aria-label={station.name}
      aria-keyshortcuts="Space ArrowLeft ArrowRight"
      onMouseDown={focusDeck}
    >
      <div className="page-head">
        <div>
          <h1 dir="auto">{station.name}</h1>
          <p>{station.genre} · <a href={`#/listen/${station.slug}`}>{t("publicLink")}</a></p>
        </div>
      </div>
      <Fold startOpen={mounts.length === 0} summary={t("newMount")}>
        <form className="grid" onSubmit={createMount}>
          <label className="field">{t("path")}<input value={form.path} onChange={(e) => setForm({ ...form, path: e.target.value })} required /></label>
          <label className="field">{t("sourcePassword")}<input value={form.source_password} onChange={(e) => setForm({ ...form, source_password: e.target.value })} autoComplete="new-password" required /></label>
          <label className="field">{t("bitrate")}<input type="number" value={form.bitrate} onChange={(e) => setForm({ ...form, bitrate: Number(e.target.value) })} /></label>
          <label className="field">{t("maxListeners")}<input type="number" value={form.max_listeners} onChange={(e) => setForm({ ...form, max_listeners: Number(e.target.value) })} /></label>
          <label className="field">{t("fallback")}
            <select value={form.fallback_playlist_id} onChange={(e) => setForm({ ...form, fallback_playlist_id: e.target.value })}>
              <option value="">{t("none")}</option>
              {playlists.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
            </select>
          </label>
          <label className="check"><input type="checkbox" checked={form.allow_takeover} onChange={(e) => setForm({ ...form, allow_takeover: e.target.checked })} /> {t("takeover")}</label>
          <button type="submit">{t("create")}</button>
        </form>
      </Fold>
      <StationOutputs stationId={id} token={token} t={t} />
      {note ? <div className={note.tone === "error" ? "banner banner-error" : "banner"} role={note.tone === "error" ? "alert" : "status"}>{note.text}</div> : null}
      {liveProblem ? <p className="hint warn">{t("liveStale")}</p> : null}
      {mounts.length === 0 ? <Notice phase="ready" empty emptyText={t("empty")} t={t} /> : null}
      {mount && settings ? (
        <>
          <div className="tabs" role="tablist" aria-label={t("path")}>
            {mounts.map((m) => (
              <button type="button" key={m.id} role="tab" dir="ltr" aria-selected={m.id === mount.id} className={m.id === mount.id ? "on" : ""} onClick={() => setActive(m.id)}>{m.path}{m.recording ? ` · ${t("recordingNow")}` : ""}</button>
            ))}
          </div>
          <div className="console">
            <Deck
              mount={mount}
              live={live}
              now={now}
              mode={mode}
              settings={settings}
              playhead={playhead}
              t={t}
              onSeek={seekTo}
              onPreview={playhead.begin}
              onSkip={() => transport("skip")}
              onPrev={() => transport("previous")}
              onShuffle={() => patch({ shuffle: !settings.shuffle }).catch(() => {})}
              onRepeat={(value) => patch({ repeat: value }).catch(() => {})}
              onGain={(value) => patch({ gain_db: value }).catch(() => {})}
            />
            <QueuePanel
              queue={queue}
              nowId={nowId}
              t={t}
              onJump={jump}
              onMove={(from, to) => queueAct("move", { from, to })}
              onRemove={(index) => queueAct("remove", { index })}
              onClear={() => queueAct("clear")}
            />
            <LibraryPanel
              rows={libraryRows}
              total={media.length}
              query={libQuery}
              nowId={nowId}
              t={t}
              onQuery={setLibQuery}
              onJump={jump}
              onPlayNext={playNext}
            />
            <div className="desk-side">
              <RelayPanel relayUrl={relayUrl} mode={mode} t={t} onSave={setRelay} />
              <QualityRack key={mount.id} settings={settings} encoders={encoderIds} t={t} onPatch={patch} />
            </div>
            <ListenerTable mount={mount} t={t} token={token} onChange={reload} />
          </div>
          <Recordings mount={mount} token={token} t={t} onChange={reload} />
          <MountControls mount={mount} playlists={playlists} media={media} token={token} t={t} onSaved={reload} onError={fail} />
          <ScheduleList rows={schedules[mount.id] || []} token={token} t={t} onChange={reload} />
        </>
      ) : null}
    </section>
  );
}


function Fold({ startOpen, summary, children }) {
  const ref = useRef(null);
  useEffect(() => {
    if (ref.current) ref.current.open = !!startOpen;
  }, []);
  return (
    <details className="fold" ref={ref}>
      <summary>{summary}</summary>
      {children}
    </details>
  );
}

function Deck({ mount, live, now, mode, settings, playhead, t, onSeek, onPreview, onSkip, onPrev, onShuffle, onRepeat, onGain }) {
  const title = now?.title || "";
  const artist = now?.artist || "";
  const duration = playhead.duration;
  const shown = playhead.shown;
  const listeners = live?.listeners ?? mount.listeners ?? 0;
  const peak = live?.peak_listeners ?? mount.peak_listeners ?? 0;
  const bytes = live?.bytes_sent ?? mount.bytes_sent ?? 0;
  const uptime = live?.uptime_sec ?? mount.uptime_sec ?? 0;
  return (
    <article className="deck" data-mode={modeClass(mode)} data-deck>
      <div className="deck-top">
        <div className="cover" data-tone={tone(title || mount.path)} aria-hidden="true">{String(title || mount.path || "?").slice(0, 1)}</div>
        <div className="deck-copy">
          <div className="deck-kicker">
            <span className={`badge ${modeClass(mode)}`}>{modeLabel(mode, t)}</span>
            <span className="muted ltr" dir="ltr">{mount.path}</span>
          </div>
          <h2 dir="auto">{title || t("nothingPlaying")}</h2>
          {artist ? <p className="artist" dir="auto">{artist}</p> : null}
        </div>
        <div className="deck-transport">
          <button type="button" className="ghost" onClick={onPrev}>{t("previous")}</button>
          <button type="button" onClick={onSkip}>{t("skip")}</button>
        </div>
      </div>
      <AirState mode={mode} t={t} />
      <SeekRail
        position={shown}
        duration={duration}
        disabled={!(duration > 0)}
        label={t("seek")}
        onPreview={onPreview}
        onSeek={onSeek}
      />
      <TimeReadout shown={shown} duration={duration} t={t} />
      {title && !(duration > 0) ? <p className="hint">{t("noDuration")}</p> : null}
      <div className="deck-tools">
        <button type="button" className={settings.shuffle ? "toggle on" : "toggle"} aria-pressed={settings.shuffle} onClick={onShuffle}>{t("shuffle")}</button>
        <div className="repeat">
          <span>{t("repeat")}</span>
          <div className="seg" role="radiogroup" aria-label={t("repeat")}>
            {[["off", "repeatOff"], ["one", "repeatOne"], ["all", "repeatAll"]].map(([value, key]) => (
              <button
                type="button"
                key={value}
                role="radio"
                aria-checked={settings.repeat === value}
                className={settings.repeat === value ? "on" : ""}
                onClick={() => { if (settings.repeat !== value) onRepeat(value); }}
              >
                {t(key)}
              </button>
            ))}
          </div>
        </div>
        <GainSlider key={mount.id} value={settings.gain_db} t={t} onCommit={onGain} />
      </div>
      <p className="hint keys">{t("keysHint")}</p>
      <p className="hint">
        <span>{listeners} {t("listeners")}</span>
        <span>{t("peak")} {peak}</span>
        <span>{fmtBytes(bytes)}</span>
        <span>{uptime}s</span>
      </p>
    </article>
  );
}

function SeekRail({ position, duration, disabled, label, onPreview, onSeek }) {
  const rail = useRef(null);
  const dragging = useRef(false);
  const off = disabled || !(duration > 0);
  const pct = duration > 0 ? Math.min(100, Math.max(0, (position / duration) * 100)) : 0;

  function msFrom(e) {
    const rect = rail.current.getBoundingClientRect();
    if (!rect.width || !(duration > 0)) return 0;
    const dir = getComputedStyle(rail.current).direction;
    let ratio = (e.clientX - rect.left) / rect.width;
    if (dir === "rtl") ratio = 1 - ratio;
    ratio = Math.min(1, Math.max(0, ratio));
    return Math.round(ratio * duration);
  }
  function down(e) {
    if (off) return;
    dragging.current = true;
    e.currentTarget.setPointerCapture(e.pointerId);
    e.preventDefault();
    onPreview(msFrom(e));
  }
  function move(e) {
    if (!dragging.current) return;
    onPreview(msFrom(e));
  }
  function finish(e) {
    if (!dragging.current) return;
    dragging.current = false;
    if (typeof e.clientX !== "number") return;
    onSeek(msFrom(e));
  }

  return (
    <div
      className={`rail ${off ? "is-disabled" : ""}`}
      ref={rail}
      role="slider"
      tabIndex={off ? -1 : 0}
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={Math.round(duration || 0)}
      aria-valuenow={Math.round(position || 0)}
      aria-valuetext={`${clock(position)}, ${remainText(position, duration)}`}
      aria-disabled={off}
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={finish}
      onPointerCancel={finish}
      onLostPointerCapture={finish}
    >
      <div className="rail-fill" style={{ width: `${pct}%` }} />
      <div className="rail-thumb" style={{ "--at": `${pct}%` }} />
    </div>
  );
}

function GainSlider({ value, onCommit, t }) {
  const [local, setLocal] = useState(null);
  const sent = useRef(value);
  const shown = local == null ? value : local;
  const min = Math.min(-24, shown);
  const max = Math.max(12, shown);
  useEffect(() => { sent.current = value; }, [value]);
  function commit(next) {
    const n = Math.round(Number(next) * 10) / 10;
    setLocal(null);
    if (!Number.isFinite(n) || n === sent.current) return;
    sent.current = n;
    onCommit(n);
  }
  return (
    <label className="gain">
      <span className="gain-read"><span>{t("gain")}</span><strong dir="ltr">{Number(shown).toFixed(1)} {t("db")}</strong></span>
      <input
        type="range"
        min={min}
        max={max}
        step="0.5"
        value={shown}
        aria-label={t("gain")}
        onChange={(e) => setLocal(Number(e.target.value))}
        onPointerUp={(e) => commit(e.currentTarget.value)}
        onKeyUp={(e) => commit(e.currentTarget.value)}
        onBlur={(e) => commit(e.currentTarget.value)}
        onLostPointerCapture={(e) => { if (local != null) commit(e.currentTarget.value); }}
      />
      <small>{t("gainHint")}</small>
    </label>
  );
}

function AirState({ mode, t }) {
  const current = airMode(mode);
  const items = [
    ["source", t("onAir")],
    ["relay", t("relayMode")],
    ["autodj", t("autodj")],
  ];
  return (
    <div className="air-state" role="status" aria-label={`${t("mode")}: ${modeLabel(mode, t)}`}>
      {items.map(([id, label]) => (
        <span key={id} className={current === id ? `on ${id}` : ""}>{label}</span>
      ))}
    </div>
  );
}

function RelayPanel({ relayUrl, mode, t, onSave }) {
  const [draft, setDraft] = useState(relayUrl || "");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");
  const editing = useRef(false);
  useEffect(() => {
    if (!editing.current) setDraft(relayUrl || "");
  }, [relayUrl]);

  async function save(e) {
    e.preventDefault();
    setBusy(true);
    setNote("");
    const ok = await onSave(draft.trim());
    setNote(ok ? "saved" : "error");
    setBusy(false);
  }

  async function clear() {
    setBusy(true);
    setNote("");
    const ok = await onSave("");
    if (ok) setDraft("");
    setNote(ok ? "saved" : "error");
    setBusy(false);
  }

  return (
    <form className="relay-card" onSubmit={save}>
      <h2>{t("relay")}</h2>
      <AirState mode={mode} t={t} />
      <label className="field">
        {t("relay")}
        <input
          dir="ltr"
          className="ltr"
          value={draft}
          onChange={(e) => { setNote(""); setDraft(e.target.value); }}
          onFocus={() => { editing.current = true; }}
          onBlur={() => { editing.current = false; }}
          placeholder="https://other.example/live"
          spellCheck="false"
          inputMode="url"
          autoComplete="off"
        />
        <small>{t("relayHint")}</small>
        <small>{t("relaySecurity")}</small>
      </label>
      {!relayUrl ? <p className="hint">{t("relayOff")}</p> : (
        <p className="hint ltr" dir="ltr">{relayUrl}</p>
      )}
      <div className="row">
        <button type="submit" disabled={busy}>{t("saveRelay")}</button>
        <button type="button" className="ghost" disabled={busy || (!relayUrl && !draft)} onClick={clear}>{t("clearRelay")}</button>
      </div>
      {note === "saved" ? <p className="hint" role="status">{t("saved")}</p> : null}
      {note === "error" ? <p className="err" role="alert">{t("actionFailed")}</p> : null}
    </form>
  );
}

function QualityRack({ settings, encoders, t, onPatch }) {
  const [form, setForm] = useState(() => qualityFrom(settings));
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const sig = qualitySig(settings);
  useEffect(() => {
    if (!dirty) setForm(qualityFrom(settings));
  }, [sig, dirty]);

  function edit(key, value) {
    setSaved(false);
    setDirty(true);
    setForm((curr) => ({ ...curr, [key]: value }));
  }

  async function apply(e) {
    e.preventDefault();
    const body = {};
    for (const key of QUALITY_KEYS) {
      const raw = takeNum(form[key]);
      if (raw == null) continue;
      const n = Math.round(raw);
      if (key === "channels" || key === "bitrate" || key === "sample_rate") body[key] = Math.max(1, n);
      else body[key] = Math.max(0, n);
    }
    if (!Object.keys(body).length) return;
    setBusy(true);
    try {
      await onPatch(body);
      setDirty(false);
      setSaved(true);
    } catch {
      setSaved(false);
    } finally {
      setBusy(false);
    }
  }

  const rates = withCurrent(SAMPLE_RATES, form.sample_rate);
  const ratesBr = withCurrent(BITRATES, form.bitrate);
  const channelOptions = withCurrent([1, 2], form.channels);
  return (
    <aside className="rack">
      <h2>{t("quality")}</h2>
      <p className="hint rack-lead">{t("delayHint")}</p>
      <div className="switch-col">
        <button type="button" className={`switch ${settings.autodj ? "is-on" : ""}`} role="switch" aria-checked={settings.autodj} onClick={() => onPatch({ autodj: !settings.autodj }).catch(() => {})}>
          <span className="switch-lamp" aria-hidden="true" />
          <span>{t("autodj")}</span>
        </button>
        <button type="button" className={`switch ${settings.allow_takeover ? "is-on" : ""}`} role="switch" aria-checked={settings.allow_takeover} onClick={() => onPatch({ allow_takeover: !settings.allow_takeover }).catch(() => {})}>
          <span className="switch-lamp" aria-hidden="true" />
          <span>{t("takeover")}</span>
        </button>
      </div>
      <form className="rack-form" onSubmit={apply}>
        <label className="field">{t("encoder")}
          <select value={settings.encoder} onChange={(e) => onPatch({ encoder: e.target.value }).catch(() => {})}>
            {(encoders || []).map((id) => <option key={id} value={id}>{encoderLabel(id, t)}</option>)}
          </select>
          <small>{t("encoderHint")}</small>
        </label>
        <label className="field">{t("bitrate")}
          <select value={String(form.bitrate)} onChange={(e) => edit("bitrate", Number(e.target.value))}>
            {ratesBr.map((n) => <option key={n} value={n}>{formatBr(n, t)}</option>)}
          </select>
          <small>{t("bitrateHint")}</small>
        </label>
        <label className="field">{t("sampleRate")}
          <select value={String(form.sample_rate)} onChange={(e) => edit("sample_rate", Number(e.target.value))}>
            {rates.map((n) => <option key={n} value={n}>{formatRate(n, t)}</option>)}
          </select>
          <small>{t("sampleHint")}</small>
        </label>
        <label className="field">{t("channels")}
          <select value={String(form.channels)} onChange={(e) => edit("channels", Number(e.target.value))}>
            {channelOptions.map((n) => <option key={n} value={n}>{n === 1 ? t("mono") : n === 2 ? t("stereo") : n}</option>)}
          </select>
          <small>{t("channelsHint")}</small>
        </label>
        <label className="field">{t("crossfade")}
          <input type="number" min="0" max="60000" step="50" value={form.crossfade_ms} onChange={(e) => edit("crossfade_ms", e.target.value)} />
          <small>{t("crossfadeHint")}</small>
        </label>
        <label className="field">{t("buffer")}
          <input type="number" min="0" max="20000" step="50" value={form.buffer_ms} onChange={(e) => edit("buffer_ms", e.target.value)} />
          <small>{t("bufferHint")}</small>
        </label>
        <label className="field">{t("hlsSegment")}
          <input type="number" min="0" max="20000" step="100" value={form.hls_segment_ms} onChange={(e) => edit("hls_segment_ms", e.target.value)} />
          <small>{t("segmentHint")}</small>
        </label>
        <button type="submit" disabled={busy}>{t("applyQuality")}</button>
        {saved ? <p className="hint">{t("saved")}</p> : null}
      </form>
    </aside>
  );
}

function QueuePanel({ queue, nowId, t, onJump, onMove, onRemove, onClear }) {
  const upcoming = queue?.upcoming || [];
  const history = queue?.history || [];
  return (
    <article className="panel queue-panel">
      <div className="track-head">
        <h2>{t("upcoming")}</h2>
        <button type="button" className="danger" disabled={upcoming.length === 0} onClick={onClear}>{t("clear")}</button>
      </div>
      <p className="hint">{t("jumpHint")}</p>
      {upcoming.length === 0 ? <p className="state state-empty">{t("queueEmpty")}</p> : (
        <div className="track-scroll">
          {upcoming.map((item, index) => (
            <div className={`track ${item.media_id && item.media_id === nowId ? "is-current" : ""}`} key={`${item.media_id || "row"}-${index}`}>
              <button type="button" className="track-main" disabled={!item.media_id} onClick={() => onJump(item.media_id)}>
                <span className="idx">{index + 1}</span>
                <span className="track-name" dir="auto">{trackLabel(item)}</span>
                {item.duration_ms ? <span className="num" dir="ltr">{clock(item.duration_ms)}</span> : null}
              </button>
              <span className="track-ops">
                <button type="button" className="quiet" disabled={index === 0} onClick={() => onMove(index, index - 1)}>{t("moveUp")}</button>
                <button type="button" className="quiet" disabled={index === upcoming.length - 1} onClick={() => onMove(index, index + 1)}>{t("moveDown")}</button>
                <button type="button" className="quiet" onClick={() => onRemove(index)}>{t("remove")}</button>
              </span>
            </div>
          ))}
        </div>
      )}
      <h3 className="subhead">{t("history")}</h3>
      {history.length === 0 ? <p className="state state-empty">{t("historyEmpty")}</p> : (
        <ul className="plain">{history.map((item, index) => <li key={`${item.media_id || "h"}-${index}`}><span dir="auto">{trackLabel(item)}</span></li>)}</ul>
      )}
    </article>
  );
}

function LibraryPanel({ rows, total, query, nowId, t, onQuery, onJump, onPlayNext }) {
  return (
    <article className="panel library-panel">
      <div className="track-head">
        <h2>{t("library")}</h2>
      </div>
      <p className="hint">{t("jumpHint")}</p>
      <label className="field">
        {t("searchLibrary")}
        <input value={query} onChange={(e) => onQuery(e.target.value)} placeholder={t("search")} />
      </label>
      {total === 0 ? <p className="state state-empty">{t("libraryEmpty")}</p> : null}
      {total > 0 && rows.length === 0 ? <p className="state state-empty">{t("noMatches")}</p> : null}
      {rows.length > 0 ? (
        <div className="track-scroll">
          {rows.map((item) => (
            <div className={`track ${item.id === nowId ? "is-current" : ""}`} key={item.id}>
              <button type="button" className="track-main" onClick={() => onJump(item.id)}>
                <span className="track-name" dir="auto">
                  {trackLabel(item)}
                  {item.id === nowId ? <em className="on-now"> {t("playing")}</em> : null}
                </span>
              </button>
              <span className="track-ops">
                <button type="button" className="quiet" onClick={() => onPlayNext(item.id)}>{t("playNext")}</button>
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </article>
  );
}

function fromMount(mount) {
  return {
    jingle_every: mount.jingle_every || 0,
    jingle_media_id: mount.jingle_media_id || "",
    intro_media_id: mount.intro_media_id || "",
    max_listeners: mount.max_listeners,
    fallback_playlist_id: mount.fallback_playlist_id || "",
  };
}

function Recordings({ mount, token, t, onChange }) {
  const [rows, setRows] = useState([]);
  const [minutes, setMinutes] = useState(() => Math.max(1, Math.round((mount.record_max_ms || 3600000) / 60000)));
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let dead = false;
    const load = () => {
      api(`/api/mounts/${mount.id}/recordings`, { token })
        .then((data) => { if (!dead) setRows(Array.isArray(data) ? data : []); })
        .catch(() => {});
    };
    load();
    const timer = setInterval(load, 2000);
    return () => { dead = true; clearInterval(timer); };
  }, [mount.id, token, mount.recording, mount.recording_bytes]);
  async function run(action) {
    setBusy(true);
    setNote("");
    try {
      await action();
      onChange();
    } catch (err) {
      setNote(err.message || t("actionFailed"));
    } finally {
      setBusy(false);
    }
  }
  async function download(rec) {
    setNote("");
    try {
      const res = await fetch(`/api/mounts/${mount.id}/recordings/${rec.id}`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        throw new Error(data.error || t("actionFailed"));
      }
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${rec.id}.${rec.format || "mp3"}`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      setNote(err.message || t("actionFailed"));
    }
  }
  const active = !!mount.recording;
  return (
    <article className="panel recordings">
      <div className="track-head">
        <h2>{t("recordings")}</h2>
        {active ? <span className="badge autodj" role="status">{t("recordingNow")}</span> : null}
      </div>
      <p className="hint">{t("recordHint")}</p>
      {mount.recording_error ? <p className="err" role="alert">{mount.recording_error}</p> : null}
      {note ? <p className="err" role="alert">{note}</p> : null}
      <div className="row">
        <label className="field">{t("recordMax")}
          <input dir="ltr" type="number" min="1" max="1440" value={minutes} onChange={(e) => setMinutes(e.target.value)} />
        </label>
        {active ? (
          <button type="button" className="danger" disabled={busy} onClick={() => run(() => api(`/api/mounts/${mount.id}/record/stop`, { token, method: "POST", body: {} }))}>{t("stopRecord")}</button>
        ) : (
          <button type="button" disabled={busy} onClick={() => run(() => api(`/api/mounts/${mount.id}/record/start`, { token, method: "POST", body: { max_duration_ms: Math.round(Number(minutes) * 60000) } }))}>{t("record")}</button>
        )}
        <button type="button" className="ghost" disabled={busy || rows.length === 0} onClick={() => run(() => api(`/api/mounts/${mount.id}/recordings`, { token, method: "DELETE" }))}>{t("deleteAllRecordings")}</button>
      </div>
      {rows.length === 0 ? <p className="state state-empty">{t("recordingsEmpty")}</p> : (
        <div className="table-wrap">
          <table className="as-cards">
            <thead>
              <tr>
                <th>{t("started")}</th><th>{t("duration")}</th><th>{t("format")}</th><th>{t("size")}</th><th>{t("path")}</th><th>{t("actions")}</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((rec) => (
                <tr key={rec.id} className={rec.active ? "is-recording" : ""}>
                  <td data-label={t("started")}>{rec.started_at ? new Date(rec.started_at).toLocaleString() : "—"}{rec.active ? ` · ${t("recordingNow")}` : ""}</td>
                  <td data-label={t("duration")} dir="ltr">{clock(rec.duration_ms)}</td>
                  <td data-label={t("format")} dir="ltr">{rec.format}</td>
                  <td data-label={t("size")} dir="ltr">{fmtBytes(rec.bytes)}</td>
                  <td data-label={t("path")} dir="ltr">{rec.mount}</td>
                  <td data-label={t("actions")}>
                    <button type="button" className="ghost" onClick={() => download(rec)}>{t("download")}</button>
                    <button type="button" className="ghost" onClick={() => run(() => api(`/api/mounts/${mount.id}/recordings/${rec.id}/jump`, { token, method: "POST", body: {} }))}>{t("replay")}</button>
                    <button type="button" className="ghost" onClick={() => run(() => api(`/api/mounts/${mount.id}/recordings/${rec.id}/queue`, { token, method: "POST", body: {} }))}>{t("queueNext")}</button>
                    <button type="button" className="danger" onClick={() => run(() => api(`/api/mounts/${mount.id}/recordings/${rec.id}`, { token, method: "DELETE" }))}>{t("delete")}</button>
                    {rec.error ? <p className="err" role="alert">{rec.error}</p> : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </article>
  );
}

function MountControls({ mount, playlists, media, token, t, onSaved, onError }) {
  const [form, setForm] = useState(() => fromMount(mount));
  const [password, setPassword] = useState("");
  const [note, setNote] = useState("");
  useEffect(() => { setForm(fromMount(mount)); }, [mount.id]);
  async function save(e) {
    e.preventDefault();
    setNote("");
    try {
      await api(`/api/mounts/${mount.id}`, {
        token,
        method: "PATCH",
        body: {
          max_listeners: Number(form.max_listeners),
          fallback_playlist_id: form.fallback_playlist_id || "",
          jingle_every: Number(form.jingle_every),
          jingle_media_id: form.jingle_media_id || "",
          intro_media_id: form.intro_media_id || "",
        },
      });
      if (password) {
        await api(`/api/mounts/${mount.id}/password`, { token, method: "POST", body: { password } });
        setPassword("");
      }
      setNote(t("saved"));
      onSaved();
    } catch (err) {
      setNote(err.message || t("failed"));
    }
  }
  const set = (key) => (e) => {
    const el = e.target;
    setForm({ ...form, [key]: el.type === "number" ? Number(el.value) : el.value });
  };
  return (
    <form className="panel" onSubmit={save}>
      <h2>{t("controls")}</h2>
      <div className="grid">
        <label className="field">{t("maxListeners")}<input type="number" value={form.max_listeners} onChange={set("max_listeners")} /></label>
        <label className="field">{t("jingleEvery")}<input type="number" min="0" value={form.jingle_every} onChange={set("jingle_every")} /></label>
        <label className="field">{t("fallback")}
          <select value={form.fallback_playlist_id} onChange={set("fallback_playlist_id")}>
            <option value="">{t("none")}</option>
            {playlists.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
          </select>
        </label>
        <label className="field">{t("intro")}
          <select value={form.intro_media_id} onChange={set("intro_media_id")}>
            <option value="">{t("none")}</option>
            {media.map((m) => <option key={m.id} value={m.id}>{m.title || m.filename}</option>)}
          </select>
        </label>
        <label className="field">{t("jingle")}
          <select value={form.jingle_media_id} onChange={set("jingle_media_id")}>
            <option value="">{t("none")}</option>
            {media.map((m) => <option key={m.id} value={m.id}>{m.title || m.filename}</option>)}
          </select>
        </label>
        {mount.hls_url ? <label className="field">{t("hls")}<input readOnly value={mount.hls_url} /></label> : null}
      </div>
      <div className="row">
        <label className="field">{t("rotatePassword")}<input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" /></label>
        <button type="submit">{t("saveControls")}</button>
        <button type="button" className="ghost" onClick={() => api(`/api/mounts/${mount.id}/disconnect`, { token, method: "POST", body: {} }).then(onSaved).catch(onError)}>{t("disconnect")}</button>
        <button type="button" className="danger" onClick={() => api(`/api/mounts/${mount.id}`, { token, method: "DELETE" }).then(onSaved).catch(onError)}>{t("delete")}</button>
      </div>
      {note ? <p className={note === t("saved") ? "hint" : "err"} role={note === t("saved") ? "status" : "alert"}>{note}</p> : null}
    </form>
  );
}

function ScheduleList({ rows, token, t, onChange, place }) {
  if (!rows) return null;
  return (
    <article className="panel">
      <h2>{t("schedule")}</h2>
      {place ? <p className="hint"><span dir="auto">{place.station}</span> · <span className="ltr" dir="ltr">{place.mount}</span></p> : null}
      {rows.length === 0 ? <p className="state state-empty">{t("scheduleEmpty")}</p> : (
        <div className="table-wrap">
          <table className="as-cards">
            <thead>
              <tr>
                <th>{t("days")}</th><th>{t("start")}</th><th>{t("end")}</th><th>{t("priority")}</th><th>{t("enabled")}</th><th>{t("actions")}</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => <ScheduleRow key={row.id} row={row} token={token} t={t} onChange={onChange} />)}
            </tbody>
          </table>
        </div>
      )}
    </article>
  );
}

function ScheduleRow({ row, token, t, onChange }) {
  const [priority, setPriority] = useState(row.priority);
  const [enabled, setEnabled] = useState(!!row.enabled);
  const [error, setError] = useState("");
  useEffect(() => { setPriority(row.priority); setEnabled(!!row.enabled); }, [row.priority, row.enabled, row.id]);
  const mins = (n) => `${String(Math.floor(n / 60)).padStart(2, "0")}:${String(n % 60).padStart(2, "0")}`;
  async function save() {
    setError("");
    try {
      await api(`/api/schedules/${row.id}`, { token, method: "PATCH", body: { enabled, priority: Number(priority) } });
      onChange();
    } catch (err) {
      setError(err.message || t("actionFailed"));
    }
  }
  return (
    <tr>
      <td data-label={t("days")}>{dayList(row.days_mask, t)}</td>
      <td className="num" dir="ltr" data-label={t("start")}>{mins(row.start_min)}</td>
      <td className="num" dir="ltr" data-label={t("end")}>{mins(row.end_min)}</td>
      <td data-label={t("priority")}><input type="number" aria-label={t("priority")} value={priority} onChange={(e) => setPriority(e.target.value)} /></td>
      <td data-label={t("enabled")}>
        <label className="hit">
          <input type="checkbox" aria-label={t("enabled")} checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
        </label>
      </td>
      <td data-label={t("actions")}>
        <button type="button" className="ghost" onClick={save}>{t("save")}</button>
        {error ? <p className="err" role="alert">{error}</p> : null}
      </td>
    </tr>
  );
}

function ListenerTable({ mount, token, t, onChange }) {
  const rows = mount.listener_rows || [];
  const [error, setError] = useState("");
  function kick(path) {
    setError("");
    api(path, { token, method: "POST", body: {} }).then(onChange).catch((err) => setError(err.message || t("actionFailed")));
  }
  return (
    <article className="panel listeners">
      <div className="track-head">
        <h2>{t("listenersTitle")}</h2>
        <button type="button" className="danger" onClick={() => kick(`/api/mounts/${mount.id}/listeners/kick`)}>{t("kickAll")}</button>
      </div>
      {error ? <p className="err" role="alert">{error}</p> : null}
      {rows.length === 0 ? <p className="state state-empty">{t("noListeners")}</p> : (
        <div className="table-wrap">
          <table className="as-cards">
            <thead><tr><th>{t("connected")}</th><th>{t("ip")}</th><th>{t("userAgent")}</th><th>{t("actions")}</th></tr></thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id}>
                  <td data-label={t("connected")}>{row.connected_sec ?? Math.round((row.connected_ms || 0) / 1000)}s</td>
                  <td data-label={t("ip")}><span className="ltr" dir="ltr">{row.ip}</span></td>
                  <td data-label={t("userAgent")}>{row.user_agent || "—"}</td>
                  <td data-label={t("actions")}><button type="button" className="ghost" onClick={() => kick(`/api/mounts/${mount.id}/listeners/${row.id}/kick`)}>{t("kick")}</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </article>
  );
}

function Library({ token, t }) {
  const [stations, setStations] = useState([]);
  const [station, setStation] = useState("");
  const [media, setMedia] = useState([]);
  const [q, setQ] = useState("");
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [folder, setFolder] = useState("");
  const [summary, setSummary] = useState("");
  const [importError, setImportError] = useState("");
  const [importing, setImporting] = useState(false);
  useEffect(() => {
    api("/api/stations", { token }).then((rows) => {
      setStations(rows);
      setStation((cur) => cur || rows[0]?.id || "");
      if (!rows[0]) setPhase("ready");
    }).catch((err) => { setPhase("error"); setError(err.message); });
  }, [token]);
  async function reload(sid, query) {
    const sidNow = sid || station;
    if (!sidNow) { setMedia([]); return; }
    try {
      const queryText = query === undefined ? q : query;
      setMedia(await api(`/api/media?station=${encodeURIComponent(sidNow)}&q=${encodeURIComponent(queryText)}`, { token }));
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => { if (station) reload(station, q); }, [station]);
  useEffect(() => {
    const timer = setTimeout(() => { if (station) reload(station, q); }, 250);
    return () => clearTimeout(timer);
  }, [q]);
  async function upload(e) {
    const file = e.target.files?.[0];
    if (!file || !station) return;
    const form = new FormData();
    form.append("station_id", station);
    form.append("file", file);
    try {
      await api("/api/media", { token, method: "POST", form });
      await reload();
    } catch (err) {
      setError(err.message);
      setPhase("error");
    }
    e.target.value = "";
  }
  async function importFolder(e) {
    e.preventDefault();
    if (!station || !folder.trim() || importing) return;
    setImporting(true);
    setSummary("");
    setImportError("");
    try {
      const report = await api("/api/media/import", { token, method: "POST", body: { station_id: station, path: folder.trim() } });
      setSummary(`${t("imported")}: ${report.imported} · ${t("skipped")}: ${report.skipped} · ${t("importFailed")}: ${report.failed}`);
      await reload();
    } catch (err) {
      setImportError(err.message || t("actionFailed"));
    } finally {
      setImporting(false);
    }
  }
  return (
    <section>
      <div className="page-head"><h1>{t("library")}</h1></div>
      <div className="row">
        <label className="field">{t("selectStation")}
          <select value={station} onChange={(e) => setStation(e.target.value)}>{stations.map((s) => <option key={s.id} value={s.id}>{s.name}</option>)}</select>
        </label>
        <label className="field">{t("search")}
          <input placeholder={t("search")} value={q} onChange={(e) => setQ(e.target.value)} />
        </label>
        <label className="filebtn">{t("upload")}<input type="file" accept="audio/*,.mp3,.flac,.ogg,.wav,.m4a,.aac" onChange={upload} /></label>
      </div>
      <form className="row" onSubmit={importFolder}>
        <label className="field">{t("serverPath")}
          <input className="ltr" dir="ltr" spellCheck={false} autoComplete="off" value={folder} onChange={(e) => setFolder(e.target.value)} />
          <small>{t("importHint")}</small>
        </label>
        <button type="submit" disabled={importing || !station || !folder.trim()}>{t("importFolder")}</button>
      </form>
      {summary ? <p className="hint" role="status">{summary}</p> : null}
      {importError ? <p className="err" role="alert">{importError}</p> : null}
      <Notice phase={phase} error={error} t={t} onRetry={() => reload()} empty={phase === "ready" && !!station && media.length === 0} emptyText={t("libraryEmpty")} />
      {phase === "ready" && stations.length === 0 ? <Notice phase="ready" empty emptyText={t("stationsEmpty")} t={t} /> : null}
      {media.length > 0 ? (
        <div className="panel table-wrap">
          <table className="as-cards">
            <thead><tr><th>{t("artist")}</th><th>{t("title")}</th><th>{t("file")}</th><th>{t("size")}</th><th>{t("actions")}</th></tr></thead>
            <tbody>
              {media.map((m) => <MediaRow key={m.id} item={m} token={token} t={t} onChange={() => reload()} />)}
            </tbody>
          </table>
        </div>
      ) : null}
    </section>
  );
}

function MediaRow({ item, token, t, onChange }) {
  const [artist, setArtist] = useState(item.artist || "");
  const [title, setTitle] = useState(item.title || "");
  const [note, setNote] = useState("");
  useEffect(() => { setArtist(item.artist || ""); setTitle(item.title || ""); }, [item.id, item.artist, item.title]);
  async function save() {
    try {
      await api(`/api/media/${item.id}`, { token, method: "PATCH", body: { artist, title } });
      setNote(t("saved"));
      onChange();
    } catch (err) { setNote(err.message); }
  }
  async function replace(e) {
    const file = e.target.files?.[0];
    if (!file) return;
    const form = new FormData();
    form.append("file", file);
    try {
      await api(`/api/media/${item.id}/replace`, { token, method: "POST", form });
      onChange();
    } catch (err) { setNote(err.message); }
    e.target.value = "";
  }
  async function remove() {
    try {
      await api(`/api/media/${item.id}`, { token, method: "DELETE" });
      onChange();
    } catch (err) { setNote(err.message || t("actionFailed")); }
  }
  return (
    <tr>
      <td data-label={t("artist")}><input aria-label={t("artist")} value={artist} onChange={(e) => setArtist(e.target.value)} /></td>
      <td data-label={t("title")}><input aria-label={t("title")} value={title} onChange={(e) => setTitle(e.target.value)} /></td>
      <td data-label={t("file")}>{item.filename}</td>
      <td data-label={t("size")}>{fmtBytes(item.bytes)}</td>
      <td data-label={t("actions")}>
        <div className="row">
          <button type="button" className="ghost" onClick={save}>{t("save")}</button>
          <label className="filebtn ghost">{t("replace")}<input type="file" accept="audio/*,.mp3,.flac,.ogg,.wav,.m4a,.aac" onChange={replace} /></label>
          <button type="button" className="danger" onClick={remove}>{t("delete")}</button>
        </div>
        {note ? <span className="hint">{note}</span> : null}
      </td>
    </tr>
  );
}

function Playlists({ token, t }) {
  const [stations, setStations] = useState([]);
  const [station, setStation] = useState("");
  const [playlists, setPlaylists] = useState([]);
  const [media, setMedia] = useState([]);
  const [mounts, setMounts] = useState([]);
  const [name, setName] = useState("");
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [formError, setFormError] = useState("");
  const [sched, setSched] = useState({ mount_id: "", playlist_id: "", days: "sun,mon,tue,wed,thu,fri,sat", start: "00:00", end: "23:59", priority: 1 });
  async function reload(nextId) {
    const sid = nextId || station;
    if (!sid) return;
    try {
      const [lists, files, ms] = await Promise.all([
        api(`/api/playlists?station=${sid}`, { token }),
        api(`/api/media?station=${sid}`, { token }),
        api(`/api/mounts?station=${sid}`, { token }),
      ]);
      setPlaylists(lists);
      setMedia(files);
      setMounts(ms);
      setSched((cur) => ({ ...cur, mount_id: cur.mount_id || ms[0]?.id || "", playlist_id: cur.playlist_id || lists[0]?.id || "" }));
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => {
    api("/api/stations", { token }).then((rows) => {
      setStations(rows);
      if (rows[0]) { setStation(rows[0].id); reload(rows[0].id); }
      else setPhase("ready");
    }).catch((err) => { setPhase("error"); setError(err.message); });
  }, [token]);
  async function create(e) {
    e.preventDefault();
    setFormError("");
    try {
      await api("/api/playlists", { token, method: "POST", body: { station_id: station, name } });
      setName("");
      reload();
    } catch (err) {
      setFormError(err.message || t("actionFailed"));
    }
  }
  async function schedule(e) {
    e.preventDefault();
    setFormError("");
    try {
      await api("/api/schedules", { token, method: "POST", body: { ...sched, days: sched.days.split(",").map((d) => d.trim()).filter(Boolean), priority: Number(sched.priority) } });
      reload();
    } catch (err) {
      setFormError(err.message || t("actionFailed"));
    }
  }
  return (
    <section>
      <div className="page-head">
        <div>
          <h1>{t("playlists")}</h1>
          <p><a href="#/schedules">{t("schedule")}</a></p>
        </div>
      </div>
      <div className="row">
        <label className="field">{t("selectStation")}
          <select value={station} onChange={(e) => { setStation(e.target.value); reload(e.target.value); }}>{stations.map((s) => <option key={s.id} value={s.id}>{s.name}</option>)}</select>
        </label>
        <form onSubmit={create} className="row">
          <label className="field">{t("name")}<input value={name} onChange={(e) => setName(e.target.value)} required /></label>
          <button type="submit">{t("create")}</button>
        </form>
      </div>
      {formError ? <p className="err" role="alert">{formError}</p> : null}
      <Notice phase={phase} error={error} t={t} onRetry={() => reload()} empty={phase === "ready" && stations.length > 0 && playlists.length === 0} emptyText={t("empty")} />
      {phase === "ready" && stations.length === 0 ? <Notice phase="ready" empty emptyText={t("stationsEmpty")} t={t} /> : null}
      {playlists.map((p) => <PlaylistCard key={p.id} playlist={p} media={media} mounts={mounts} token={token} t={t} onChange={() => reload()} />)}
      <form className="panel grid" onSubmit={schedule}>
        <h2 className="full">{t("schedule")}</h2>
        <label className="field">{t("path")}
          <select dir="ltr" value={sched.mount_id} onChange={(e) => setSched({ ...sched, mount_id: e.target.value })}>{mounts.map((m) => <option key={m.id} value={m.id}>{m.path}</option>)}</select>
        </label>
        <label className="field">{t("playlists")}
          <select value={sched.playlist_id} onChange={(e) => setSched({ ...sched, playlist_id: e.target.value })}>{playlists.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}</select>
        </label>
        <label className="field">{t("days")}
          <input value={sched.days} onChange={(e) => setSched({ ...sched, days: e.target.value })} />
          <small>{t("daysHint")}</small>
        </label>
        <label className="field">{t("start")}<input type="time" value={sched.start} onChange={(e) => setSched({ ...sched, start: e.target.value })} /></label>
        <label className="field">{t("end")}<input type="time" value={sched.end} onChange={(e) => setSched({ ...sched, end: e.target.value })} /></label>
        <label className="field">{t("priority")}<input type="number" value={sched.priority} onChange={(e) => setSched({ ...sched, priority: e.target.value })} /></label>
        <button type="submit">{t("schedule")}</button>
      </form>
    </section>
  );
}

function PlaylistCard({ playlist, media, mounts, token, t, onChange }) {
  const [mountId, setMountId] = useState(mounts[0]?.id || "");
  const [error, setError] = useState("");
  const items = playlist.items || [];
  async function run(job) {
    setError("");
    try {
      await job();
      onChange();
    } catch (err) {
      setError(err.message || t("actionFailed"));
    }
  }
  return (
    <article className="panel">
      <h2>{playlist.name}</h2>
      {error ? <p className="err" role="alert">{error}</p> : null}
      {items.length === 0 ? <p className="state state-empty">{t("playlistEmpty")}</p> : (
        <ol className="queue-list">
          {items.map((item, index) => (
            <li key={item.id || `${item.media_id}-${index}`}>
              <span>{item.artist ? `${item.artist} — ` : ""}{item.title || item.filename}</span>
              <span className="row">
                <button type="button" className="quiet" disabled={index === 0} onClick={() => run(() => api(`/api/playlists/${playlist.id}/reorder`, { token, method: "POST", body: { from: index, to: index - 1 } }))}>{t("moveUp")}</button>
                <button type="button" className="quiet" disabled={index === items.length - 1} onClick={() => run(() => api(`/api/playlists/${playlist.id}/reorder`, { token, method: "POST", body: { from: index, to: index + 1 } }))}>{t("moveDown")}</button>
                <button type="button" className="quiet" onClick={() => run(() => api(`/api/playlists/${playlist.id}/items/${item.id}`, { token, method: "DELETE" }))}>{t("remove")}</button>
              </span>
            </li>
          ))}
        </ol>
      )}
      <div className="row">
        <label className="field">{t("add")}
          <select defaultValue="" onChange={(e) => { const id = e.target.value; e.target.value = ""; if (!id) return; run(() => api(`/api/playlists/${playlist.id}/items`, { token, method: "POST", body: { media_id: id } })); }}>
            <option value="">{t("add")}</option>
            {media.map((m) => <option key={m.id} value={m.id}>{m.title || m.filename}</option>)}
          </select>
        </label>
        <label className="field">{t("path")}
          <select dir="ltr" value={mountId} onChange={(e) => setMountId(e.target.value)}>{mounts.map((m) => <option key={m.id} value={m.id}>{m.path}</option>)}</select>
        </label>
        <button type="button" className="ghost" disabled={!mountId} onClick={() => run(() => api(`/api/mounts/${mountId}`, { token, method: "PATCH", body: { fallback_playlist_id: playlist.id } }))}>{t("assignFallback")}</button>
      </div>
    </article>
  );
}

function Schedules({ token, t }) {
  const [tree, setTree] = useState([]);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [formError, setFormError] = useState("");
  const [note, setNote] = useState("");
  const [form, setForm] = useState({ station_id: "", mount_id: "", playlist_id: "", days: "sun,mon,tue,wed,thu,fri,sat", start: "00:00", end: "23:59", priority: 1 });

  async function reload() {
    try {
      const stations = await api("/api/stations", { token });
      const next = [];
      for (const station of stations) {
        const [mounts, playlists] = await Promise.all([
          api(`/api/mounts?station=${station.id}`, { token }),
          api(`/api/playlists?station=${station.id}`, { token }),
        ]);
        const withRows = [];
        for (const mount of mounts) {
          const rows = await api(`/api/schedules?mount=${mount.id}`, { token });
          withRows.push({ mount, rows });
        }
        next.push({ station, playlists, mounts: withRows });
      }
      setTree(next);
      setForm((cur) => {
        const station_id = next.some((g) => g.station.id === cur.station_id) ? cur.station_id : (next[0]?.station.id || "");
        const group = next.find((g) => g.station.id === station_id);
        const mount_id = group?.mounts.some((m) => m.mount.id === cur.mount_id) ? cur.mount_id : (group?.mounts[0]?.mount.id || "");
        const playlist_id = group?.playlists.some((p) => p.id === cur.playlist_id) ? cur.playlist_id : (group?.playlists[0]?.id || "");
        return { ...cur, station_id, mount_id, playlist_id };
      });
      setPhase("ready");
    } catch (err) {
      setPhase("error");
      setError(err.message);
    }
  }
  useEffect(() => { reload(); }, [token]);

  const group = tree.find((g) => g.station.id === form.station_id);
  const total = tree.reduce((n, g) => n + g.mounts.reduce((m, item) => m + item.rows.length, 0), 0);
  const hasMount = tree.some((g) => g.mounts.length > 0);

  function pickStation(station_id) {
    const g = tree.find((item) => item.station.id === station_id);
    setForm({
      ...form,
      station_id,
      mount_id: g?.mounts[0]?.mount.id || "",
      playlist_id: g?.playlists[0]?.id || "",
    });
  }

  async function create(e) {
    e.preventDefault();
    setFormError("");
    setNote("");
    try {
      await api("/api/schedules", {
        token,
        method: "POST",
        body: {
          mount_id: form.mount_id,
          playlist_id: form.playlist_id,
          days: form.days.split(",").map((d) => d.trim()).filter(Boolean),
          start: form.start,
          end: form.end,
          priority: Number(form.priority),
        },
      });
      setNote(t("saved"));
      await reload();
    } catch (err) {
      setFormError(err.message || t("actionFailed"));
    }
  }

  return (
    <section>
      <div className="page-head"><h1>{t("schedule")}</h1></div>
      <Notice phase={phase === "ready" ? "ready" : phase} error={error} t={t} onRetry={reload} empty={phase === "ready" && tree.length === 0} emptyText={t("stationsEmpty")} />
      {phase === "ready" && tree.length > 0 && !hasMount ? <Notice phase="ready" empty emptyText={t("empty")} t={t} /> : null}
      {phase === "ready" && hasMount ? (
        <form className="panel grid" onSubmit={create}>
          <label className="field">{t("station")}
            <select value={form.station_id} onChange={(e) => pickStation(e.target.value)}>
              {tree.map((g) => <option key={g.station.id} value={g.station.id}>{g.station.name}</option>)}
            </select>
          </label>
          <label className="field">{t("path")}
            <select dir="ltr" value={form.mount_id} onChange={(e) => setForm({ ...form, mount_id: e.target.value })}>
              {(group?.mounts || []).map((item) => <option key={item.mount.id} value={item.mount.id}>{item.mount.path}</option>)}
            </select>
          </label>
          <label className="field">{t("playlists")}
            <select value={form.playlist_id} onChange={(e) => setForm({ ...form, playlist_id: e.target.value })}>
              {(group?.playlists || []).map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
            </select>
          </label>
          <label className="field">{t("days")}
            <input value={form.days} onChange={(e) => setForm({ ...form, days: e.target.value })} />
            <small>{t("daysHint")}</small>
          </label>
          <label className="field">{t("start")}<input type="time" value={form.start} onChange={(e) => setForm({ ...form, start: e.target.value })} /></label>
          <label className="field">{t("end")}<input type="time" value={form.end} onChange={(e) => setForm({ ...form, end: e.target.value })} /></label>
          <label className="field">{t("priority")}<input type="number" value={form.priority} onChange={(e) => setForm({ ...form, priority: Number(e.target.value) })} /></label>
          <button type="submit">{t("schedule")}</button>
          {formError ? <p className="err full" role="alert">{formError}</p> : null}
          {note ? <p className="hint full">{note}</p> : null}
        </form>
      ) : null}
      {phase === "ready" && hasMount && total === 0 ? <Notice phase="ready" empty emptyText={t("scheduleEmpty")} t={t} /> : null}
      {tree.map((g) => g.mounts.filter((item) => item.rows.length).map((item) => (
        <ScheduleList
          key={item.mount.id}
          rows={item.rows}
          token={token}
          t={t}
          onChange={reload}
          place={{ station: g.station.name, mount: item.mount.path }}
        />
      )))}
    </section>
  );
}

function ListenPage({ slug, lang, setLang, t }) {
  const [data, setData] = useState(null);
  const [phase, setPhase] = useState("loading");
  const [mount, setMount] = useState("");
  const audioRef = useRef(null);
  async function reload() {
    try {
      setData(await api(`/api/public/stations/${slug}`));
      setPhase("ready");
    } catch {
      setPhase("error");
    }
  }
  useEffect(() => {
    let stop = false;
    const tick = () => {
      api(`/api/public/stations/${slug}`).then((row) => {
        if (!stop) { setData(row); setPhase("ready"); }
      }).catch(() => { if (!stop) setPhase("error"); });
    };
    tick();
    const timer = setInterval(tick, 2000);
    return () => { stop = true; clearInterval(timer); };
  }, [slug]);
  const chosen = useMemo(() => data?.mounts?.find((m) => m.path === mount) || data?.mounts?.[0], [data, mount]);
  useEffect(() => {
    const audio = audioRef.current;
    if (!audio || !chosen?.hls_url) return undefined;
    if (audio.canPlayType("application/vnd.apple.mpegurl")) {
      audio.src = chosen.hls_url;
      return undefined;
    }
    if (!Hls.isSupported()) return undefined;
    const hls = new Hls();
    hls.loadSource(chosen.hls_url);
    hls.attachMedia(audio);
    return () => hls.destroy();
  }, [chosen?.hls_url]);
  const title = chosen?.now?.title || data?.name || slug;
  const artist = chosen?.now?.artist || "";
  const pos = finite(chosen?.position_ms) ?? finite(chosen?.now?.elapsed_ms) ?? 0;
  const dur = finite(chosen?.duration_ms) ?? finite(chosen?.now?.duration_ms) ?? 0;
  return (
    <div className="air">
      <div className="air-wrap">
        <div className="air-top">
          <strong className="wordmark">{t("brand")}</strong>
          <button type="button" className="ghost" onClick={() => setLang(lang === "en" ? "fa" : "en")}>{lang === "en" ? "فارسی" : "English"}</button>
        </div>
        {phase === "loading" ? <Notice phase="loading" t={t} /> : null}
        {phase === "error" ? <Notice phase="error" t={t} onRetry={reload} /> : null}
        {phase === "ready" && data ? (
          <section className="listen-card">
            <p className="kicker">{data.genre}</p>
            <h1 dir="auto">{data.name}</h1>
            {data.description ? <p className="muted" dir="auto">{data.description}</p> : null}
            <p className="kicker">{t("cover")}</p>
            <div className="cover-art" data-tone={tone(title)} aria-hidden="true">{String(title).slice(0, 1)}</div>
            {artist ? <p className="artist" dir="auto">{artist}</p> : null}
            <h2 dir="auto">{chosen?.now?.title || t("nothingPlaying")}</h2>
            <p><span className={`badge ${modeClass(chosen?.live ? "source" : chosen?.now?.source === "relay" ? "relay" : "autodj")}`}>{modeLabel(chosen?.live ? "source" : chosen?.now?.source === "relay" ? "relay" : "autodj", t)}</span></p>
            <ListenerProgress position={pos} duration={dur} epoch={`${chosen?.path || ""}:${chosen?.now?.title || ""}`} label={t("nowPlaying")} t={t} />
            {(data.mounts || []).length > 1 ? (
              <label className="field">{t("path")}
                <select dir="ltr" value={chosen?.path || ""} onChange={(e) => setMount(e.target.value)}>
                  {data.mounts.map((m) => <option key={m.path} value={m.path}>{m.path}</option>)}
                </select>
              </label>
            ) : null}
            {(data.mounts || []).length === 0 ? <Notice phase="ready" empty emptyText={t("empty")} t={t} /> : (
              <>
                <p className="kicker">{t("hls")}</p>
                <audio ref={audioRef} controls />
              </>
            )}
            <h3>{t("history")}</h3>
            {(chosen?.history || []).length === 0 ? <p className="state state-empty">{t("historyEmpty")}</p> : (
              <ul className="plain">{(chosen.history || []).map((h, i) => <li key={i} dir="auto">{h.artist ? `${h.artist} — ` : ""}{h.title}</li>)}</ul>
            )}
          </section>
        ) : null}
      </div>
    </div>
  );
}

function ListenerProgress({ position, duration, epoch, label, t }) {
  const play = usePlayhead(position, duration, epoch, 250);
  const pct = play.duration > 0 ? Math.min(100, (play.shown / play.duration) * 100) : 0;
  return (
    <div className="passive">
      <div
        className="rail is-static"
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={Math.round(play.duration || 0)}
        aria-valuenow={Math.round(play.shown)}
        aria-valuetext={`${clock(play.shown)}, ${remainText(play.shown, play.duration)}`}
      >
        <div className="rail-fill" style={{ width: `${pct}%` }} />
      </div>
      <TimeReadout shown={play.shown} duration={play.duration} t={t} />
    </div>
  );
}
