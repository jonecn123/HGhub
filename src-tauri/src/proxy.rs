//! 出网代理：跟随系统 / 直连 / 手动指定。
//!
//! # 为什么需要这个模块
//!
//! 客户端的请求**全部**由 [`crate::net`] 的 reqwest 发出，不经过 WebView2，
//! 所以它既不继承浏览器的代理设置，也不继承浏览器的 PAC：
//!
//! * 系统代理只能靠 reqwest 的 `system-proxy` feature 读注册表里的固定代理
//!   （`ProxyEnable` / `ProxyServer`），`AutoConfigURL`（PAC）不被支持；
//! * Android 没有注册表，系统「Wi-Fi 代理」同样读不到，只有 VPN/TUN 才有效。
//!
//! 在加这个模块之前，用户唯一的出路是环境变量 `HGHUB_PROXY`——普通用户不可能知道。
//! 于是「浏览器能开、客户端打不开」长期只能靠改环境变量自救。
//!
//! # 生效优先级
//!
//! 手动 > 直连 > `HGHUB_PROXY` > 系统代理。见 [`decide`]。
//!
//! * 手动：固定走用户填写的地址，环境变量不再起作用；
//! * 直连：显式 `no_proxy()`，连环境变量也不认——用户已经明确要求不走代理；
//! * 跟随系统：有 `HGHUB_PROXY` 就用它（保留原有排查手段），否则交给 reqwest 探测。
//!
//! # 安全约定
//!
//! 代理地址可能带用户名密码，**任何**回显给界面、写进 debug.log 或诊断快照的地方
//! 都必须先过 [`crate::net::redact_proxy`]；只有设置页为了回填输入框才拿完整值。

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub const MODE_SYSTEM: &str = "system";
pub const MODE_DIRECT: &str = "direct";
pub const MODE_MANUAL: &str = "manual";

/// 环境变量覆盖（排查用）：`HGHUB_PROXY=http://127.0.0.1:7897`。
pub fn env_override() -> Option<String> {
    std::env::var("HGHUB_PROXY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    System,
    Direct,
    Manual(String),
}

impl Mode {
    fn name(&self) -> &'static str {
        match self {
            Mode::System => MODE_SYSTEM,
            Mode::Direct => MODE_DIRECT,
            Mode::Manual(_) => MODE_MANUAL,
        }
    }
}

/// 最终怎么建 HTTP 客户端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effective {
    /// 不干预，交给 reqwest 的 `system-proxy` 探测（注册表 + 环境变量）。
    Inherit,
    /// 显式禁用一切代理。
    Direct,
    /// 使用指定代理。
    Url(String),
}

static MODE: Lazy<RwLock<Mode>> = Lazy::new(|| RwLock::new(Mode::System));
/// 配置落盘位置（应用数据目录/proxy.json），由 [`bind`] 在启动时绑定。
static FILE: Lazy<RwLock<Option<PathBuf>>> = Lazy::new(|| RwLock::new(None));
/// 配置变更代数。客户端缓存键带上它，改完设置旧客户端立刻失效，
/// 不用等 [`crate::net`] 里 30 秒的 TTL 自然过期。
static GEN: AtomicU64 = AtomicU64::new(0);

fn decide(mode: &Mode, env: Option<String>) -> Effective {
    match mode {
        Mode::Manual(url) => Effective::Url(url.clone()),
        Mode::Direct => Effective::Direct,
        Mode::System => match env {
            Some(url) => Effective::Url(url),
            None => Effective::Inherit,
        },
    }
}

/// 当前实际生效的代理策略。
pub fn effective() -> Effective {
    let mode = match MODE.read() {
        Ok(guard) => guard.clone(),
        Err(_) => Mode::System,
    };
    decide(&mode, env_override())
}

/// 代理是否由本模块显式接管（「跟随系统」不算，那时可能压根没有代理）。
///
/// [`crate::net`] 用它决定还要不要自己解析并固定 DNS：走代理时目标地址由代理端解析，
/// 本地再解析一次既多余，又会在 DNS 被污染 / 只能由代理解析的环境里直接失败。
pub fn is_proxied() -> bool {
    !matches!(effective(), Effective::Inherit)
}

/// 客户端缓存键的组成部分：任何配置变更都会改变它。
pub fn cache_tag() -> String {
    let name = match MODE.read() {
        Ok(guard) => guard.name(),
        Err(_) => MODE_SYSTEM,
    };
    format!("{name}:{}", GEN.load(Ordering::Relaxed))
}

/// 启动时读回上次选择（文件不存在 / 内容坏了就保持「跟随系统」）。
pub fn bind(dir: &Path) {
    let path = dir.join("proxy.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            let raw_mode = v.get("mode").and_then(Value::as_str);
            let mode = if raw_mode == Some(MODE_SYSTEM) {
                Some(Mode::System)
            } else if raw_mode == Some(MODE_DIRECT) {
                Some(Mode::Direct)
            } else if raw_mode == Some(MODE_MANUAL) {
                v.get("url")
                    .and_then(Value::as_str)
                    .and_then(|raw| normalize_proxy(raw).ok())
                    .map(Mode::Manual)
            } else {
                // 旧版本 / 手改坏的文件：不猜，保持默认
                None
            };
            if let Some(next) = mode {
                if let Ok(mut cur) = MODE.write() {
                    *cur = next;
                }
            }
        }
    }
    if let Ok(mut f) = FILE.write() {
        *f = Some(path);
    }
}

/// 规整用户填写的代理地址，失败了也不回显原始值（里面可能有密码）。
///
/// 接受省略协议的写法（`127.0.0.1:7897` 按 `http://` 处理），这是普通用户最常输入的形式。
pub fn normalize_proxy(raw: &str) -> Result<String, String> {
    const INVALID: &str = "代理地址无效：需要「协议://主机:端口」，例如 http://127.0.0.1:7897";
    let text = raw.trim();
    if text.is_empty() {
        return Err("请填写代理地址".into());
    }
    let with_scheme = if text.contains("://") {
        text.to_string()
    } else {
        format!("http://{text}")
    };
    let parsed = url::Url::parse(&with_scheme).map_err(|_| INVALID.to_string())?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks5" | "socks5h" | "socks4"
    ) {
        return Err("仅支持 http / https / socks5 代理".into());
    }
    // 代理地址只能是 origin：带路径 / 查询串的一律拒绝，避免把参数当代理配置误用。
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(INVALID.into());
    }
    let host = parsed.host().ok_or_else(|| INVALID.to_string())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "socks 代理必须写明端口，例如 socks5://127.0.0.1:1080".to_string())?;

    let mut out = String::from(parsed.scheme());
    out.push_str("://");
    if !parsed.username().is_empty() {
        out.push_str(parsed.username());
        if let Some(password) = parsed.password() {
            out.push(':');
            out.push_str(password);
        }
        out.push('@');
    }
    match host {
        url::Host::Ipv6(v6) => out.push_str(&format!("[{v6}]")),
        other => out.push_str(&other.to_string()),
    }
    out.push_str(&format!(":{port}"));
    Ok(out)
}

/// 切换代理并落盘，返回最新状态（界面直接用返回值刷新）。
pub fn set(mode: &str, url: Option<&str>) -> Result<Value, String> {
    let next = if mode == MODE_SYSTEM {
        Mode::System
    } else if mode == MODE_DIRECT {
        Mode::Direct
    } else if mode == MODE_MANUAL {
        Mode::Manual(normalize_proxy(url.unwrap_or_default())?)
    } else {
        return Err("未知的代理模式".into());
    };
    persist(&next)?;
    if let Ok(mut cur) = MODE.write() {
        *cur = next.clone();
    }
    GEN.fetch_add(1, Ordering::SeqCst);
    // 缓存里的旧客户端还带着旧代理，必须立刻清掉
    crate::net::refresh_client();
    crate::playback_log::record(
        "proxy.changed",
        json!({
            "mode": next.name(),
            // 只记录脱敏后的协议/主机/端口
            "target": match &next {
                Mode::Manual(u) => crate::net::redact_proxy(u),
                _ => String::new(),
            },
        }),
    );
    Ok(state())
}

fn persist(mode: &Mode) -> Result<(), String> {
    let target = match FILE.read() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    // 还没绑定数据目录（单元测试 / 极早期调用）：只在内存生效，不写盘
    let Some(path) = target else {
        return Ok(());
    };
    let payload = match mode {
        Mode::System => json!({ "mode": MODE_SYSTEM }),
        Mode::Direct => json!({ "mode": MODE_DIRECT }),
        Mode::Manual(url) => json!({ "mode": MODE_MANUAL, "url": url }),
    };
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, payload.to_string()).map_err(|e| format!("代理设置写入失败：{e}"))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("代理设置保存失败：{e}"));
    }
    Ok(())
}

/// 给设置页用的完整快照（含 `url` 原文，仅供回填输入框）。
pub fn state() -> Value {
    snapshot()
}

/// 给诊断 / 日志用的脱敏快照：**不含** `url` 原文，避免账号密码被复制外发。
pub fn summary() -> Value {
    let mut value = snapshot();
    if let Some(obj) = value.as_object_mut() {
        obj.remove("url");
    }
    value
}

fn snapshot() -> Value {
    let current = match MODE.read() {
        Ok(guard) => guard.clone(),
        Err(_) => Mode::System,
    };
    let (mode, url) = match current {
        Mode::Manual(u) => (MODE_MANUAL, u),
        Mode::Direct => (MODE_DIRECT, String::new()),
        Mode::System => (MODE_SYSTEM, String::new()),
    };
    let env = env_override();
    let active = match effective() {
        Effective::Inherit => MODE_SYSTEM.to_string(),
        Effective::Direct => MODE_DIRECT.to_string(),
        Effective::Url(u) => crate::net::redact_proxy(&u),
    };
    json!({
        "mode": mode,
        "url": url,
        "display": if url.is_empty() { String::new() } else { crate::net::redact_proxy(&url) },
        "env": env.as_deref().map(crate::net::redact_proxy),
        "active": active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_proxy_is_normalized() {
        assert_eq!(
            normalize_proxy("127.0.0.1:7897").unwrap(),
            "http://127.0.0.1:7897"
        );
        assert_eq!(
            normalize_proxy("  http://127.0.0.1:7897/  ").unwrap(),
            "http://127.0.0.1:7897"
        );
        assert_eq!(
            normalize_proxy("socks5://alice:pw@[::1]:1080").unwrap(),
            "socks5://alice:pw@[::1]:1080"
        );
        assert_eq!(
            normalize_proxy("https://proxy.example:8443").unwrap(),
            "https://proxy.example:8443"
        );
        // 省略端口时按协议补默认值
        assert_eq!(
            normalize_proxy("http://127.0.0.1").unwrap(),
            "http://127.0.0.1:80"
        );
    }

    #[test]
    fn manual_proxy_rejects_malformed_values() {
        for raw in [
            "",
            "   ",
            "ftp://127.0.0.1:21",
            "http://127.0.0.1:7897/path",
            "http://127.0.0.1:7897/?secret=1",
            "socks5://127.0.0.1",
            "not a proxy",
        ] {
            assert!(normalize_proxy(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn errors_never_echo_the_raw_value() {
        // 带密码但格式错误的输入，报错里不能出现密码
        let err = normalize_proxy("http://alice:s3cret@host:7897/path").unwrap_err();
        assert!(!err.contains("s3cret"), "{err}");
        assert!(!err.contains("alice"), "{err}");
    }

    #[test]
    fn precedence_is_manual_then_direct_then_env_then_system() {
        let env = Some("http://127.0.0.1:7890".to_string());
        assert_eq!(
            decide(&Mode::Manual("http://127.0.0.1:7897".into()), env.clone()),
            Effective::Url("http://127.0.0.1:7897".into())
        );
        // 直连模式下环境变量也不生效
        assert_eq!(decide(&Mode::Direct, env.clone()), Effective::Direct);
        assert_eq!(
            decide(&Mode::System, env),
            Effective::Url("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(decide(&Mode::System, None), Effective::Inherit);
    }
}
