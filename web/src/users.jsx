import React, { useEffect, useState } from "react";
import { api } from "./api.js";

const PERMS = ["stations", "mounts", "playlists", "library", "import", "recordings", "schedules", "bots", "listeners", "takeover"];
const QUOTAS = ["max_stations", "max_mounts", "max_playlists", "max_tracks", "max_library_bytes", "max_bots", "max_recordings", "max_recording_bytes"];
const BYTE_QUOTAS = new Set(["max_library_bytes", "max_recording_bytes"]);

const EMPTY_PERMS = () => Object.fromEntries(PERMS.map((key) => [key, true]));
const EMPTY_QUOTAS = () => Object.fromEntries(QUOTAS.map((key) => [key, ""]));

function bytes(n) {
  const v = Number(n) || 0;
  if (v < 1024) return `${v} B`;
  if (v < 1048576) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / 1048576).toFixed(1)} MB`;
}

function capText(used, cap, t, asBytes) {
  const shown = asBytes ? bytes(used) : String(used ?? 0);
  if (cap == null) return `${shown} · ${t("unlimited")}`;
  const limit = asBytes ? bytes(cap) : String(cap);
  return `${shown} / ${limit}`;
}

export function AccountUsage({ token, t }) {
  const [me, setMe] = useState(null);
  useEffect(() => {
    let stop = false;
    api("/api/me", { token }).then((row) => { if (!stop) setMe(row); }).catch(() => {});
    return () => { stop = true; };
  }, [token]);
  if (!me) return null;
  const usage = me.usage || {};
  const quotas = me.quotas || {};
  return (
    <section className="panel user-usage">
      <h2>{t("yourAccount")}</h2>
      <p className="hint">{me.owner ? t("ownerUnlimited") : t("usageHint")}</p>
      <div className="ts-grid">
        {QUOTAS.map((key) => (
          <article key={key} className="metric">
            <span>{t(key)}</span>
            <strong className="ltr" dir="ltr">{capText(usage[key.replace("max_", "")] ?? usage[usageKey(key)], quotas[key], t, BYTE_QUOTAS.has(key))}</strong>
          </article>
        ))}
      </div>
    </section>
  );
}

function usageKey(quota) {
  return {
    max_stations: "stations",
    max_mounts: "mounts",
    max_playlists: "playlists",
    max_tracks: "tracks",
    max_library_bytes: "library_bytes",
    max_bots: "bots",
    max_recordings: "recordings",
    max_recording_bytes: "recording_bytes",
  }[quota];
}

function Switch({ on, label, onClick }) {
  return (
    <button type="button" className={`switch ${on ? "is-on" : ""}`} role="switch" aria-checked={on} onClick={onClick}>
      <span className="switch-lamp" aria-hidden="true" />
      <span>{label}</span>
    </button>
  );
}

export function UsersPage({ token, t, userId, owner }) {
  const [users, setUsers] = useState([]);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [detail, setDetail] = useState(null);
  const [form, setForm] = useState({ username: "", password: "", permissions: EMPTY_PERMS(), quotas: EMPTY_QUOTAS() });
  const [edit, setEdit] = useState(null);
  const [password, setPassword] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);

  async function loadList() {
    const rows = await api("/api/users", { token });
    setUsers(Array.isArray(rows) ? rows : []);
    setPhase("ready");
  }

  useEffect(() => {
    if (!owner) return undefined;
    let stop = false;
    loadList().catch((err) => { if (!stop) { setPhase("error"); setError(err.message); } });
    return () => { stop = true; };
  }, [token, owner]);

  useEffect(() => {
    if (!owner || !userId) { setDetail(null); return undefined; }
    let stop = false;
    const tick = () => {
      api(`/api/users/${userId}`, { token }).then((row) => {
        if (stop) return;
        setDetail(row);
        setEdit((current) => current && current.id === row.user.id ? current : fromUser(row.user));
      }).catch((err) => { if (!stop) setError(err.message); });
    };
    tick();
    const timer = setInterval(tick, 2000);
    return () => { stop = true; clearInterval(timer); };
  }, [token, owner, userId]);

  if (!owner) {
    return (
      <section>
        <div className="page-head"><h1>{t("users")}</h1></div>
        <p className="hint">{t("ownerOnly")}</p>
      </section>
    );
  }

  async function createUser(e) {
    e.preventDefault();
    setBusy(true);
    setNote("");
    try {
      const created = await api("/api/users", { token, method: "POST", body: payload(form, form.password) });
      setForm({ username: "", password: "", permissions: EMPTY_PERMS(), quotas: EMPTY_QUOTAS() });
      await loadList();
      window.location.hash = `#/users/${created.id}`;
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function save() {
    if (!edit) return;
    setBusy(true);
    setNote("");
    try {
      const body = payload(edit, password);
      await api(`/api/users/${edit.id}`, { token, method: "PATCH", body });
      setPassword("");
      await loadList();
      setNote(t("saved"));
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function setDisabled(disabled) {
    if (!edit) return;
    setBusy(true);
    setNote("");
    try {
      await api(`/api/users/${edit.id}`, { token, method: "PATCH", body: { disabled } });
      await loadList();
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!edit) return;
    if (!window.confirm(t("deleteUserConfirm"))) return;
    setBusy(true);
    try {
      await api(`/api/users/${edit.id}`, { token, method: "DELETE" });
      window.location.hash = "#/users";
      await loadList();
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  const selected = users.find((row) => row.id === userId);
  return (
    <section>
      <div className="page-head">
        <div>
          <h1>{t("users")}</h1>
          <p>{t("usersHint")}</p>
        </div>
      </div>
      {phase === "error" ? <p className="err" role="alert">{error}</p> : null}
      <div className="ts-layout">
        <div className="ts-list">
          {users.filter((row) => !row.owner).map((row) => (
            <a key={row.id} className={`ts-card ${row.id === userId ? "on" : ""}`} href={`#/users/${row.id}`}>
              <h2>{row.username}</h2>
              <p className="hint">{row.disabled ? t("disabled") : t("enabled")}</p>
              <p className="hint ltr" dir="ltr">{t("stations")} {row.usage?.stations ?? 0}{row.quotas?.max_stations == null ? "" : ` / ${row.quotas.max_stations}`}</p>
            </a>
          ))}
          {phase === "ready" && users.filter((row) => !row.owner).length === 0 ? <p className="hint">{t("usersEmpty")}</p> : null}
        </div>
        <div className="ts-main">
          {userId && detail ? (
            <LiveAccount
              detail={detail}
              edit={edit}
              setEdit={setEdit}
              password={password}
              setPassword={setPassword}
              note={note}
              busy={busy}
              t={t}
              onSave={save}
              onDisable={setDisabled}
              onDelete={remove}
              disabled={selected?.disabled || detail.user.disabled}
            />
          ) : (
            <form className="ts-card ts-create" onSubmit={createUser}>
              <h2>{t("createUser")}</h2>
              <p className="hint">{t("sellHint")}</p>
              <div className="ts-grid">
                <label className="field">{t("username")}<input value={form.username} onChange={(e) => setForm({ ...form, username: e.target.value })} required autoComplete="off" /></label>
                <label className="field">{t("password")}<input type="password" value={form.password} onChange={(e) => setForm({ ...form, password: e.target.value })} required autoComplete="new-password" minLength={8} /></label>
              </div>
              <Grants form={form} setForm={setForm} t={t} />
              {note ? <p className="err" role="alert">{note}</p> : null}
              <div className="ts-actions">
                <button type="submit" disabled={busy}>{t("create")}</button>
              </div>
            </form>
          )}
        </div>
      </div>
    </section>
  );
}

function fromUser(user) {
  const quotas = {};
  QUOTAS.forEach((key) => {
    const value = user.quotas?.[key];
    quotas[key] = value == null ? "" : String(value);
  });
  return { id: user.id, permissions: { ...EMPTY_PERMS(), ...user.permissions }, quotas };
}

function payload(form, password) {
  const quotas = {};
  QUOTAS.forEach((key) => {
    const raw = String(form.quotas[key] ?? "").trim();
    quotas[key] = raw === "" ? null : Number(raw);
  });
  const body = { permissions: form.permissions, quotas };
  if (form.username) body.username = form.username;
  if (password) body.password = password;
  return body;
}

function Grants({ form, setForm, t }) {
  return (
    <>
      <h3>{t("permissions")}</h3>
      <div className="ts-grid">
        {PERMS.map((key) => (
          <Switch
            key={key}
            on={!!form.permissions[key]}
            label={t(`perm_${key}`)}
            onClick={() => setForm({ ...form, permissions: { ...form.permissions, [key]: !form.permissions[key] } })}
          />
        ))}
      </div>
      <h3>{t("quotas")}</h3>
      <p className="hint">{t("quotaHint")}</p>
      <div className="ts-grid">
        {QUOTAS.map((key) => (
          <label key={key} className="field">{t(key)}
            <input
              className="ltr"
              dir="ltr"
              inputMode="numeric"
              placeholder={t("unlimited")}
              value={form.quotas[key]}
              onChange={(e) => setForm({ ...form, quotas: { ...form.quotas, [key]: e.target.value.replace(/[^\d]/g, "") } })}
            />
          </label>
        ))}
      </div>
    </>
  );
}

function LiveAccount({ detail, edit, setEdit, password, setPassword, note, busy, t, onSave, onDisable, onDelete, disabled }) {
  const user = detail.user;
  const bots = detail.bots?.bots || [];
  if (!edit) return null;
  return (
    <>
      <article className="ts-card">
        <h2>{user.username}</h2>
        <p className="hint">{disabled ? t("disabled") : t("enabled")}</p>
        <Grants form={edit} setForm={setEdit} t={t} />
        <label className="field">{t("resetPassword")}
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" placeholder={t("leaveBlank")} />
        </label>
        {note ? <p className={note === t("saved") ? "hint" : "err"} role={note === t("saved") ? "status" : "alert"}>{note}</p> : null}
        <div className="ts-actions">
          <button type="button" disabled={busy} onClick={onSave}>{t("save")}</button>
          <button type="button" className="ghost" disabled={busy} onClick={() => onDisable(!disabled)}>{disabled ? t("enable") : t("disable")}</button>
          <button type="button" className="danger" disabled={busy} onClick={onDelete}>{t("deleteUser")}</button>
        </div>
      </article>
      <article className="ts-card">
        <h2>{t("runningNow")}</h2>
        <div className="ts-grid">
          <article className="metric"><span>{t("library")}</span><strong className="ltr" dir="ltr">{user.usage?.tracks ?? 0}</strong><em className="ltr" dir="ltr">{bytes(user.usage?.library_bytes)}</em></article>
          <article className="metric"><span>{t("recordings")}</span><strong className="ltr" dir="ltr">{user.usage?.recordings ?? 0}</strong><em className="ltr" dir="ltr">{bytes(user.usage?.recording_bytes)}</em></article>
        </div>
        <h3>{t("stations")}</h3>
        {(detail.stations || []).length === 0 ? <p className="hint">{t("noneYet")}</p> : (
          <ul className="ts-queue">
            {(detail.stations || []).map((row) => <li key={row.id}>{row.name} <span className="ltr" dir="ltr">{row.slug}</span></li>)}
          </ul>
        )}
        <h3>{t("mounts")}</h3>
        {(detail.mounts || []).length === 0 ? <p className="hint">{t("noneYet")}</p> : (
          <ul className="ts-queue">
            {(detail.mounts || []).map((row) => (
              <li key={row.id}><span className="ltr" dir="ltr">{row.path}</span> · {row.playing ? t("playing") : t("idle")} · {t("listeners")} {row.listeners || 0}</li>
            ))}
          </ul>
        )}
        <h3>{t("playlists")}</h3>
        {(detail.playlists || []).length === 0 ? <p className="hint">{t("noneYet")}</p> : (
          <ul className="ts-queue">{(detail.playlists || []).map((row) => <li key={row.id}>{row.name}</li>)}</ul>
        )}
        <h3>{t("teamspeak")}</h3>
        {bots.length === 0 ? <p className="hint">{t("noneYet")}</p> : (
          <div className="ts-chips">
            {bots.map((bot) => (
              <span key={bot.id} className="ts-chip">
                <span className={`ts-dot ${bot.status || "disconnected"}`} />
                {bot.nickname}
                <span className="hint">{t(`ts${statusKey(bot.status)}`) || bot.status}</span>
              </span>
            ))}
          </div>
        )}
        <h3>{t("recordings")}</h3>
        {(detail.recordings || []).length === 0 ? <p className="hint">{t("noneYet")}</p> : (
          <ul className="ts-queue">
            {(detail.recordings || []).map((row) => <li key={row.id}><span className="ltr" dir="ltr">{row.filename}</span> · {bytes(row.bytes)}</li>)}
          </ul>
        )}
      </article>
    </>
  );
}

function statusKey(status) {
  if (status === "in_channel") return "InChannel";
  if (status === "connecting") return "Connecting";
  if (status === "error") return "Error";
  return "Disconnected";
}
