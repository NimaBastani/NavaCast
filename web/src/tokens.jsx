import React, { useEffect, useState } from "react";
import { api } from "./api.js";

const PERMS = ["stations", "mounts", "playlists", "library", "import", "recordings", "schedules", "bots", "listeners", "takeover"];
const EMPTY = () => Object.fromEntries(PERMS.map((key) => [key, false]));

function Switch({ on, label, onClick }) {
  return (
    <button type="button" className={`switch ${on ? "is-on" : ""}`} role="switch" aria-checked={on} onClick={onClick}>
      <span className="switch-lamp" aria-hidden="true" />
      <span>{label}</span>
    </button>
  );
}

function when(ms, t) {
  if (!ms) return t("tokenNever");
  try { return new Date(ms).toLocaleString(); } catch { return t("tokenNever"); }
}

export function TokensPage({ token, t, owner }) {
  const [rows, setRows] = useState([]);
  const [users, setUsers] = useState([]);
  const [phase, setPhase] = useState("loading");
  const [error, setError] = useState("");
  const [note, setNote] = useState("");
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [form, setForm] = useState({ name: "", scope: "owner", user_id: "", expires: "", permissions: EMPTY() });

  async function load() {
    const list = await api("/api/tokens", { token });
    setRows(Array.isArray(list) ? list : []);
    setPhase("ready");
  }

  useEffect(() => {
    if (!owner) return undefined;
    let stop = false;
    load().catch((err) => { if (!stop) { setPhase("error"); setError(err.message); } });
    api("/api/users", { token }).then((list) => { if (!stop) setUsers((Array.isArray(list) ? list : []).filter((row) => !row.owner)); }).catch(() => {});
    return () => { stop = true; };
  }, [token, owner]);

  if (!owner) {
    return (
      <section>
        <div className="page-head"><h1>{t("tokens")}</h1></div>
        <p className="hint">{t("tokenOwnerOnly")}</p>
      </section>
    );
  }

  async function create(event) {
    event.preventDefault();
    setBusy(true);
    setNote("");
    setSecret("");
    try {
      const expires_at = form.expires ? new Date(form.expires).getTime() : null;
      const body = { name: form.name.trim(), scope: form.scope, permissions: form.permissions, expires_at };
      if (form.scope === "user") body.user_id = form.user_id;
      const made = await api("/api/tokens", { token, method: "POST", body });
      setSecret(made.token || "");
      setForm({ name: "", scope: "owner", user_id: "", expires: "", permissions: EMPTY() });
      await load();
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function revoke(id) {
    setBusy(true);
    setNote("");
    try {
      await api(`/api/tokens/${id}`, { token, method: "DELETE" });
      setSecret("");
      await load();
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  async function rotate(id) {
    setBusy(true);
    setNote("");
    try {
      const made = await api(`/api/tokens/${id}/rotate`, { token, method: "POST", body: {} });
      setSecret(made.token || "");
      await load();
    } catch (err) {
      setNote(err.message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <section>
      <div className="page-head">
        <div>
          <h1>{t("tokens")}</h1>
          <p>{t("tokensHint")}</p>
        </div>
      </div>
      {phase === "error" ? <p className="err" role="alert">{error}</p> : null}
      {secret ? (
        <article className="ts-card">
          <h2>{t("tokenSecret")}</h2>
          <input className="ltr" dir="ltr" readOnly value={secret} onFocus={(e) => e.target.select()} />
        </article>
      ) : null}
      <div className="ts-layout">
        <div className="ts-list">
          {rows.map((row) => (
            <article key={row.id} className="ts-card">
              <h2>{row.name}</h2>
              <p className="hint ltr" dir="ltr">{row.prefix}</p>
              <p className="hint">{row.scope === "owner" ? t("scopeOwner") : row.username || t("scopeUser")}</p>
              <p className="hint">{t("tokenExpiry")}: {row.expires_at ? when(row.expires_at, t) : t("unlimited")}</p>
              <p className="hint">{t("tokenLastUsed")}: {when(row.last_used_at, t)}</p>
              <div className="ts-actions">
                <button type="button" className="ghost" disabled={busy} onClick={() => rotate(row.id)}>{t("tokenRotate")}</button>
                <button type="button" className="danger" disabled={busy} onClick={() => revoke(row.id)}>{t("tokenRevoke")}</button>
              </div>
            </article>
          ))}
          {phase === "ready" && rows.length === 0 ? <p className="hint">{t("tokensEmpty")}</p> : null}
        </div>
        <form className="ts-card ts-create" onSubmit={create}>
          <h2>{t("createToken")}</h2>
          <label className="field">{t("tokenName")}<input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} required autoComplete="off" /></label>
          <label className="field">{t("tokenScope")}
            <select value={form.scope} onChange={(e) => setForm({ ...form, scope: e.target.value })}>
              <option value="owner">{t("scopeOwner")}</option>
              <option value="user">{t("scopeUser")}</option>
            </select>
          </label>
          {form.scope === "user" ? (
            <label className="field">{t("tokenUser")}
              <select value={form.user_id} onChange={(e) => setForm({ ...form, user_id: e.target.value })} required>
                <option value="" />
                {users.map((row) => <option key={row.id} value={row.id}>{row.username}</option>)}
              </select>
            </label>
          ) : null}
          <label className="field">{t("tokenExpiry")}
            <input className="ltr" dir="ltr" type="datetime-local" value={form.expires} onChange={(e) => setForm({ ...form, expires: e.target.value })} />
          </label>
          <p className="hint">{t("tokenExpiryHint")}</p>
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
          {note ? <p className="err" role="alert">{note}</p> : null}
          <div className="ts-actions">
            <button type="submit" disabled={busy}>{t("create")}</button>
          </div>
        </form>
      </div>
    </section>
  );
}
