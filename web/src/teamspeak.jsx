import React, { useEffect, useState } from "react";
import { api } from "./api.js";

const COMMANDS = ["play", "queue", "skip", "stop", "np", "vol", "radio", "help"];

function dotClass(status) {
  if (status === "connecting" || status === "in_channel" || status === "error") return status;
  return "off";
}

function statusDetail(error, t) {
  if (error === "timed out connecting") return t("tsTimeout");
  if (error === "failed password") return t("tsBadPassword");
  if (error === "could not connect") return t("tsCouldNot");
  if (error === "the bot stopped") return t("tsStopped");
  return error || "";
}

function statusText(status, t) {
  if (status === "connecting") return t("tsConnecting");
  if (status === "in_channel") return t("tsInChannel");
  if (status === "error") return t("tsError");
  return t("tsDisconnected");
}

function nest(nodes, parent) {
  return nodes.filter((node) => Number(node.parent) === Number(parent)).sort((a, b) => String(a.name).localeCompare(String(b.name)));
}

export function StationOutputs({ stationId, token, t }) {
  const [board, setBoard] = useState({ radio: true, bots: [] });
  const [note, setNote] = useState("");

  useEffect(() => {
    let stop = false;
    async function tick() {
      try {
        const data = await api(`/api/stations/${stationId}/outputs`, { token });
        if (!stop) setBoard(data || { radio: true, bots: [] });
      } catch {
        if (!stop) setBoard({ radio: true, bots: [] });
      }
    }
    tick();
    const timer = setInterval(tick, 2000);
    return () => { stop = true; clearInterval(timer); };
  }, [stationId, token]);

  async function run(task) {
    setNote("");
    try {
      setBoard(await task());
    } catch (err) {
      setNote(err.message || t("failed"));
    }
  }

  return (
    <div className="ts-board">
      <h2>{t("tsFeeds")}</h2>
      <label className="check">
        <input type="checkbox" checked={!!board.radio} onChange={(e) => run(() => api(`/api/stations/${stationId}/outputs`, { token, method: "POST", body: { radio: e.target.checked } }))} />
        {t("tsRadio")}
      </label>
      <div className="ts-chips">
        {(board.bots || []).map((bot) => (
          <span className={bot.feeding ? "ts-chip on" : "ts-chip"} key={bot.id}>
            <button type="button" className="quiet" onClick={() => run(() => api(`/api/stations/${stationId}/outputs`, { token, method: "POST", body: { bot_id: bot.id, feed: !bot.feeding } }))}>
              <i className={`ts-dot ${dotClass(bot.status)}`} aria-hidden="true" /> {bot.nickname}
            </button>
            <a href={`#/teamspeak/${bot.id}`}>{t("tsEdit")}</a>
          </span>
        ))}
        {(board.bots || []).length === 0 ? <a href="#/teamspeak">{t("tsAddBot")}</a> : null}
      </div>
      {note ? <p className="err" role="alert">{note}</p> : null}
    </div>
  );
}

export function TeamSpeakPage({ token, t, botId }) {
  const [pack, setPack] = useState({ master_volume: 100, bots: [] });
  const [stations, setStations] = useState([]);
  const [media, setMedia] = useState([]);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [serverPw, setServerPw] = useState("");
  const [channelPw, setChannelPw] = useState("");
  const [joinPw, setJoinPw] = useState("");
  const [pending, setPending] = useState(null);
  const [draft, setDraft] = useState({ nickname: "NavaCast", host: "", voice_port: 9987 });
  const selected = (pack.bots || []).find((bot) => bot.id === botId) || null;

  useEffect(() => {
    let stop = false;
    async function tick() {
      try {
        const data = await api("/api/bots", { token });
        if (!stop) setPack(data || { master_volume: 100, bots: [] });
      } catch {
        if (!stop) setPack({ master_volume: 100, bots: [] });
      }
    }
    tick();
    const timer = setInterval(tick, 2000);
    return () => { stop = true; clearInterval(timer); };
  }, [token]);

  useEffect(() => {
    api("/api/stations", { token }).then((rows) => setStations(Array.isArray(rows) ? rows : [])).catch(() => setStations([]));
  }, [token]);

  useEffect(() => {
    if (!selected?.station_id) {
      setMedia([]);
      return undefined;
    }
    let stop = false;
    api(`/api/media?station=${selected.station_id}`, { token }).then((rows) => {
      if (!stop) setMedia(Array.isArray(rows) ? rows : []);
    }).catch(() => { if (!stop) setMedia([]); });
    return () => { stop = true; };
  }, [selected?.station_id, token]);

  useEffect(() => {
    setServerPw("");
    setChannelPw("");
    setJoinPw("");
    setPending(null);
  }, [botId]);

  async function run(task) {
    setBusy(true);
    setNote("");
    try {
      const data = await task();
      if (data?.master_volume !== undefined && Array.isArray(data.bots)) setPack(data);
      else if (data?.master_volume !== undefined) setPack((cur) => ({ ...cur, master_volume: data.master_volume }));
      else if (data?.id) {
        setPack((cur) => ({ ...cur, bots: (cur.bots || []).map((bot) => (bot.id === data.id ? data : bot)) }));
      }
      return data;
    } catch (err) {
      setNote(err.message || t("failed"));
      return null;
    } finally {
      setBusy(false);
    }
  }

  async function create(event) {
    event.preventDefault();
    const made = await run(() => api("/api/bots", { token, method: "POST", body: { nickname: draft.nickname.trim(), host: draft.host.trim(), voice_port: Number(draft.voice_port) || 9987 } }));
    if (made?.id) window.location.hash = `#/teamspeak/${made.id}`;
  }

  function saveIdentity(event) {
    event.preventDefault();
    if (!selected) return;
    const body = {
      nickname: event.target.nickname.value,
      phonetic: event.target.phonetic.value,
      away: event.target.away.value,
      host: event.target.host.value,
      voice_port: Number(event.target.voice_port.value) || 9987,
      auto_reconnect: event.target.auto_reconnect.checked,
      prefix: event.target.prefix.value,
      gate: event.target.gate.value,
      gate_group: event.target.gate_group.value,
      commands: COMMANDS.filter((name) => event.target[name].checked),
    };
    if (serverPw) body.server_password = serverPw;
    if (channelPw) body.channel_password = channelPw;
    run(async () => {
      const data = await api(`/api/bots/${selected.id}`, { token, method: "PATCH", body });
      setServerPw("");
      setChannelPw("");
      return data;
    });
  }

  function follow(stationId) {
    if (!selected) return;
    run(async () => {
      await api(`/api/stations/${stationId}/outputs`, { token, method: "POST", body: { bot_id: selected.id, feed: true } });
      return api("/api/bots", { token });
    });
  }

  function onDrop(event, index) {
    event.preventDefault();
    const from = Number(event.dataTransfer.getData("text/plain"));
    if (!selected || !Number.isFinite(from) || from === index) return;
    const ids = (selected.queue || []).map((item) => item.media_id);
    const [moved] = ids.splice(from, 1);
    ids.splice(index, 0, moved);
    run(() => api(`/api/bots/${selected.id}/reorder`, { token, method: "POST", body: { order: ids } }));
  }

  const bots = pack.bots || [];
  return (
    <section className="station">
      <div className="page-head">
        <div>
          <h1>{t("teamspeak")}</h1>
          <p className="hint">{t("tsTabHint")}</p>
        </div>
      </div>
      <label className="field">{t("tsMaster")}
        <input dir="ltr" type="number" min="0" max="100" value={pack.master_volume ?? 100} onChange={(e) => setPack({ ...pack, master_volume: e.target.value })} onBlur={(e) => run(() => api("/api/bots/master", { token, method: "POST", body: { volume: Number(e.target.value) } }))} />
      </label>
      {note ? <p className="err" role="alert">{note}</p> : null}
      <div className="ts-layout">
        <div className="ts-list">
          {bots.length === 0 ? (
            <div className="ts-create">
              <h2>{t("tsEmpty")}</h2>
              <p className="hint">{t("tsTabHint")}</p>
            </div>
          ) : bots.map((bot) => (
            <a className={`ts-card status-${bot.status || "disconnected"}${bot.id === botId ? " on" : ""}`} key={bot.id} href={`#/teamspeak/${bot.id}`}>
              <h2 dir="auto">{bot.nickname}</h2>
              <p><i className={`ts-dot ${dotClass(bot.status)}`} aria-hidden="true" /> {statusText(bot.status, t)}</p>
              {bot.status === "error" && bot.error ? <p className="err" dir="auto">{statusDetail(bot.error, t)}</p> : null}
              <p className="ltr" dir="ltr">{bot.host}:{bot.voice_port}</p>
              <p>{t("tsChannel")}: <span dir="auto">{bot.current_channel || t("tsNoChannel")}</span></p>
              <p>{t("nowPlaying")}: <span dir="auto">{bot.now_playing || bot.station_name || "—"}</span></p>
              <p>{t("tsListening")}: {(bot.people || []).map((p) => p.name).join(", ") || "—"}</p>
            </a>
          ))}
          <form className="ts-create" onSubmit={create}>
            <h2>{t("tsAddBot")}</h2>
            <div className="ts-grid">
              <label className="field">{t("tsNickname")}<input value={draft.nickname} onChange={(e) => setDraft({ ...draft, nickname: e.target.value })} required maxLength={30} /></label>
              <label className="field">{t("tsHost")}<input className="ltr" dir="ltr" value={draft.host} onChange={(e) => setDraft({ ...draft, host: e.target.value })} required autoComplete="off" /></label>
              <label className="field">{t("tsPort")}<input dir="ltr" type="number" min="1" max="65535" value={draft.voice_port} onChange={(e) => setDraft({ ...draft, voice_port: e.target.value })} /></label>
            </div>
            <div className="ts-actions"><button type="submit" disabled={busy}>{t("create")}</button></div>
          </form>
        </div>
        <div className="ts-main">
          {!selected ? <p className="hint">{bots.length ? t("tsEdit") : t("tsTabHint")}</p> : (
            <>
              <div className={`ts-banner status-${selected.status || "disconnected"}`} role="status">
                <i className={`ts-dot ${dotClass(selected.status)}`} aria-hidden="true" />
                <strong>{statusText(selected.status, t)}</strong>
                {selected.status === "in_channel" && selected.current_channel ? <span dir="auto">{selected.current_channel}</span> : null}
                {selected.status === "error" && selected.error ? <span dir="auto">{statusDetail(selected.error, t)}</span> : null}
                {selected.status === "connecting" ? <span>{t("tsTabHint")}</span> : null}
              </div>
              <div className="ts-actions">
                <button type="button" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/connect`, { token, method: "POST", body: {} }))}>{t("tsConnect")}</button>
                <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/disconnect`, { token, method: "POST", body: {} }))}>{t("tsDisconnect")}</button>
                <button type="button" className="danger" disabled={busy} onClick={() => run(async () => { await api(`/api/bots/${selected.id}`, { token, method: "DELETE" }); window.location.hash = "#/teamspeak"; return api("/api/bots", { token }); })}>{t("delete")}</button>
              </div>
              <form className="ts-card" onSubmit={saveIdentity}>
                <h2>{t("tsIdentity")}</h2>
                <div className="ts-grid">
                  <label className="field">{t("tsNickname")}<input name="nickname" defaultValue={selected.nickname} key={`${selected.id}-n`} required maxLength={30} /></label>
                  <label className="field">{t("tsPhonetic")}<input name="phonetic" defaultValue={selected.phonetic || ""} key={`${selected.id}-p`} /></label>
                  <label className="field">{t("tsAway")}<input name="away" defaultValue={selected.away || ""} key={`${selected.id}-a`} /></label>
                  <label className="field">{t("tsHost")}<input name="host" className="ltr" dir="ltr" defaultValue={selected.host} key={`${selected.id}-h`} required /></label>
                  <label className="field">{t("tsPort")}<input name="voice_port" dir="ltr" type="number" min="1" max="65535" defaultValue={selected.voice_port} key={`${selected.id}-v`} /></label>
                  <label className="field">{t("tsServerPassword")}
                    <input type="password" dir="ltr" autoComplete="new-password" value={serverPw} onChange={(e) => setServerPw(e.target.value)} />
                    <small>{selected.server_password_set ? t("tsPasswordSet") : t("tsPasswordEmpty")}</small>
                  </label>
                  <label className="field">{t("tsChannelPassword")}
                    <input type="password" dir="ltr" autoComplete="new-password" value={channelPw} onChange={(e) => setChannelPw(e.target.value)} />
                    <small>{selected.channel_password_set ? t("tsPasswordSet") : t("tsPasswordEmpty")}</small>
                  </label>
                  <label className="field">{t("tsPrefix")}<input name="prefix" dir="ltr" defaultValue={selected.prefix || "!"} key={`${selected.id}-pre`} maxLength={3} required /></label>
                  <label className="field">{t("tsGate")}
                    <select name="gate" defaultValue={selected.gate || "everyone"} key={`${selected.id}-g`}>
                      <option value="everyone">{t("tsEveryone")}</option>
                      <option value="group">{t("tsGroup")}</option>
                    </select>
                  </label>
                  <label className="field">{t("tsGroupName")}<input name="gate_group" defaultValue={selected.gate_group || ""} key={`${selected.id}-gg`} /></label>
                  <label className="check"><input name="auto_reconnect" type="checkbox" defaultChecked={selected.auto_reconnect !== false} key={`${selected.id}-ar`} /> {t("tsAuto")}</label>
                </div>
                <fieldset>
                  <legend>{t("tsCommands")}</legend>
                  {COMMANDS.map((name) => (
                    <label className="check" key={name}><input name={name} type="checkbox" defaultChecked={(selected.commands || COMMANDS).includes(name)} key={`${selected.id}-${name}`} /> {name}</label>
                  ))}
                </fieldset>
                <p>{t("tsDefault")}: <span dir="ltr">{selected.default_channel_id || "—"}</span></p>
                <div className="ts-actions"><button type="submit" disabled={busy}>{t("save")}</button></div>
              </form>
              {selected.status === "in_channel" ? (
                <div className="ts-card">
                  <h2>{t("tsTree")}</h2>
                  <ChannelTree nodes={selected.channels || []} current={selected.current_channel_id} onPick={(node) => { setPending(node); setJoinPw(""); if (!node.locked) run(() => api(`/api/bots/${selected.id}/join`, { token, method: "POST", body: { channel_id: node.id } })); }} />
                  {pending?.locked ? (
                    <form className="ts-actions" onSubmit={(event) => { event.preventDefault(); run(() => api(`/api/bots/${selected.id}/join`, { token, method: "POST", body: { channel_id: pending.id, password: joinPw } })); setJoinPw(""); }}>
                      <label className="field">{t("tsLocked")}<input type="password" dir="ltr" autoComplete="new-password" value={joinPw} onChange={(e) => setJoinPw(e.target.value)} /></label>
                      <button type="submit" disabled={busy}>{t("tsJoin")}</button>
                    </form>
                  ) : null}
                  <button type="button" className="ghost" disabled={busy || !selected.current_channel_id} onClick={() => run(() => api(`/api/bots/${selected.id}`, { token, method: "PATCH", body: { default_channel_id: selected.current_channel_id } }))}>{t("tsSetDefault")}</button>
                </div>
              ) : null}
              <div className="ts-card">
                <h2>{t("tsMode")}</h2>
                <label className="field">{t("tsFollows")}
                  <select dir="auto" value={selected.station_id || ""} onChange={(e) => { if (e.target.value) follow(e.target.value); }}>
                    <option value="">{t("none")}</option>
                    {stations.map((s) => <option key={s.id} value={s.id}>{s.name}</option>)}
                  </select>
                </label>
                <p>{t("nowPlaying")}: <span dir="auto">{selected.now_playing || "—"}</span></p>
                <div className="ts-actions">
                  <label className="field">{t("tsVolume")}<input dir="ltr" type="number" min="0" max="100" defaultValue={selected.volume} key={`${selected.id}-vol-${selected.volume}`} onBlur={(e) => run(() => api(`/api/bots/${selected.id}/volume`, { token, method: "POST", body: { volume: Number(e.target.value) } }))} /></label>
                  <label className="field">{t("tsRepeat")}
                    <select value={selected.repeat || "off"} onChange={(e) => run(() => api(`/api/bots/${selected.id}/repeat`, { token, method: "POST", body: { repeat: e.target.value } }))}>
                      <option value="off">{t("tsRepeatOff")}</option>
                      <option value="one">{t("tsRepeatOne")}</option>
                      <option value="all">{t("tsRepeatAll")}</option>
                    </select>
                  </label>
                </div>
                <div className="ts-actions">
                  <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/pause`, { token, method: "POST", body: {} }))}>{t("tsPause")}</button>
                  <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/skip`, { token, method: "POST", body: {} }))}>{t("skip")}</button>
                  <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/stop`, { token, method: "POST", body: {} }))}>{t("tsStop")}</button>
                  <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/radio`, { token, method: "POST", body: {} }))}>{t("tsBack")}</button>
                  <button type="button" className="ghost" disabled={busy} onClick={() => run(() => api(`/api/bots/${selected.id}/clear`, { token, method: "POST", body: {} }))}>{t("clear")}</button>
                </div>
                <label className="field">{t("tsPlay")}
                  <select dir="auto" defaultValue="" onChange={(e) => { if (e.target.value) run(() => api(`/api/bots/${selected.id}/play`, { token, method: "POST", body: { media_id: e.target.value } })); e.target.value = ""; }}>
                    <option value="">{t("none")}</option>
                    {media.map((item) => <option key={item.id} value={item.id}>{item.artist ? `${item.artist} — ` : ""}{item.title || item.filename}</option>)}
                  </select>
                </label>
                {(selected.queue || []).length === 0 ? <p className="hint">{t("queueEmpty")}</p> : (
                  <ol className="ts-queue">
                    {(selected.queue || []).map((item, index) => (
                      <li key={`${item.media_id}-${index}`} draggable onDragStart={(e) => e.dataTransfer.setData("text/plain", String(index))} onDragOver={(e) => e.preventDefault()} onDrop={(e) => onDrop(e, index)} dir="auto">
                        {item.current ? "● " : ""}{item.artist ? `${item.artist} — ` : ""}{item.title}
                      </li>
                    ))}
                  </ol>
                )}
              </div>
              <div className="ts-card">
                <h2>{t("tsPeople")}</h2>
                {(selected.people || []).length === 0 ? <p className="hint">{t("tsNoChannel")}</p> : (selected.people || []).map((person) => (
                  <div className="ts-person" key={person.id}>
                    <strong dir="auto">{person.name}</strong>
                    <button type="button" className="ghost" disabled={busy || !person.mute_ok} title={person.mute_reason || ""} onClick={() => run(() => api(`/api/bots/${selected.id}/mute`, { token, method: "POST", body: { client_id: person.id, muted: !person.muted } }))}>{t("tsMute")}{person.mute_reason ? ` — ${person.mute_reason}` : ""}</button>
                    <button type="button" className="ghost" disabled={busy || !person.kick_ok} title={person.kick_reason || ""} onClick={() => run(() => api(`/api/bots/${selected.id}/kick`, { token, method: "POST", body: { client_id: person.id } }))}>{t("tsKick")}{person.kick_reason ? ` — ${person.kick_reason}` : ""}</button>
                    <label className="field">{t("tsMove")}
                      <select dir="auto" defaultValue="" disabled={!person.move_ok} onChange={(e) => { if (e.target.value) run(() => api(`/api/bots/${selected.id}/move`, { token, method: "POST", body: { client_id: person.id, channel_id: Number(e.target.value) } })); }}>
                        <option value="">{person.move_reason || t("none")}</option>
                        {(selected.channels || []).map((ch) => <option key={ch.id} value={ch.id}>{ch.name}</option>)}
                      </select>
                    </label>
                  </div>
                ))}
              </div>
              <div className="ts-card">
                <h2>{t("tsLog")}</h2>
                {(selected.log || []).length === 0 ? <p className="hint">—</p> : (
                  <ol className="ts-log">{(selected.log || []).map((line, index) => <li key={`${index}-${line}`} dir="auto">{line}</li>)}</ol>
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </section>
  );
}

function ChannelTree({ nodes, current, onPick }) {
  function branch(parent) {
    return nest(nodes, parent).map((node) => (
      <li key={node.id}>
        <button type="button" className={Number(node.id) === Number(current) ? "on" : ""} onClick={() => onPick(node)}>
          {node.locked ? "🔒 " : ""}<span dir="auto">{node.name}</span> <span dir="ltr">{node.clients}</span>
        </button>
        <ul>{branch(node.id)}</ul>
      </li>
    ));
  }
  if (!nodes.length) return null;
  return <ul className="ts-tree">{branch(0)}</ul>;
}
