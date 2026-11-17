import { useCallback, useEffect, useMemo, useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Machine = { name: string; input: string; hotkey: string };
type KvmRole = "off" | "server" | "client";
type KvmConfig = {
  role: KvmRole;
  deskflow_core: string;
  settings: string;
  poll_ms: number;
  restart_delay_ms: number;
  screen_name: string;
  effect: EffectConfig;
};
type EffectStyle = "rift" | "smoke" | "glow" | "particles";
type EffectConfig = {
  enabled: boolean;
  style: EffectStyle;
  duration_ms: number;
  color: string;
  port: number;
  peer: string;
};
type StreamConfig = {
  moonlight: string;
  enabled: boolean;
  sunshine_autostart: boolean;
  sunshine: string;
  sunshine_formula: string;
};
type StreamProfile = { name: string; host: string; app: string; args: string[]; only_on: string };
type TraySettings = {
  inputs: Record<string, number>;
  machines: Machine[];
  tray: { autostart: boolean; pbp_hotkey: string };
  kvm: KvmConfig;
  stream: StreamConfig;
  streams: StreamProfile[];
};
/** Profile row in the editor: args edited as one line. */
type StreamRow = Omit<StreamProfile, "args"> & { argsText: string };
type Status = {
  input: number | null;
  input_alias: string | null;
  machine: number | null;
  pbp: string | null;
  pbp_on: boolean;
  error: string | null;
  kvm: string;
};
type Payload = {
  settings: TraySettings;
  config_path: string;
  config_error: string | null;
  has_pbp: boolean;
  platform: string;
  hotkey_errors: string[];
  status: Status;
  kvm_core: string | null;
  moonlight: string | null;
};
type InputRow = { alias: string; value: string };

const hex = (v: number) => "0x" + v.toString(16).toUpperCase().padStart(2, "0");

function parseValue(s: string): number | null {
  const t = s.trim();
  const n = /^0x[0-9a-f]+$/i.test(t) ? parseInt(t.slice(2), 16) : /^\d+$/.test(t) ? parseInt(t, 10) : NaN;
  return Number.isInteger(n) && n >= 0 && n <= 0xffff ? n : null;
}

/** KeyboardEvent → "Ctrl+Alt+1" (format understood by the Rust hotkey parser). */
function hotkeyFromEvent(e: KeyboardEvent): string | null {
  const code = e.code;
  if (/^(Control|Alt|Shift|Meta|OS)(Left|Right)?$/.test(code)) return null;
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  if (mods.length === 0) return null;
  const key = code.startsWith("Digit") ? code.slice(5) : code.startsWith("Key") ? code.slice(3) : code;
  return [...mods, key].join("+");
}

function HotkeyField({ value, onChange, mac }: { value: string; onChange: (v: string) => void; mac: boolean }) {
  const [recording, setRecording] = useState(false);
  // character the shortcut would steal on the current keyboard layout (Ctrl+Alt = AltGr)
  const [typed, setTyped] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    if (!value) {
      setTyped(null);
      return;
    }
    invoke<string | null>("hotkey_conflict", { hotkey: value })
      .then((c) => live && setTyped(c))
      .catch(() => live && setTyped(null));
    return () => {
      live = false;
    };
  }, [value]);
  const label = mac
    ? value.replace(/Ctrl/g, "⌃").replace(/Alt/g, "⌥").replace(/Shift/g, "⇧").replace(/Super/g, "⌘").replace(/\+/g, "")
    : value;
  return (
    <div>
      <input
        className={"field font-mono " + (recording ? "border-rose-500" : "")}
        readOnly
        value={recording ? "press a shortcut…" : label || "—"}
        title="Click and press a shortcut. Backspace = clear, Esc = cancel."
        onFocus={() => setRecording(true)}
        onBlur={() => setRecording(false)}
        onKeyDown={(e) => {
          e.preventDefault();
          if (e.key === "Escape") return e.currentTarget.blur();
          if ((e.key === "Backspace" || e.key === "Delete") && !e.ctrlKey && !e.altKey && !e.metaKey) {
            onChange("");
            return e.currentTarget.blur();
          }
          const hk = hotkeyFromEvent(e);
          if (hk) {
            onChange(hk);
            e.currentTarget.blur();
          }
        }}
      />
      {typed && !recording && (
        <p className="mt-0.5 text-xs text-amber-500">
          Types “{typed}” – you could no longer type that character. Add Shift.
        </p>
      )}
    </div>
  );
}

type SunshineInfo = { state: "running" | "stopped" | "missing"; has_control: boolean };

/** Sunshine on this machine: state, on/off, autostart, Windows service permission. */
function SunshinePanel({
  mac,
  autostart,
  onAutostart,
}: {
  mac: boolean;
  autostart: boolean;
  onAutostart: (v: boolean) => void;
}) {
  const [info, setInfo] = useState<SunshineInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<SunshineInfo>("sunshine_status")
      .then(setInfo)
      .catch((e) => setError(String(e)));
  }, []);

  const run = (cmd: string, args?: Record<string, unknown>) => {
    setBusy(true);
    setError(null);
    invoke<SunshineInfo>(cmd, args)
      .then(setInfo)
      .catch((e) => {
        setError(String(e));
        invoke<SunshineInfo>("sunshine_status").then(setInfo).catch(() => {});
      })
      .finally(() => setBusy(false));
  };

  const label =
    info === null ? "…" : info.state === "running" ? "running" : info.state === "stopped" ? "stopped" : "not installed";

  return (
    <div className="space-y-2 rounded-lg border border-zinc-200 p-3 text-sm dark:border-zinc-700">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">Sunshine on this computer:</span>
        <span className={info?.state === "running" ? "text-emerald-600" : "text-zinc-500"}>{label}</span>
        {info && info.state !== "missing" && (
          <button
            className="btn ml-auto"
            disabled={busy || (!mac && !info.has_control)}
            onClick={() => run("sunshine_set", { on: info.state !== "running" })}
          >
            {info.state === "running" ? "Stop" : "Start"}
          </button>
        )}
      </div>
      {info && info.state !== "missing" && !mac && !info.has_control && (
        <div className="flex flex-wrap items-center gap-2 text-xs text-amber-600">
          <span>Windows does not let regular users start the Sunshine service.</span>
          <button className="btn" disabled={busy} onClick={() => run("sunshine_grant")}>
            Allow (once, as administrator)
          </button>
        </div>
      )}
      <label className="flex items-center gap-2">
        <input type="checkbox" checked={autostart} onChange={(e) => onAutostart(e.target.checked)} />
        Start Sunshine with monswitch (and stop it on quit)
      </label>
      {error && <p className="text-xs text-red-600">{error}</p>}
    </div>
  );
}

function Section({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <section className="space-y-2">
      <div>
        <h2 className="text-sm font-semibold uppercase tracking-wide text-zinc-500">{title}</h2>
        {hint && <p className="text-xs text-zinc-500">{hint}</p>}
      </div>
      {children}
    </section>
  );
}

export default function App() {
  const [payload, setPayload] = useState<Payload | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [inputs, setInputs] = useState<InputRow[]>([]);
  const [machines, setMachines] = useState<Machine[]>([]);
  const [pbpHotkey, setPbpHotkey] = useState("");
  const [autostart, setAutostart] = useState(false);
  const [kvm, setKvm] = useState<KvmConfig | null>(null);
  const [streams, setStreams] = useState<StreamRow[]>([]);
  const [streamCfg, setStreamCfg] = useState<StreamConfig>({
    moonlight: "",
    enabled: true,
    sunshine_autostart: false,
    sunshine: "",
    sunshine_formula: "",
  });
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback((p: Payload) => {
    setPayload(p);
    setStatus(p.status);
    setInputs(Object.entries(p.settings.inputs).map(([alias, v]) => ({ alias, value: hex(v) })));
    setMachines(p.settings.machines.map((m) => ({ ...m })));
    setPbpHotkey(p.settings.tray.pbp_hotkey);
    setAutostart(p.settings.tray.autostart);
    setKvm({ ...p.settings.kvm });
    setStreams(p.settings.streams.map(({ args, ...rest }) => ({ ...rest, argsText: args.join(" ") })));
    setStreamCfg({ ...p.settings.stream });
  }, []);

  const refreshStatus = useCallback(() => {
    setBusy(true);
    invoke<Status>("get_status")
      .then(setStatus)
      .catch((e) => setMessage({ ok: false, text: String(e) }))
      .finally(() => setBusy(false));
  }, []);

  useEffect(() => {
    invoke<Payload>("get_settings").then(load).catch((e) => setMessage({ ok: false, text: String(e) }));
    refreshStatus();
    const un = listen<Status>("status", (e) => {
      setStatus(e.payload);
      setBusy(false);
    });
    return () => {
      un.then((f) => f());
    };
  }, [load, refreshStatus]);

  const aliases = useMemo(() => inputs.map((r) => r.alias.trim().toLowerCase()).filter(Boolean), [inputs]);
  const mac = payload?.platform === "macos";

  const save = async () => {
    const map: Record<string, number> = {};
    for (const r of inputs) {
      const alias = r.alias.trim().toLowerCase();
      if (!alias) continue;
      const v = parseValue(r.value);
      if (v === null) return setMessage({ ok: false, text: `Invalid value for '${alias}': ${r.value}` });
      if (alias in map) return setMessage({ ok: false, text: `Alias '${alias}' is used twice` });
      map[alias] = v;
    }
    const settings: TraySettings = {
      inputs: map,
      machines: machines.map((m) => ({ ...m, name: m.name.trim() })),
      tray: { autostart, pbp_hotkey: pbpHotkey },
      kvm: kvm ?? payload!.settings.kvm,
      stream: streamCfg,
      streams: streams.map(({ argsText, ...rest }) => ({
        ...rest,
        args: argsText.split(/\s+/).filter(Boolean),
      })),
    };
    setSaving(true);
    try {
      const p = await invoke<Payload>("save_settings", { settings });
      load(p);
      setMessage(
        p.hotkey_errors.length
          ? { ok: false, text: "Saved, but: " + p.hotkey_errors.join("; ") }
          : { ok: true, text: "Saved" },
      );
    } catch (e) {
      setMessage({ ok: false, text: String(e) });
    } finally {
      setSaving(false);
    }
  };

  if (!payload) return <p className="p-6 text-sm text-zinc-500">Loading…</p>;

  const statusLine = status?.error
    ? `Monitor not reachable: ${status.error}`
    : status?.pbp_on
      ? `${(status.pbp ?? "").toUpperCase()} – second window: ${status.input_alias ?? "?"}`
      : status?.machine != null
        ? `Active: ${machines[status.machine]?.name ?? "?"}`
        : status?.input != null
          ? `Active input: ${status.input_alias ?? hex(status.input)}`
          : "Reading monitor state…";

  return (
    <main className="mx-auto max-w-2xl space-y-6 p-5">
      <header className="space-y-3">
        <div className="flex items-center justify-between">
          <h1 className="text-lg font-semibold">monswitch</h1>
          <button className="btn" onClick={refreshStatus} disabled={busy}>
            {busy ? "…" : "Refresh"}
          </button>
        </div>
        <p className={"rounded-md px-3 py-2 text-sm " + (status?.error ? "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-200" : "bg-zinc-200/60 dark:bg-zinc-800")}>
          {statusLine}
          {status?.kvm && payload.settings.kvm.role !== "off" && (
            <span className="block text-xs opacity-75">{status.kvm}</span>
          )}
        </p>
        <div className="flex flex-wrap gap-2">
          {payload.settings.machines.map((m, i) => (
            <button
              key={i}
              className={"btn " + (status?.machine === i ? "border-rose-500 text-rose-600 dark:text-rose-400" : "")}
              disabled={busy}
              onClick={() => {
                setBusy(true);
                invoke("switch_machine", { index: i });
              }}
            >
              {m.name}
            </button>
          ))}
          {payload.has_pbp && (
            <button
              className={"btn " + (status?.pbp_on ? "border-rose-500 text-rose-600 dark:text-rose-400" : "")}
              disabled={busy}
              onClick={() => {
                setBusy(true);
                invoke("toggle_pbp");
              }}
            >
              PbP {status?.pbp_on ? "off" : "on"}
            </button>
          )}
        </div>
      </header>

      {payload.config_error && (
        <p className="rounded-md bg-amber-100 px-3 py-2 text-sm text-amber-900 dark:bg-amber-950 dark:text-amber-200">
          Could not load the config, running with defaults: {payload.config_error}
        </p>
      )}

      <Section title="Inputs" hint="Alias → value of VCP 0x60 (hex 0x0F or decimal). `monswitch caps` lists the values your monitor accepts.">
        {inputs.map((r, i) => (
          <div key={i} className="grid grid-cols-[1fr_8rem_auto] gap-2">
            <input
              className="field"
              value={r.alias}
              placeholder="alias"
              onChange={(e) => setInputs(inputs.map((x, j) => (j === i ? { ...x, alias: e.target.value } : x)))}
            />
            <input
              className={"field font-mono " + (parseValue(r.value) === null ? "border-red-500" : "")}
              value={r.value}
              onChange={(e) => setInputs(inputs.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))}
            />
            <button className="btn" onClick={() => setInputs(inputs.filter((_, j) => j !== i))} title="Remove">
              ✕
            </button>
          </div>
        ))}
        <button className="btn" onClick={() => setInputs([...inputs, { alias: "", value: "0x" }])}>
          + input
        </button>
      </Section>

      <Section title="Computers in the menu" hint={mac ? "Shortcuts work system-wide. ⌥ = Option." : "Shortcuts work system-wide. Ctrl+Alt = AltGr – a shortcut that types a character on your keyboard layout is flagged."}>
        {machines.map((m, i) => (
          <div key={i} className="grid grid-cols-[1fr_7rem_9rem_auto] gap-2">
            <input
              className="field"
              value={m.name}
              placeholder="name"
              onChange={(e) => setMachines(machines.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))}
            />
            <select
              className="field"
              value={m.input}
              onChange={(e) => setMachines(machines.map((x, j) => (j === i ? { ...x, input: e.target.value } : x)))}
            >
              {!aliases.includes(m.input) && <option value={m.input}>{m.input || "—"}</option>}
              {aliases.map((a) => (
                <option key={a} value={a}>
                  {a}
                </option>
              ))}
            </select>
            <HotkeyField
              value={m.hotkey}
              mac={mac}
              onChange={(hk) => setMachines(machines.map((x, j) => (j === i ? { ...x, hotkey: hk } : x)))}
            />
            <button className="btn" onClick={() => setMachines(machines.filter((_, j) => j !== i))} title="Remove">
              ✕
            </button>
          </div>
        ))}
        <button
          className="btn"
          onClick={() => setMachines([...machines, { name: "", input: aliases[0] ?? "", hotkey: "" }])}
        >
          + computer
        </button>
      </Section>

      {payload.has_pbp && (
        <Section title="PbP" hint="System-wide shortcut that turns PbP on/off.">
          <div className="w-48">
            <HotkeyField value={pbpHotkey} onChange={setPbpHotkey} mac={mac} />
          </div>
        </Section>
      )}

      {kvm && (
        <Section
          title="Software KVM (Deskflow)"
          hint="One keyboard and mouse across both halves in PbP. Set Deskflow up once in its own app (layout, clipboard, trust); monswitch only starts and stops it."
        >
          <div className="grid grid-cols-[9rem_1fr] items-center gap-2 text-sm">
            <label htmlFor="kvm-role">Role</label>
            <select
              id="kvm-role"
              className="field"
              value={kvm.role}
              onChange={(e) => setKvm({ ...kvm, role: e.target.value as KvmRole })}
            >
              <option value="off">off</option>
              <option value="server">server – this computer gets the keyboard in PbP</option>
              <option value="client">client – the other computer</option>
            </select>
            <label htmlFor="kvm-core">deskflow-core</label>
            <input
              id="kvm-core"
              className="field font-mono text-xs"
              value={kvm.deskflow_core}
              placeholder={payload.kvm_core ?? "not found – install Deskflow"}
              onChange={(e) => setKvm({ ...kvm, deskflow_core: e.target.value })}
            />
            <label htmlFor="kvm-settings">Settings file</label>
            <input
              id="kvm-settings"
              className="field font-mono text-xs"
              value={kvm.settings}
              placeholder="Deskflow's default settings"
              onChange={(e) => setKvm({ ...kvm, settings: e.target.value })}
            />
          </div>
          {kvm.role !== "off" && !payload.kvm_core && !kvm.deskflow_core && (
            <p className="text-xs text-red-600">deskflow-core was not found.</p>
          )}
          <h3 className="pt-2 text-xs font-semibold uppercase tracking-wide text-zinc-500">Edge transition effect</h3>
          <div className="grid grid-cols-[9rem_1fr] items-center gap-2 text-sm">
            <label htmlFor="fx-on">Enabled</label>
            <input
              id="fx-on"
              type="checkbox"
              className="h-4 w-4 justify-self-start"
              checked={kvm.effect.enabled}
              onChange={(e) => setKvm({ ...kvm, effect: { ...kvm.effect, enabled: e.target.checked } })}
            />
            <label htmlFor="fx-style">Style</label>
            <select
              id="fx-style"
              className="field"
              value={kvm.effect.style}
              onChange={(e) => setKvm({ ...kvm, effect: { ...kvm.effect, style: e.target.value as EffectStyle } })}
            >
              <option value="rift">rift (portal on the edge)</option>
              <option value="smoke">mist + light along the edge</option>
              <option value="glow">glow</option>
              <option value="particles">particles</option>
            </select>
            <label htmlFor="fx-dur">Duration (ms)</label>
            <input
              id="fx-dur"
              type="number"
              min={50}
              max={3000}
              step={50}
              className="field w-28"
              value={kvm.effect.duration_ms}
              onChange={(e) =>
                setKvm({ ...kvm, effect: { ...kvm.effect, duration_ms: Math.round(Number(e.target.value) || 0) } })
              }
            />
            <label htmlFor="fx-color">Color</label>
            <div className="flex items-center gap-2">
              <input
                id="fx-color"
                type="color"
                className="h-8 w-12 cursor-pointer rounded border border-zinc-300 bg-transparent dark:border-zinc-700"
                value={/^#[0-9a-f]{6}$/i.test(kvm.effect.color) ? kvm.effect.color : "#f2f6ff"}
                onChange={(e) => setKvm({ ...kvm, effect: { ...kvm.effect, color: e.target.value.toUpperCase() } })}
              />
              <span className="font-mono text-xs text-zinc-500">{kvm.effect.color}</span>
            </div>
            <label htmlFor="fx-peer">Server address</label>
            <input
              id="fx-peer"
              className="field font-mono text-xs"
              value={kvm.effect.peer}
              placeholder="automatic (from Deskflow)"
              onChange={(e) => setKvm({ ...kvm, effect: { ...kvm.effect, peer: e.target.value } })}
            />
          </div>
          <p className="text-xs text-zinc-500">
            The computers tell each other about crossings over UDP (port {kvm.effect.port}). Windows may ask about the
            firewall on first start – allow private networks.
          </p>
        </Section>
      )}

      <Section
        title="Streaming (Moonlight → Sunshine)"
        hint="Host = the other computer as Moonlight knows it (pair once in Moonlight). Profiles for another platform are not shown in the menu."
      >
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={streamCfg.enabled}
            onChange={(e) => setStreamCfg({ ...streamCfg, enabled: e.target.checked })}
          />
          Show Moonlight streams in the menu
        </label>
        {streams.map((s, i) => {
          const set = (patch: Partial<StreamRow>) =>
            setStreams(streams.map((x, j) => (j === i ? { ...x, ...patch } : x)));
          const here = !s.only_on || s.only_on === payload.platform;
          const saved = payload.settings.streams[i];
          return (
            <div key={i} className="space-y-2 rounded-md border border-zinc-200 p-2 dark:border-zinc-700">
              <div className="grid grid-cols-[1fr_1fr_6rem_6rem_auto] gap-2">
                <input className="field" value={s.name} placeholder="name" onChange={(e) => set({ name: e.target.value })} />
                <input className="field" value={s.host} placeholder="host (e.g. studio-mac.local)" onChange={(e) => set({ host: e.target.value })} />
                <input className="field" value={s.app} placeholder="app" onChange={(e) => set({ app: e.target.value })} />
                <select className="field" value={s.only_on} onChange={(e) => set({ only_on: e.target.value })}>
                  <option value="">everywhere</option>
                  <option value="windows">Windows</option>
                  <option value="macos">macOS</option>
                </select>
                <button className="btn" onClick={() => setStreams(streams.filter((_, j) => j !== i))} title="Remove">
                  ✕
                </button>
              </div>
              <div className="flex gap-2">
                <input
                  className="field font-mono text-xs"
                  value={s.argsText}
                  placeholder="Moonlight options"
                  onChange={(e) => set({ argsText: e.target.value })}
                />
                <button
                  className="btn shrink-0"
                  disabled={!here || !saved?.host}
                  title={!saved?.host ? "Save a host first" : "Start the saved profile"}
                  onClick={() => invoke("start_stream", { index: i })}
                >
                  Start
                </button>
              </div>
            </div>
          );
        })}
        <button
          className="btn"
          onClick={() =>
            setStreams([
              ...streams,
              { name: "", host: "", app: "Desktop", argsText: "--display-mode windowed --video-codec HEVC", only_on: payload.platform },
            ])
          }
        >
          + profile
        </button>
        <input
          className="field font-mono text-xs"
          value={streamCfg.moonlight}
          placeholder={payload.moonlight ?? "Moonlight not found – install it"}
          onChange={(e) => setStreamCfg({ ...streamCfg, moonlight: e.target.value })}
          title="Path to Moonlight (empty = default location)"
        />
        <SunshinePanel
          mac={mac}
          autostart={streamCfg.sunshine_autostart}
          onAutostart={(v) => setStreamCfg({ ...streamCfg, sunshine_autostart: v })}
        />
      </Section>

      <Section title="System">
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={autostart} onChange={(e) => setAutostart(e.target.checked)} />
          Start at login
        </label>
      </Section>

      <footer className="flex items-center justify-between gap-4 border-t border-zinc-200 pt-4 dark:border-zinc-700">
        <p className="truncate text-xs text-zinc-500" title={payload.config_path}>
          {payload.config_path}
        </p>
        <div className="flex items-center gap-3">
          {message && (
            <span className={"text-sm " + (message.ok ? "text-green-600" : "text-red-600")}>{message.text}</span>
          )}
          <button className="btn-primary" onClick={save} disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </footer>
    </main>
  );
}
