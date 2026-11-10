import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  DOWNLOAD_EVENT, api, fmtBytes,
  type DownloadNaming, type DownloadProgress, type DownloadRequest, type DownloadSummary, type FfmpegInfo,
} from "../api";
import type { Episode } from "../types";
import { useStore } from "../store";
import { Modal } from "./ui";
import { ICheck, IClose, IDownload, ISparkF } from "../icons";

const DIR_KEY = "hg.download.dir";
const PREFS_KEY = "hg.download.prefs";

type Prefs = {
  naming: DownloadNaming;
  quality: "main" | "hevc";
  concurrency: number;
  overwrite: boolean;
  remux: boolean;
};

const DEFAULT_PREFS: Prefs = {
  naming: "e_title",
  quality: "main",
  concurrency: 4,
  overwrite: false,
  remux: true,
};

function readPrefs(): Prefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return DEFAULT_PREFS;
    return { ...DEFAULT_PREFS, ...(JSON.parse(raw) as Partial<Prefs>) };
  } catch {
    return DEFAULT_PREFS;
  }
}

type Row = {
  status: "queued" | "running" | "done" | "skip" | "error";
  done: number;
  total: number;
  bytes: number;
  msg?: string;
};

const NAMING_LABEL: { value: DownloadNaming; label: string; sample: string }[] = [
  { value: "e_title", label: "剧名 - E01 - 标题", sample: "剧名 - E03 - 第三次相遇.mp4" },
  { value: "ep_only", label: "第01集", sample: "第03集.mp4" },
  { value: "season_episode", label: "剧名 S01E01", sample: "剧名 S01E03.mp4" },
];

/**
 * 批量下载弹层：范围 / 画质 / 命名 / 目录 / 并发，加逐集进度。
 *
 * 进度由 Rust 侧 `hg-download` 事件推上来（见 src-tauri/src/download.rs），
 * 这里只按 `phase` 归档，不自己轮询接口。
 */
export function DownloadSheet({ open, onClose, videoId, title, episodes }: {
  open: boolean;
  onClose: () => void;
  videoId: number;
  title: string;
  episodes: Episode[];
}) {
  const { toast } = useStore();
  const [prefs, setPrefs] = useState<Prefs>(readPrefs);
  const [scope, setScope] = useState<"all" | "range">("all");
  const [from, setFrom] = useState("1");
  const [to, setTo] = useState("1");
  const [dir, setDir] = useState(() => localStorage.getItem(DIR_KEY) || "");
  const [ffmpeg, setFfmpeg] = useState<FfmpegInfo | null>(null);
  const [running, setRunning] = useState(false);
  const [rows, setRows] = useState<Record<number, Row>>({});
  const [summary, setSummary] = useState<DownloadSummary | null>(null);
  const alive = useRef(true);

  const allEps = useMemo(() => episodes.map((e) => e.ep_num), [episodes]);
  const lo = Math.min(Number(from) || 1, Number(to) || 1);
  const hi = Math.max(Number(from) || 1, Number(to) || 1);
  const picked = useMemo(
    () => (scope === "all" ? allEps : allEps.filter((n) => n >= lo && n <= hi)),
    [scope, allEps, lo, hi],
  );

  // 打开时补默认目录 / ffmpeg 探测，并把集数区间默认成「全部」的边界。
  useEffect(() => {
    if (!open) return;
    alive.current = true;
    setSummary(null);
    setRows({});
    setScope("all");
    if (allEps.length) {
      setFrom(String(allEps[0]));
      setTo(String(allEps[allEps.length - 1]));
    }
    if (!dir) {
      api.downloadDefaultDir()
        .then((d) => { if (alive.current && d) setDir(d); })
        .catch(() => {});
    }
    api.downloadFfmpeg()
      .then((info) => { if (alive.current) setFfmpeg(info); })
      .catch(() => { if (alive.current) setFfmpeg({ available: false, path: null }); });
    return () => { alive.current = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // 进度事件：只认当前挂在弹层里的任务，关闭弹层即解绑。
  useEffect(() => {
    if (!open) return;
    let dispose: (() => void) | undefined;
    let mounted = true;
    listen<DownloadProgress>(DOWNLOAD_EVENT, ({ payload: p }) => {
      setRows((prev) => {
        const cur = prev[p.ep];
        if (!cur) return prev;
        const next: Row = { ...cur };
        if (p.phase === "ep_start") next.status = "running";
        else if (p.phase === "progress") {
          next.status = "running";
          next.done = p.seg_done ?? cur.done;
          next.total = p.seg_total ?? cur.total;
          next.bytes = p.bytes ?? cur.bytes;
        } else if (p.phase === "ep_done") {
          next.status = "done";
          next.bytes = p.bytes ?? cur.bytes;
          next.done = next.total || next.done;
        } else if (p.phase === "ep_skip") next.status = "skip";
        else if (p.phase === "ep_error") {
          next.status = "error";
          next.msg = p.message || "失败";
        } else return prev;
        return { ...prev, [p.ep]: next };
      });
    })
      .then((fn) => { if (mounted) dispose = fn; else fn(); })
      .catch(() => {});
    return () => { mounted = false; dispose?.(); };
  }, [open]);

  const savePrefs = (patch: Partial<Prefs>) => {
    const next = { ...prefs, ...patch };
    setPrefs(next);
    try { localStorage.setItem(PREFS_KEY, JSON.stringify(next)); } catch { /* 隐私模式下忽略 */ }
  };

  const saveDir = (value: string) => {
    setDir(value);
    try { localStorage.setItem(DIR_KEY, value); } catch { /* 同上 */ }
  };

  const start = async () => {
    if (running || !picked.length) return;
    const init: Record<number, Row> = {};
    for (const ep of picked) init[ep] = { status: "queued", done: 0, total: 0, bytes: 0 };
    setRows(init);
    setSummary(null);
    setRunning(true);
    const req: DownloadRequest = {
      id: videoId,
      title,
      eps: picked,
      dir: dir.trim(),
      quality: prefs.quality,
      naming: prefs.naming,
      remux: prefs.remux ? "auto" : "off",
      concurrency: prefs.concurrency,
      overwrite: prefs.overwrite,
    };
    try {
      const res = await api.downloadEpisodes(req);
      setSummary(res);
      if (res.cancelled) toast(`已取消，成功 ${res.ok} 集`, "info");
      else if (res.failed) toast(`完成：成功 ${res.ok} 集，失败 ${res.failed} 集`, "info");
      else toast(`下载完成，共 ${res.ok} 集${res.skipped ? `（跳过 ${res.skipped}）` : ""}`, "ok");
    } catch (e) {
      toast((e as Error).message || "下载失败", "info");
    } finally {
      setRunning(false);
    }
  };

  const cancel = async () => {
    try { await api.downloadCancel(); toast("正在停止…", "info"); }
    catch { toast("取消失败", "info"); }
  };

  const reveal = async (path: string) => {
    try { await api.downloadReveal(path); }
    catch { toast(`已保存到 ${path}`, "info"); }
  };

  const doneCount = Object.values(rows).filter((r) => r.status === "done").length;
  const failedCount = Object.values(rows).filter((r) => r.status === "error").length;
  const finished = !running && summary !== null;

  return (
    <Modal open={open} onClose={running ? () => toast("任务进行中，请先取消", "info") : onClose} width={520}>
      <div className="flex items-center gap-2 px-4 h-12 border-b border-s3/60">
        <IDownload size={16} className="text-primary" />
        <span className="text-[13.5px] font-bold text-on">批量下载</span>
        <span className="text-[10.5px] text-mut truncate">{title || `剧集 ${videoId}`}</span>
        <button onClick={running ? undefined : onClose} aria-label="关闭"
          className={`ml-auto w-7 h-7 rounded-full bg-s3 flex items-center justify-center text-mut ${running ? "opacity-40" : "pressable"}`}>
          <IClose size={13} />
        </button>
      </div>

      <div className="px-4 py-3 max-h-[68vh] overflow-y-auto scroll-y">
        {/* 范围 */}
        <div className="text-[10.5px] font-semibold text-dim mb-1.5">下载范围</div>
        <div className="flex items-center gap-2">
          <div className="flex bg-s3 rounded-(--r-ctl) p-1">
            {([["all", `全部 ${allEps.length} 集`], ["range", "指定区间"]] as const).map(([k, l]) => (
              <button key={k} disabled={running} onClick={() => setScope(k)}
                className={`h-7 px-3 rounded-(--r-ctl) text-[11.5px] font-semibold pressable disabled:opacity-50 ${scope === k ? "bg-s2 text-on" : "text-mut"}`}>{l}</button>
            ))}
          </div>
          {scope === "range" && (
            <div className="flex items-center gap-1.5 text-[11.5px] text-mut">
              <input value={from} onChange={(e) => setFrom(e.target.value.replace(/[^\d]/g, ""))} inputMode="numeric" disabled={running}
                className="w-14 h-7 rounded-(--r-ctl) bg-s4 px-2 text-center text-on outline-none disabled:opacity-50" />
              <span>—</span>
              <input value={to} onChange={(e) => setTo(e.target.value.replace(/[^\d]/g, ""))} inputMode="numeric" disabled={running}
                className="w-14 h-7 rounded-(--r-ctl) bg-s4 px-2 text-center text-on outline-none disabled:opacity-50" />
              <span className="text-dim">共 {picked.length} 集</span>
            </div>
          )}
        </div>

        {/* 画质 + 并发 */}
        <div className="mt-3 flex items-center gap-2 flex-wrap">
          <span className="text-[10.5px] font-semibold text-dim">画质</span>
          <div className="flex bg-s3 rounded-(--r-ctl) p-1">
            {([["main", "主线路"], ["hevc", "HEVC 优先"]] as const).map(([k, l]) => (
              <button key={k} disabled={running} onClick={() => savePrefs({ quality: k })}
                className={`h-7 px-2.5 rounded-(--r-ctl) text-[11px] font-semibold pressable disabled:opacity-50 ${prefs.quality === k ? "bg-s2 text-on" : "text-mut"}`}>{l}</button>
            ))}
          </div>
          <span className="text-[10.5px] font-semibold text-dim ml-2">分片并发</span>
          <select value={prefs.concurrency} disabled={running}
            onChange={(e) => savePrefs({ concurrency: Number(e.target.value) })}
            className="h-7 rounded-(--r-ctl) bg-s4 px-2 text-[11.5px] text-on outline-none disabled:opacity-50">
            {[1, 2, 3, 4, 6, 8].map((n) => <option key={n} value={n}>{n}</option>)}
          </select>
        </div>

        {/* 命名 */}
        <div className="mt-3 text-[10.5px] font-semibold text-dim mb-1.5">文件命名</div>
        <div className="grid grid-cols-3 gap-1.5">
          {NAMING_LABEL.map((item) => (
            <button key={item.value} disabled={running} onClick={() => savePrefs({ naming: item.value })}
              title={item.sample}
              className={`px-2 h-8 rounded-(--r-ctl) text-[11px] font-semibold pressable disabled:opacity-50 truncate ${prefs.naming === item.value ? "bg-primary text-on-primary" : "bg-s3 text-on/75"}`}>
              {item.label}
            </button>
          ))}
        </div>
        <div className="mt-1 text-[10px] text-dim truncate">
          示例：{NAMING_LABEL.find((n) => n.value === prefs.naming)?.sample}
        </div>

        {/* 目录 */}
        <div className="mt-3 text-[10.5px] font-semibold text-dim mb-1.5">保存到（按剧名自动建子目录）</div>
        <div className="flex items-center gap-1.5">
          <input value={dir} onChange={(e) => saveDir(e.target.value)} disabled={running}
            placeholder="留空则用系统「下载」目录下的 HGhub"
            className="flex-1 min-w-0 h-8 rounded-(--r-ctl) bg-s4 px-2.5 text-[11.5px] text-on placeholder-dim outline-none disabled:opacity-50" />
          <button onClick={() => api.downloadDefaultDir().then(saveDir).catch(() => {})} disabled={running}
            className="h-8 px-2.5 rounded-(--r-ctl) bg-s3 text-[11px] text-on/80 pressable disabled:opacity-50 shrink-0">默认</button>
        </div>

        {/* 选项 */}
        <div className="mt-3 flex items-center gap-4 text-[11.5px] text-on/80 flex-wrap">
          <label className="flex items-center gap-1.5 cursor-pointer">
            <input type="checkbox" checked={prefs.remux} disabled={running}
              onChange={(e) => savePrefs({ remux: e.target.checked })} style={{ accentColor: "var(--p-primary)" }} />
            转成 MP4
          </label>
          <label className="flex items-center gap-1.5 cursor-pointer">
            <input type="checkbox" checked={prefs.overwrite} disabled={running}
              onChange={(e) => savePrefs({ overwrite: e.target.checked })} style={{ accentColor: "var(--p-primary)" }} />
            覆盖已存在的文件
          </label>
        </div>
        <div className="mt-1 text-[10px] text-dim leading-relaxed">
          {ffmpeg === null
            ? "正在检测 ffmpeg…"
            : ffmpeg.available
              ? "已检测到 ffmpeg：无损重封装为 mp4（不转码，速度取决于磁盘）"
              : "未检测到 ffmpeg：将直接输出 .ts，VLC / PotPlayer / mpv 均可播放"}
        </div>

        {/* 进度 */}
        {(running || summary) && (
          <div className="mt-4">
            <div className="flex items-center gap-2 mb-1.5">
              <span className="text-[10.5px] font-semibold text-dim">
                {running ? `下载中 ${doneCount}/${picked.length}` : finished ? `完成 ${summary?.ok ?? 0} · 跳过 ${summary?.skipped ?? 0} · 失败 ${summary?.failed ?? 0}` : ""}
              </span>
              {failedCount > 0 && <span className="text-[10px] text-amber">{failedCount} 集失败</span>}
              <button onClick={() => reveal(summary?.dir || dir)} className="ml-auto text-[10.5px] text-primary pressable">打开文件夹</button>
            </div>
            <div className="rounded-(--r-md) bg-s2/70 divide-y divide-s3/50 max-h-52 overflow-y-auto scroll-y">
              {picked.map((ep) => {
                const row = rows[ep];
                const pct = row && row.total > 0 ? Math.round((row.done / row.total) * 100) : 0;
                const tone = row?.status === "error" ? "text-amber" : row?.status === "done" ? "text-mint" : "text-mut";
                return (
                  <div key={ep} className="flex items-center gap-2 px-2.5 py-1.5">
                    <span className="w-8 shrink-0 text-[11px] font-semibold text-on/80">E{String(ep).padStart(2, "0")}</span>
                    <div className="flex-1 min-w-0">
                      <div className="h-1 rounded-full bg-s4 overflow-hidden">
                        <div className="h-full rounded-full transition-all duration-300"
                          style={{
                            width: row?.status === "done" || row?.status === "skip" ? "100%" : `${pct}%`,
                            background: row?.status === "error" ? "var(--p-star)" : "var(--p-primary)",
                          }} />
                      </div>
                      {row?.status === "error" && row.msg && (
                        <div className="text-[9.5px] text-amber mt-0.5 truncate">{row.msg}</div>
                      )}
                    </div>
                    <span className={`w-14 shrink-0 text-right text-[10px] ${tone}`}>
                      {!row ? "等待" : row.status === "queued" ? "排队" :
                        row.status === "running" ? (row.total ? `${pct}%` : "取流…") :
                        row.status === "done" ? fmtBytes(row.bytes) :
                        row.status === "skip" ? "已存在" : "失败"}
                    </span>
                    {row?.status === "done" && <ICheck size={12} className="text-mint shrink-0" />}
                  </div>
                );
              })}
            </div>
            {summary?.errors?.length ? (
              <div className="mt-1.5 text-[10px] text-amber leading-relaxed max-h-16 overflow-y-auto scroll-y">
                {summary.errors.slice(0, 6).map((err, i) => <div key={i} className="truncate">{err}</div>)}
              </div>
            ) : null}
          </div>
        )}

        {!episodes.length && (
          <div className="mt-3 flex items-center gap-1.5 text-[11px] text-dim">
            <ISparkF size={12} /> 这部剧还没解析出剧集列表，先去播放一集再试
          </div>
        )}
      </div>

      <div className="flex items-center gap-2 px-4 py-3 border-t border-s3/60">
        <span className="text-[10px] text-dim truncate">
          {picked.length ? `将下载 ${picked.length} 集` : "没有可下载的剧集"}
        </span>
        {running ? (
          <button onClick={cancel}
            className="ml-auto h-8 px-4 rounded-(--r-ctl) bg-s3 text-[12px] font-semibold text-on/85 pressable">取消下载</button>
        ) : (
          <>
            {summary && <button onClick={() => reveal(summary.dir || dir)}
              className="ml-auto h-8 px-3 rounded-(--r-ctl) bg-s3 text-[12px] font-semibold text-on/85 pressable">打开目录</button>}
            <button onClick={start} disabled={!picked.length}
              className={`${summary ? "" : "ml-auto"} h-8 px-5 rounded-(--r-ctl) text-[12px] font-bold pressable disabled:opacity-40 bg-primary text-on-primary`}>
              {summary ? "再下一批" : "开始下载"}
            </button>
          </>
        )}
      </div>
    </Modal>
  );
}
