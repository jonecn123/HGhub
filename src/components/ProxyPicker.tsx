import { useEffect, useState } from "react";
import { api, isMobile, type ProxyMode, type ProxyState } from "../api";
import { useStore } from "../store";
import { Modal, Sheet } from "./ui";
import { ICheck, IClose, ICompass, IRefresh } from "../icons";

/* 三种模式：说明里点明各自的能力边界，避免用户配了 PAC 却以为生效 */
const MODES: { key: ProxyMode; name: string; desc: string }[] = [
  {
    key: "system",
    name: "跟随系统",
    desc: "读 Windows「Internet 选项」里的固定代理。不支持 PAC 自动配置脚本（.pac），也不支持「按域名分流」。",
  },
  {
    key: "direct",
    name: "直连（不使用代理）",
    desc: "忽略系统代理与 HGHUB_PROXY 环境变量，所有请求直连。代理软件本身接管了流量时选这个。",
  },
  {
    key: "manual",
    name: "手动指定",
    desc: "固定走一个代理，适合 Clash / V2Ray 等本地客户端。填错会直接连不上，改完可点下面的「检测」。",
  },
];

/* 常见本地代理端口，点一下填进输入框，省得用户去翻客户端配置 */
const PRESETS = [
  { label: "Clash 7890", url: "http://127.0.0.1:7890" },
  { label: "Clash Verge 7897", url: "http://127.0.0.1:7897" },
  { label: "v2rayN 10809", url: "http://127.0.0.1:10809" },
  { label: "SOCKS5 1080", url: "socks5://127.0.0.1:1080" },
];

/* 代理设置：桌面居中弹窗，移动端底部抽屉（与 LinePicker 保持一致） */
export default function ProxyPicker({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { toast } = useStore();
  const [state, setState] = useState<ProxyState | null>(null);
  const [mode, setMode] = useState<ProxyMode>("system");
  const [url, setUrl] = useState("");
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [inlineError, setInlineError] = useState("");

  useEffect(() => {
    if (!open) return;
    setInlineError("");
    api.proxyState()
      .then((s) => {
        setState(s);
        setMode(s.mode);
        setUrl(s.url || "");
      })
      .catch((e) => toast((e as Error).message || "读取代理设置失败", "info"));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const save = async () => {
    if (saving) return;
    const manual = mode === "manual";
    if (manual && !url.trim()) {
      setInlineError("请填写代理地址，例如 http://127.0.0.1:7897");
      return;
    }
    setInlineError("");
    setSaving(true);
    try {
      const next = await api.proxySet(mode, manual ? url.trim() : "");
      setState(next);
      toast(
        next.mode === "manual" ? `已走代理 ${next.display}` : next.mode === "direct" ? "已切换为直连" : "已改为跟随系统代理",
        "ok",
      );
      onClose();
    } catch (e) {
      // 后端的报错已经脱敏（不含账号密码），可以原样展示
      setInlineError((e as Error).message || "保存失败");
    } finally {
      setSaving(false);
    }
  };

  /* 检测走的是**已保存**的配置，所以提示里要说清楚 */
  const test = async () => {
    if (testing) return;
    setTesting(true);
    try {
      const diag = (await api.netDiag()) as {
        http?: { status?: number; ms?: number; error?: string };
        proxy?: { active?: string };
      };
      const http = diag.http || {};
      if (http.error) toast(`连不上上游：${http.error}`, "info");
      else toast(`网络正常 · HTTP ${http.status ?? "-"} · ${http.ms ?? "-"}ms`, "ok");
    } catch (e) {
      toast((e as Error).message || "检测失败", "info");
    } finally {
      setTesting(false);
    }
  };

  const body = (
    <div className="flex flex-col min-h-0" style={{ maxHeight: isMobile ? "78vh" : "min(76vh, 640px)" }}>
      <div className="shrink-0 flex items-center gap-2 px-4 pt-3 pb-2">
        <span className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-primary">
          <ICompass size={16} />
        </span>
        <div className="flex-1 min-w-0">
          <div className="text-[14px] font-bold text-on leading-tight">网络代理</div>
          <div className="text-[10.5px] text-mut mt-0.5 truncate">
            当前生效：{state ? (state.active === "system" ? "系统代理" : state.active === "direct" ? "直连" : state.active) : "读取中…"}
          </div>
        </div>
        <button onClick={onClose} aria-label="关闭" className="w-8 h-8 rounded-full bg-s3/80 flex items-center justify-center text-mut pressable">
          <IClose size={16} />
        </button>
      </div>

      <div className="flex-1 scroll-y px-3 pb-4">
        <div className="rounded-(--r-md) bg-s2 divide-y divide-s3 overflow-hidden">
          {MODES.map((item) => {
            const on = mode === item.key;
            return (
              <button
                key={item.key}
                onClick={() => { setMode(item.key); setInlineError(""); }}
                disabled={saving}
                className={`w-full text-left px-3.5 py-2.5 pressable disabled:opacity-60 ${on ? "bg-s3/70" : ""}`}
              >
                <div className="flex items-center gap-1.5 text-[12.5px] text-on">
                  <span>{item.name}</span>
                  {on && <ICheck size={13} className="text-mint shrink-0" />}
                </div>
                <div className="text-[10.5px] text-mut leading-relaxed mt-0.5">{item.desc}</div>
              </button>
            );
          })}
        </div>

        {mode === "manual" && (
          <>
            <div className="text-[10.5px] font-semibold text-dim tracking-wide px-1 mt-4 mb-1.5">代理地址</div>
            <input
              value={url}
              onChange={(e) => { setUrl(e.target.value); setInlineError(""); }}
              onKeyDown={(e) => { if (e.key === "Enter") void save(); }}
              disabled={saving}
              spellCheck={false}
              autoComplete="off"
              placeholder="http://127.0.0.1:7897"
              className="w-full h-10 px-3.5 rounded-(--r-sm) bg-s4 text-[13px] text-on placeholder-mut outline-none focus:ring-2 focus:ring-[var(--p-primary)]"
            />
            <div className="flex flex-wrap gap-1.5 mt-2">
              {PRESETS.map((p) => (
                <button
                  key={p.url}
                  onClick={() => { setUrl(p.url); setInlineError(""); }}
                  disabled={saving}
                  className="h-7 px-2.5 rounded-(--r-sm) bg-s3 text-[10.5px] text-on/80 pressable disabled:opacity-50"
                >
                  {p.label}
                </button>
              ))}
            </div>
            <div className="text-[10px] text-dim leading-relaxed px-1 mt-2">
              支持 http:// 、https:// 、socks5:// 。省略协议时按 http 处理；账号密码直接写在地址里
              （<span className="text-mut">http://用户名:密码@主机:端口</span>），只保存在本机，日志与诊断里始终隐藏。
            </div>
          </>
        )}

        {mode === "system" && state?.env && (
          <div className="mt-3 rounded-(--r-sm) bg-s3/60 p-3 text-[10.5px] text-mut leading-relaxed">
            检测到环境变量 <span className="text-on">HGHUB_PROXY={state.env}</span>，它优先于系统代理生效。
            想改用系统代理，请清掉这个环境变量后重启客户端。
          </div>
        )}

        {inlineError && (
          <div role="alert" className="mt-3 text-[11px] text-danger leading-relaxed px-1">{inlineError}</div>
        )}

        <div className="flex gap-2 mt-4">
          <button
            onClick={test}
            disabled={testing || saving}
            className="flex-1 h-9 rounded-(--r-ctl) bg-s3 text-[12.5px] text-on/80 pressable disabled:opacity-60 inline-flex items-center justify-center gap-1.5"
          >
            <IRefresh size={13} />
            {testing ? "检测中…" : "检测连通性"}
          </button>
          <button
            onClick={save}
            disabled={saving}
            className="flex-1 h-9 rounded-(--r-ctl) bg-primary text-[12.5px] font-semibold text-on-primary pressable disabled:opacity-60"
          >
            {saving ? "保存中…" : "保存"}
          </button>
        </div>

        <div className="text-[10px] text-dim leading-relaxed px-1 mt-3">
          保存后立即生效，不用重启；已建立的请求会自动换用新的客户端。检测使用的是已保存的配置，
          改完请先保存再检测。安卓端系统「Wi-Fi 代理」读不到，只有手动指定或 VPN 模式才有效。
        </div>
      </div>
    </div>
  );

  return isMobile
    ? <Sheet open={open} onClose={onClose}>{body}</Sheet>
    : <Modal open={open} onClose={onClose} width={460}>{body}</Modal>;
}
