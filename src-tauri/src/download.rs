//! 剧集批量下载：拉 m3u8 → 并发取分片 → AES-128 解密 → 按序拼接 → 按模板命名落盘。
//!
//! # 为什么单独一个模块
//!
//! `media.rs` 的 `/hls`、`/seg` 是给 WebView 播放器用的**代理**：它把 m3u8 里的地址改写成
//! 本地协议，分片只在内存里过一遍（`SEG_CACHE_MAX` 只有 4MB / 36 片），既不留存也不保证顺序。
//! 下载需要的正好相反：原始地址、按序、落盘、可中途取消，所以这里独立实现，
//! 但**出网一律走 [`crate::net::fetch_bytes`]**，继承 `network_policy` 的内网拦截、
//! 明文降级拒绝、逐跳重定向复检与重试，不另外开客户端。
//!
//! # 落盘约定
//!
//! * 目标目录：`<用户选择的目录>/<剧名>/`，剧名与单集标题都过 [`sanitize_part`]。
//! * 分片先落到目标目录下的临时子目录 `.hgdl-<id>-<ep>/`，按 `seg-%06d.bin` 命名，
//!   全部下完才按文件名排序拼接，最后整段 rename 到最终文件——中途失败不会留下
//!   一个能播放一半的成品。
//! * fMP4（带 `#EXT-X-MAP`）拼出来本身就是 mp4；TS 分片默认在检测到 ffmpeg 时
//!   无损重封装成 mp4，没装就保留 `.ts`（VLC / PotPlayer / mpv 都能放）。

use crate::api::{play_info, Api};
use crate::net::{base, edge, fetch_bytes};
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

/// 前端监听的事件名（`src/api.ts` 里的 `DOWNLOAD_EVENT` 必须一致）。
pub const EVENT: &str = "hg-download";

/// 同一时间只允许一个批量任务：同时跑两批会抢带宽、也会让「取消」的语义变得含糊。
static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// 任务结束（正常 / 报错 / 提前 return）时复位全局状态。
struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        CANCEL.store(false, Ordering::SeqCst);
    }
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/* ───────────────────────── 请求 / 结果 ───────────────────────── */

fn default_concurrency() -> u8 {
    4
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Main,
    Hevc,
}

impl Default for Quality {
    fn default() -> Self {
        Quality::Main
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Naming {
    /// `剧名 - E03 - 集标题`
    ETitle,
    /// `第03集`
    EpOnly,
    /// `剧名 S01E03`（Emby / Jellyfin 刮削认得）
    SeasonEpisode,
}

impl Default for Naming {
    fn default() -> Self {
        Naming::ETitle
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemuxMode {
    /// 有 ffmpeg 就转 mp4，没有就保留原始容器
    Auto,
    /// 一律保留原始容器
    Off,
    /// 必须转 mp4，缺 ffmpeg 直接报错
    Require,
}

impl Default for RemuxMode {
    fn default() -> Self {
        RemuxMode::Auto
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadRequest {
    pub id: i64,
    /// 前端已知的剧名；为空时回落到详情接口返回的 title
    #[serde(default)]
    pub title: String,
    pub eps: Vec<u32>,
    /// 目标根目录；为空时用系统「下载」目录下的 HGhub
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub naming: Naming,
    #[serde(default)]
    pub remux: RemuxMode,
    #[serde(default = "default_concurrency")]
    pub concurrency: u8,
    #[serde(default)]
    pub overwrite: bool,
}

/// 逐集 / 逐分片进度，前端只按 `phase` 分支。
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub phase: &'static str,
    pub ep: u32,
    pub ep_title: Option<String>,
    pub done_eps: usize,
    pub total_eps: usize,
    pub seg_done: Option<usize>,
    pub seg_total: Option<usize>,
    /// 该集已落盘的累计字节数
    pub bytes: Option<u64>,
    pub file: Option<String>,
    pub message: Option<String>,
}

impl Progress {
    fn new(phase: &'static str, ep: u32, done_eps: usize, total_eps: usize) -> Self {
        Self {
            phase,
            ep,
            ep_title: None,
            done_eps,
            total_eps,
            seg_done: None,
            seg_total: None,
            bytes: None,
            file: None,
            message: None,
        }
    }

    fn title(mut self, title: &str) -> Self {
        if !title.trim().is_empty() {
            self.ep_title = Some(title.to_string());
        }
        self
    }

    fn message(mut self, msg: impl Into<String>) -> Self {
        self.message = Some(msg.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedFile {
    pub ep: u32,
    pub path: String,
    pub bytes: u64,
    pub container: String,
    /// 例如「转 mp4 失败，已保留 .ts」
    pub note: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub dir: String,
    pub files: Vec<SavedFile>,
    pub errors: Vec<String>,
}

enum Outcome {
    Saved(SavedFile),
    Skipped(PathBuf),
}

/* ───────────────────────── 命令 ───────────────────────── */

#[tauri::command]
pub async fn episode_download(
    app: AppHandle,
    state: State<'_, Api>,
    req: DownloadRequest,
) -> Result<Summary, String> {
    if req.id <= 0 {
        return Err("剧集 ID 无效".into());
    }
    if req.eps.is_empty() {
        return Err("没有选中任何剧集".into());
    }
    if req.concurrency == 0 || req.concurrency > 12 {
        return Err("分片并发需在 1-12 之间".into());
    }
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("已有下载任务在进行，请等它结束或先取消".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let _guard = RunGuard;

    let origin = base();
    let token = state.session.token(&origin);

    // 详情只用来补剧名与每集标题；拉不到不算失败，命名会自动退化成不带标题。
    let detail = edge(
        &origin,
        "GET",
        &format!("/api/videos/{}", req.id),
        None,
        token.as_deref(),
    )
    .await
    .ok();

    let show = {
        let from_req = req.title.trim();
        let from_api = detail
            .as_ref()
            .and_then(|d| d.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !from_req.is_empty() {
            from_req.to_string()
        } else if !from_api.is_empty() {
            from_api.to_string()
        } else {
            format!("视频{}", req.id)
        }
    };

    let mut ep_titles: HashMap<u32, String> = HashMap::new();
    if let Some(list) = detail
        .as_ref()
        .and_then(|d| d.get("episodes"))
        .and_then(Value::as_array)
    {
        for item in list {
            let num = item.get("ep_num").and_then(Value::as_u64).unwrap_or(0) as u32;
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if num > 0 {
                ep_titles.insert(num, title);
            }
        }
    }

    let root = {
        let raw = req.dir.trim();
        let base_dir = if raw.is_empty() {
            app.path()
                .download_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("HGhub")
        } else {
            PathBuf::from(raw)
        };
        base_dir.join(sanitize_part(&show))
    };
    std::fs::create_dir_all(&root).map_err(|e| format!("创建目录失败：{e}"))?;

    let ffmpeg = ffmpeg_path();
    if req.remux == RemuxMode::Require && ffmpeg.is_none() {
        return Err("未检测到 ffmpeg，无法输出 MP4".into());
    }
    let remux = match req.remux {
        RemuxMode::Off => false,
        RemuxMode::Auto => ffmpeg.is_some(),
        RemuxMode::Require => true,
    };

    let total = req.eps.len();
    let ctx = Ctx {
        app: &app,
        api: state.inner(),
        req: &req,
        root: &root,
        show: &show,
        remux,
    };
    let _ = app.emit(
        EVENT,
        Progress::new("start", 0, 0, total).message(root.to_string_lossy().to_string()),
    );

    let mut summary = Summary {
        dir: root.to_string_lossy().to_string(),
        ..Default::default()
    };
    let mut done_eps = 0usize;

    for ep in req.eps.iter().copied() {
        if cancelled() {
            summary.cancelled = true;
            break;
        }
        let ep_title = ep_titles.get(&ep).cloned().unwrap_or_default();
        let _ = app.emit(
            EVENT,
            Progress::new("ep_start", ep, done_eps, total).title(&ep_title),
        );
        match download_one(&ctx, ep, &ep_title, done_eps).await {
            Ok(Outcome::Saved(file)) => {
                let _ = app.emit(
                    EVENT,
                    Progress {
                        bytes: Some(file.bytes),
                        file: Some(file.path.clone()),
                        ..Progress::new("ep_done", ep, done_eps + 1, total).title(&ep_title)
                    },
                );
                summary.ok += 1;
                summary.files.push(file);
            }
            Ok(Outcome::Skipped(path)) => {
                let _ = app.emit(
                    EVENT,
                    Progress {
                        file: Some(path.to_string_lossy().to_string()),
                        ..Progress::new("ep_skip", ep, done_eps + 1, total).title(&ep_title)
                    },
                );
                summary.skipped += 1;
            }
            Err(err) => {
                if cancelled() {
                    summary.cancelled = true;
                    break;
                }
                let _ = app.emit(
                    EVENT,
                    Progress::new("ep_error", ep, done_eps, total)
                        .title(&ep_title)
                        .message(err.clone()),
                );
                summary.failed += 1;
                summary.errors.push(format!("第{ep}集：{err}"));
            }
        }
        done_eps += 1;
    }

    let _ = app.emit(
        EVENT,
        Progress::new(
            if summary.cancelled { "cancelled" } else { "done" },
            0,
            done_eps,
            total,
        ),
    );
    Ok(summary)
}

/// 请求取消：返回「当时是否真的有任务在跑」，前端据此决定提示语。
#[tauri::command]
pub async fn download_cancel() -> Result<bool, String> {
    let was_running = RUNNING.load(Ordering::SeqCst);
    CANCEL.store(true, Ordering::SeqCst);
    Ok(was_running)
}

/// 探测 ffmpeg（`HGHUB_FFMPEG` 环境变量优先，其次 PATH）。
#[tauri::command]
pub async fn download_ffmpeg() -> Result<Value, String> {
    let path = ffmpeg_path();
    Ok(json!({ "available": path.is_some(), "path": path }))
}

/// 默认保存目录：系统「下载」目录下的 HGhub。
#[tauri::command]
pub async fn download_default_dir(app: AppHandle) -> Result<String, String> {
    let dir = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir())
        .map_err(|e| e.to_string())?;
    Ok(dir.join("HGhub").to_string_lossy().to_string())
}

/// 在系统文件管理器里定位下载目录。
#[tauri::command]
pub async fn download_reveal(app: AppHandle, path: String) -> Result<(), String> {
    let target = PathBuf::from(path.trim());
    if !target.exists() {
        return Err("目录不存在".into());
    }
    if !target.is_dir() {
        return Err("目标不是目录".into());
    }
    app.opener()
        .reveal_item_in_dir(&target)
        .map_err(|e| e.to_string())
}

/* ───────────────────────── 单集流程 ───────────────────────── */

struct Ctx<'a> {
    app: &'a AppHandle,
    api: &'a Api,
    req: &'a DownloadRequest,
    root: &'a Path,
    show: &'a str,
    remux: bool,
}

async fn download_one(
    ctx: &Ctx<'_>,
    ep: u32,
    ep_title: &str,
    done_eps: usize,
) -> Result<Outcome, String> {
    let info = play_info(ctx.api, ctx.req.id, ep).await?;
    let url = pick_stream(&info, ctx.req.quality)?;

    let playlist = resolve_playlist(&url, 0).await?;
    if playlist.segments.is_empty() {
        return Err("播放列表里没有分片".into());
    }

    // 密钥整集共用，先取一次，避免每个分片各拉一遍。
    let mut key_bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    for seg in &playlist.segments {
        let Some(key) = &seg.key else { continue };
        if key.method != "AES-128" {
            continue;
        }
        if key.uri.is_empty() {
            return Err("加密分片缺少密钥地址".into());
        }
        if !key_bytes.contains_key(&key.uri) {
            let (_, bytes) = fetch_bytes(&key.uri).await?;
            if bytes.len() != 16 {
                return Err(format!("密钥长度异常：{} 字节", bytes.len()));
            }
            key_bytes.insert(key.uri.clone(), Arc::new(bytes));
        }
    }
    let key_bytes = Arc::new(key_bytes);

    let container_is_mp4 = playlist.map.is_some()
        || playlist
            .segments
            .last()
            .map(|s| {
                let path = s.uri.split('?').next().unwrap_or("");
                path.ends_with(".m4s") || path.ends_with(".mp4") || path.ends_with(".m4a")
            })
            .unwrap_or(false);

    let stem = file_stem(ctx.req.naming, ctx.show, ep, ep_title);
    let want_mp4 = container_is_mp4 || ctx.remux;
    let ext = if want_mp4 { "mp4" } else { "ts" };

    if !ctx.req.overwrite {
        if let Some(existing) = existing_file(ctx.root, &stem) {
            return Ok(Outcome::Skipped(existing));
        }
    }

    let tmp = ctx.root.join(format!(".hgdl-{}-{ep}", ctx.req.id));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败：{e}"))?;

    let seg_total = playlist.segments.len();
    let mut last_emit = Instant::now();
    let mut bytes_so_far = 0u64;

    // fMP4 的初始化段必须先于所有分片拼进文件。
    if let Some(map_uri) = &playlist.map {
        let (_, bytes) = fetch_bytes(map_uri).await?;
        let path = tmp.join("seg-000000.bin");
        std::fs::write(&path, &bytes).map_err(|e| format!("写入分片失败：{e}"))?;
        bytes_so_far += bytes.len() as u64;
    }

    let conc = ctx.req.concurrency as usize;
    let mut set: tokio::task::JoinSet<(usize, Result<Vec<u8>, String>)> =
        tokio::task::JoinSet::new();
    let mut cursor = 0usize;
    let mut finished = 0usize;
    let mut failure: Option<String> = None;

    while cursor < seg_total && set.len() < conc {
        spawn_segment(&mut set, &playlist.segments[cursor], cursor + 1, &key_bytes);
        cursor += 1;
    }

    while !set.is_empty() {
        let Some(joined) = set.join_next().await else {
            break;
        };
        match joined {
            Ok((idx, Ok(bytes))) => {
                let path = tmp.join(format!("seg-{idx:06}.bin"));
                if let Err(e) = std::fs::write(&path, &bytes) {
                    failure = Some(format!("写入分片失败：{e}"));
                    break;
                }
                finished += 1;
                bytes_so_far += bytes.len() as u64;
                // 一集几百片，逐片上报会把 IPC 打满，限流到 ~220ms 一次。
                if last_emit.elapsed() >= Duration::from_millis(220) {
                    last_emit = Instant::now();
                    let _ = ctx.app.emit(
                        EVENT,
                        Progress {
                            seg_done: Some(finished),
                            seg_total: Some(seg_total),
                            bytes: Some(bytes_so_far),
                            ..Progress::new("progress", ep, done_eps, ctx.req.eps.len())
                                .title(ep_title)
                        },
                    );
                }
                if cancelled() {
                    failure = Some("已取消".into());
                    break;
                }
                if cursor < seg_total {
                    spawn_segment(&mut set, &playlist.segments[cursor], cursor + 1, &key_bytes);
                    cursor += 1;
                }
            }
            Ok((_, Err(e))) => {
                failure = Some(e);
                break;
            }
            Err(e) => {
                failure = Some(format!("分片任务异常：{e}"));
                break;
            }
        }
    }
    drop(set); // JoinSet 析构时 abort 掉还没跑完的分片

    if let Some(err) = failure {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err);
    }

    let joined_path = tmp.join("joined.bin");
    let tmp_clone = tmp.clone();
    let joined_clone = joined_path.clone();
    let total_bytes = tokio::task::spawn_blocking(move || concat_segments(&tmp_clone, &joined_clone))
        .await
        .map_err(|e| format!("拼接任务异常：{e}"))??;
    if total_bytes == 0 {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("分片总长度为 0，可能被源站拦截".into());
    }

    let mut note: Option<String> = None;
    let target = ctx.root.join(format!("{stem}.{ext}"));
    let final_path = if container_is_mp4 || !ctx.remux {
        // 已经是最终容器，直接改名落位（同分区 rename 是原子的）。
        std::fs::rename(&joined_path, &target)
            .or_else(|_| std::fs::copy(&joined_path, &target).map(|_| ()))
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&tmp);
                format!("保存文件失败：{e}")
            })?;
        target
    } else {
        let from = joined_path.clone();
        let to = target.clone();
        let remuxed =
            tokio::task::spawn_blocking(move || remux_to_mp4(&from, &to)).await;
        match remuxed {
            Ok(Ok(())) => target,
            _ => {
                // ffmpeg 不可用或封装失败时，宁可给用户一个能播的 .ts，也不要丢结果。
                let fallback = ctx.root.join(format!("{stem}.ts"));
                std::fs::rename(&joined_path, &fallback)
                    .or_else(|_| std::fs::copy(&joined_path, &fallback).map(|_| ()))
                    .map_err(|e| {
                        let _ = std::fs::remove_dir_all(&tmp);
                        format!("保存文件失败：{e}")
                    })?;
                note = Some("未能封装为 mp4（ffmpeg 不可用或封装失败），已保留 .ts".into());
                fallback
            }
        }
    };

    let _ = std::fs::remove_dir_all(&tmp);

    Ok(Outcome::Saved(SavedFile {
        ep,
        path: final_path.to_string_lossy().to_string(),
        bytes: total_bytes,
        container: final_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase(),
        note,
    }))
}

fn spawn_segment(
    set: &mut tokio::task::JoinSet<(usize, Result<Vec<u8>, String>)>,
    seg: &Seg,
    idx: usize,
    keys: &Arc<HashMap<String, Arc<Vec<u8>>>>,
) {
    let uri = seg.uri.clone();
    let key = seg.key.clone();
    let seq = seg.seq;
    let keys = keys.clone();
    set.spawn(async move {
        let result = fetch_and_decrypt(uri, key, seq, keys).await;
        (idx, result)
    });
}

async fn fetch_and_decrypt(
    uri: String,
    key: Option<Key>,
    seq: u64,
    keys: Arc<HashMap<String, Arc<Vec<u8>>>>,
) -> Result<Vec<u8>, String> {
    let (_, bytes) = fetch_bytes(&uri).await?;
    let Some(key) = key else { return Ok(bytes) };
    if key.method != "AES-128" {
        return Ok(bytes);
    }
    let raw = keys
        .get(&key.uri)
        .ok_or_else(|| "密钥未预取到".to_string())?;
    let key_arr: [u8; 16] = raw
        .as_slice()
        .try_into()
        .map_err(|_| "密钥长度异常".to_string())?;
    let iv = key.iv.unwrap_or_else(|| iv_from_seq(seq));
    aes128_cbc_decrypt(&key_arr, &iv, &bytes)
}

/// 从播放信息里挑一路流：HEVC 拿不到就退回主线路，反之亦然。
fn pick_stream(info: &Value, quality: Quality) -> Result<String, String> {
    let field = |name: &str| {
        info.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let main = field("video_url");
    let hevc = field("video_url_hevc");
    let chosen = match quality {
        Quality::Hevc => hevc.or(main),
        Quality::Main => main.or(hevc),
    };
    chosen.ok_or_else(|| "没有拿到播放地址（该集可能需要登录或会员）".to_string())
}

/* ───────────────────────── HLS 解析 ───────────────────────── */

#[derive(Debug, Clone, Default)]
struct Key {
    method: String,
    uri: String,
    iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone)]
struct Seg {
    uri: String,
    seq: u64,
    key: Option<Key>,
}

#[derive(Debug, Default)]
struct Playlist {
    /// 主列表里的清晰度：(带宽, 地址)
    variants: Vec<(u64, String)>,
    segments: Vec<Seg>,
    /// fMP4 的初始化段
    map: Option<String>,
}

/// 主列表就下钻一层拿到真实分片；下钻过深说明地址异常，直接报错而不是死循环。
async fn resolve_playlist(url: &str, depth: u8) -> Result<Playlist, String> {
    let (_, bytes) = fetch_bytes(url).await?;
    let text = String::from_utf8_lossy(&bytes).replace('\u{feff}', "");
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("播放地址返回的不是 HLS 列表".into());
    }
    let playlist = parse_playlist(&text, url)?;
    if playlist.variants.is_empty() {
        return Ok(playlist);
    }
    if depth >= 2 {
        return Err("播放列表嵌套层数异常".into());
    }
    let best = playlist
        .variants
        .iter()
        .max_by_key(|(bandwidth, _)| *bandwidth)
        .map(|(_, uri)| uri.clone())
        .ok_or_else(|| "主播放列表没有可用清晰度".to_string())?;
    Box::pin(resolve_playlist(&best, depth + 1)).await
}

fn parse_playlist(text: &str, base_url: &str) -> Result<Playlist, String> {
    let mut playlist = Playlist::default();
    let mut current_key: Option<Key> = None;
    let mut seq = 0u64;
    let mut pending_variant: Option<u64> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = rest.trim().parse::<u64>().unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attr_value(rest, "METHOD").unwrap_or_default().to_uppercase();
            if method == "NONE" {
                current_key = None;
            } else if method == "AES-128" {
                let uri = attr_value(rest, "URI")
                    .map(|u| absolutize(base_url, &u))
                    .unwrap_or_default();
                current_key = Some(Key {
                    method,
                    uri,
                    iv: attr_value(rest, "IV").and_then(|v| parse_iv(&v)),
                });
            } else {
                return Err(format!("不支持的加密方式 {method}（仅支持 AES-128 / 明文）"));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attr_value(rest, "URI").ok_or("EXT-X-MAP 缺少 URI")?;
            playlist.map = Some(absolutize(base_url, &uri));
            continue;
        }
        if line.starts_with("#EXT-X-BYTERANGE:") {
            return Err("该播放列表按字节范围切分片，暂不支持".into());
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            // 必须把标签名去掉再取属性，否则第一项会变成 `#EXT-X-STREAM-INF:BANDWIDTH`。
            pending_variant = Some(
                attr_u64(rest, "BANDWIDTH")
                    .or_else(|| attr_u64(rest, "AVERAGE-BANDWIDTH"))
                    .unwrap_or(0),
            );
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        let absolute = absolutize(base_url, line);
        match pending_variant.take() {
            Some(bandwidth) => playlist.variants.push((bandwidth, absolute)),
            None => {
                playlist.segments.push(Seg {
                    uri: absolute,
                    seq,
                    key: current_key.clone(),
                });
                seq += 1;
            }
        }
    }

    Ok(playlist)
}

/// 属性串里引号内可以含逗号（`NAME="a,b"`），不能直接 split(',')。
fn split_attributes(attrs: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut buf = String::with_capacity(attrs.len());
    let mut quoted = false;
    for ch in attrs.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                buf.push(ch);
            }
            ',' if !quoted => parts.push(std::mem::take(&mut buf)),
            _ => buf.push(ch),
        }
    }
    parts.push(buf);
    parts
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    for part in split_attributes(attrs) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn attr_u64(line: &str, name: &str) -> Option<u64> {
    attr_value(line, name).and_then(|v| v.trim().parse::<u64>().ok())
}

fn parse_iv(raw: &str) -> Option<[u8; 16]> {
    let hex = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.is_empty() || hex.len() > 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{hex:0>32}");
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// 未显式给 IV 时，IV 就是分片序号的大端 128 位表示。
fn iv_from_seq(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..16].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn absolutize(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    url::Url::parse(base)
        .ok()
        .and_then(|b| b.join(target).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| target.to_string())
}

/* ───────────────────────── AES-128-CBC ───────────────────────── */

fn aes128_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.len() % 16 != 0 {
        return Err(format!("密文长度 {} 不是 16 的整数倍", data.len()));
    }
    let cipher = Aes128::new_from_slice(key).map_err(|_| "AES 密钥初始化失败".to_string())?;
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let cipher_block = block;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = cipher_block;
    }
    Ok(strip_pkcs7(out))
}

/// 只去掉合法的 PKCS#7 填充；长度不整除的明文原样返回，避免误伤。
fn strip_pkcs7(mut buf: Vec<u8>) -> Vec<u8> {
    let pad = match buf.last() {
        Some(&p) => p as usize,
        None => return buf,
    };
    if pad == 0 || pad > 16 || buf.len() < pad {
        return buf;
    }
    if buf[buf.len() - pad..].iter().all(|b| *b as usize == pad) {
        buf.truncate(buf.len() - pad);
    }
    buf
}

/* ───────────────────────── 落盘 / 命名 ───────────────────────── */

fn concat_segments(tmp: &Path, out: &Path) -> Result<u64, String> {
    let mut parts: Vec<PathBuf> = std::fs::read_dir(tmp)
        .map_err(|e| format!("读取临时目录失败：{e}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg-") && n.ends_with(".bin"))
        })
        .collect();
    // 文件名是零填充的，字典序就是播放顺序。
    parts.sort();

    let mut file = std::fs::File::create(out).map_err(|e| format!("创建文件失败：{e}"))?;
    let mut total = 0u64;
    for part in parts {
        let bytes = std::fs::read(&part).map_err(|e| format!("读取分片失败：{e}"))?;
        file.write_all(&bytes)
            .map_err(|e| format!("写入文件失败：{e}"))?;
        total += bytes.len() as u64;
    }
    file.flush().ok();
    Ok(total)
}

fn sanitize_part(raw: &str) -> String {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {}
            c => out.push(c),
        }
    }
    let mut limited: String = out.chars().take(80).collect();
    // Windows 不允许文件名以空格或点结尾。
    while limited.ends_with(' ') || limited.ends_with('.') {
        limited.pop();
    }
    let name = limited.trim_start();
    if name.is_empty() {
        return "未命名".to_string();
    }
    if RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r)) {
        return format!("{name}_");
    }
    name.to_string()
}

fn file_stem(naming: Naming, show: &str, ep: u32, ep_title: &str) -> String {
    let show = sanitize_part(show);
    let title = ep_title.trim();
    match naming {
        Naming::ETitle => {
            if title.is_empty() {
                sanitize_part(&format!("{show} - E{ep:02}"))
            } else {
                sanitize_part(&format!("{show} - E{ep:02} - {}", sanitize_part(title)))
            }
        }
        Naming::EpOnly => format!("第{ep:02}集"),
        Naming::SeasonEpisode => sanitize_part(&format!("{show} S01E{ep:02}")),
    }
}

fn existing_file(dir: &Path, stem: &str) -> Option<PathBuf> {
    for ext in ["mp4", "ts"] {
        let path = dir.join(format!("{stem}.{ext}"));
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() && meta.len() > 0 {
                return Some(path);
            }
        }
    }
    None
}

/* ───────────────────────── ffmpeg ───────────────────────── */

static FFMPEG: Lazy<Mutex<Option<Option<String>>>> = Lazy::new(|| Mutex::new(None));

fn ffmpeg_path() -> Option<String> {
    if let Ok(cache) = FFMPEG.lock() {
        if let Some(cached) = cache.as_ref() {
            return cached.clone();
        }
    }
    let found = detect_ffmpeg();
    if let Ok(mut cache) = FFMPEG.lock() {
        *cache = Some(found.clone());
    }
    found
}

fn detect_ffmpeg() -> Option<String> {
    if let Ok(raw) = std::env::var("HGHUB_FFMPEG") {
        let path = PathBuf::from(raw.trim());
        if path.is_file() {
            return Some(path.to_string_lossy().to_string());
        }
    }
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：不要给用户闪一个黑框
    }
    cmd.status()
        .ok()
        .filter(|status| status.success())
        .map(|_| "ffmpeg".to_string())
}

/// 无损重封装成 mp4：视频/音频都 `-c copy`，只在必要时修正 AAC 的 ADTS 头。
fn remux_to_mp4(input: &Path, output: &Path) -> Result<(), String> {
    let ffmpeg = ffmpeg_path().ok_or("未检测到 ffmpeg")?;
    let attempt = |with_bsf: bool| -> Result<(), String> {
        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-c")
            .arg("copy");
        if with_bsf {
            cmd.arg("-bsf:a").arg("aac_adtstoasc");
        }
        cmd.arg("-movflags")
            .arg("+faststart")
            .arg(output)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let out = cmd.output().map_err(|e| format!("调用 ffmpeg 失败：{e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("ffmpeg 返回失败")
                .chars()
                .take(200)
                .collect())
        }
    };
    attempt(true).or_else(|_| attempt(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_encrypted_playlist_with_relative_segments() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n\
                    #EXT-X-MEDIA-SEQUENCE:7\n\
                    #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?auth_key=1\",IV=0x0123456789ABCDEF0123456789ABCDEF\n\
                    #EXTINF:4.0,\nseg-1.ts?auth_key=1\n#EXTINF:4.0,\nseg-2.ts?auth_key=1\n#EXT-X-ENDLIST\n";
        let playlist = parse_playlist(text, "https://cdn.example/v/a/v.m3u8?sig=1").unwrap();
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(
            playlist.segments[0].uri,
            "https://cdn.example/v/a/seg-1.ts?auth_key=1"
        );
        assert_eq!(playlist.segments[0].seq, 7);
        assert_eq!(playlist.segments[1].seq, 8);
        let key = playlist.segments[0].key.as_ref().unwrap();
        assert_eq!(key.method, "AES-128");
        assert_eq!(key.uri, "https://cdn.example/v/a/key.bin?auth_key=1");
        assert_eq!(key.iv.unwrap()[0], 0x01);
    }

    #[test]
    fn picks_variant_and_keeps_map_for_fmp4() {
        let master = "#EXTM3U\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\nhigh.m3u8\n";
        let playlist = parse_playlist(master, "https://cdn.example/master.m3u8").unwrap();
        assert_eq!(playlist.variants.len(), 2);
        assert_eq!(playlist.variants[1].1, "https://cdn.example/high.m3u8");

        let fmp4 = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nseg1.m4s\n";
        let playlist = parse_playlist(fmp4, "https://cdn.example/x/v.m3u8").unwrap();
        assert_eq!(
            playlist.map.as_deref(),
            Some("https://cdn.example/x/init.mp4")
        );
        assert_eq!(playlist.segments[0].uri, "https://cdn.example/x/seg1.m4s");
    }

    #[test]
    fn rejects_unsupported_protection_instead_of_producing_garbage() {
        for text in [
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:1000@0\n#EXTINF:4,\na.ts\n",
        ] {
            assert!(parse_playlist(text, "https://cdn.example/v.m3u8").is_err());
        }
    }

    #[test]
    fn attribute_split_respects_quoted_commas() {
        let attrs = "NAME=\"audio,main\",URI=\"a.m3u8\",BANDWIDTH=128000";
        assert_eq!(attr_value(attrs, "NAME").as_deref(), Some("audio,main"));
        assert_eq!(attr_value(attrs, "URI").as_deref(), Some("a.m3u8"));
        assert_eq!(attr_u64(attrs, "BANDWIDTH"), Some(128_000));
        assert_eq!(attr_value(attrs, "MISSING"), None);
    }

    #[test]
    fn iv_falls_back_to_sequence_number_and_parses_hex() {
        assert_eq!(iv_from_seq(1)[15], 1);
        assert_eq!(iv_from_seq(258)[14..16], [1, 2]);
        let iv = parse_iv("0xA1").unwrap();
        assert_eq!(iv[15], 0xA1);
        assert!(parse_iv("zz").is_none());
        assert!(parse_iv("0x1234567890123456789012345678901234").is_none());
    }

    /// 用已知明文 + 固定 key/IV 构造密文，验证 CBC 解密与去填充。
    #[test]
    fn aes128_cbc_roundtrip_strips_padding() {
        use aes::cipher::BlockEncrypt;

        let key = *b"0123456789abcdef";
        let iv = *b"fedcba9876543210";
        let plain = b"\x47\x40\x11segment-payload".to_vec();
        let mut buf = plain.clone();
        let pad = 16 - plain.len() % 16;
        buf.extend(std::iter::repeat_n(pad as u8, pad));

        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut prev = iv;
        let mut encrypted = Vec::with_capacity(buf.len());
        for chunk in buf.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let mut out = block;
            cipher.encrypt_block((&mut out).into());
            encrypted.extend_from_slice(&out);
            prev = out;
        }

        let decrypted = aes128_cbc_decrypt(&key, &iv, &encrypted).unwrap();
        assert_eq!(decrypted, plain);
        assert!(aes128_cbc_decrypt(&key, &iv, b"short").is_err());
    }

    #[test]
    fn filenames_are_sanitized_and_ordered() {
        assert_eq!(
            file_stem(Naming::ETitle, "霸道总裁", 3, "第三次相遇"),
            "霸道总裁 - E03 - 第三次相遇"
        );
        assert_eq!(file_stem(Naming::ETitle, "剧名", 12, "  "), "剧名 - E12");
        assert_eq!(file_stem(Naming::EpOnly, "剧名", 3, "无关标题"), "第03集");
        assert_eq!(
            file_stem(Naming::SeasonEpisode, "剧名", 3, ""),
            "剧名 S01E03"
        );
        // 非法字符替换、结尾点去掉、保留名加后缀
        assert_eq!(sanitize_part("a/b:c*?"), "a_b_c__");
        assert_eq!(sanitize_part("名字..."), "名字");
        assert_eq!(sanitize_part("con"), "con_");
        assert_eq!(sanitize_part("   "), "未命名");
        assert_eq!(sanitize_part(&"字".repeat(200)).chars().count(), 80);
    }

    #[test]
    fn only_complete_files_are_treated_as_existing() {
        let dir = std::env::temp_dir().join(format!("hgdl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(existing_file(&dir, "剧名 - E01").is_none());
        std::fs::write(dir.join("剧名 - E01.ts"), b"").unwrap();
        assert!(existing_file(&dir, "剧名 - E01").is_none());
        std::fs::write(dir.join("剧名 - E01.ts"), b"x").unwrap();
        assert!(existing_file(&dir, "剧名 - E01").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concatenation_follows_zero_padded_names() {
        let dir = std::env::temp_dir().join(format!("hgdl-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seg-000002.bin"), b"c").unwrap();
        std::fs::write(dir.join("seg-000001.bin"), b"b").unwrap();
        std::fs::write(dir.join("seg-000000.bin"), b"a").unwrap();
        std::fs::write(dir.join("joined.bin"), b"ignore").unwrap();
        let out = dir.join("out.bin");
        assert_eq!(concat_segments(&dir, &out).unwrap(), 3);
        assert_eq!(std::fs::read(&out).unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
