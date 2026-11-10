# 剧集批量下载

选集面板右上角的下载按钮 → 勾范围 → 逐集落盘。整条链路在
`src-tauri/src/download.rs`，前端只负责参数与进度展示
（`src/components/DownloadSheet.tsx`）。

## 一次下载发生了什么

| 步骤 | 位置 | 说明 |
| --- | --- | --- |
| 1. 取详情 | `episode_download` | `GET /api/videos/{id}`，只用来补剧名与每集标题；拉不到不影响下载，命名退化为不带标题 |
| 2. 取播放地址 | `api::play_info` | 与播放器共用同一份逻辑：先 `POST /api/videos/{id}/episodes/{ep}/playback`，无地址再回落 `GET /api/videos/{id}/play?ep=` |
| 3. 拉列表 | `resolve_playlist` | 主列表（`#EXT-X-STREAM-INF`）自动挑最高码率下钻一层；`#EXT-X-MAP` 的 fMP4 初始化段会被记住 |
| 4. 预取密钥 | `download_one` | AES-128 的 KEY 整集只取一次，避免每片重复请求 |
| 5. 并发取分片 | `spawn_segment` | `JoinSet` 限流到「分片并发」路；每片独立落 `seg-%06d.bin` |
| 6. 解密 | `fetch_and_decrypt` | AES-128-CBC；未给 `IV` 时用分片序号的大端 128 位，与 HLS 规范一致 |
| 7. 按序拼接 | `concat_segments` | 文件名零填充，字典序即播放顺序 |
| 8. 封装 / 落位 | `remux_to_mp4` | fMP4 直接改名；TS 在检测到 ffmpeg 时 `-c copy` 转 mp4，失败则退回 `.ts` |

出网全部走 `net::fetch_bytes`，因此继承 `network_policy` 的内网拦截、
明文降级拒绝、逐跳重定向复检与三次重试——下载器没有另开 HTTP 客户端。

## 落盘结构

```
<目标目录>/<剧名>/
  ├─ 剧名 - E01 - 第一集标题.mp4
  ├─ 剧名 - E02.mp4            ← 该集没有标题时省略后缀
  └─ .hgdl-117-3/              ← 下载中的临时分片，成功或失败都会清掉
```

临时目录以 `.` 开头，正常流程结束即删除；中途崩溃留下的残片不会被
误认为成品——成品判定要求 `.mp4` / `.ts` 存在**且非空**。

命名三种口径（弹层里可切，选择会记住）：

| 取值 | 结果 |
| --- | --- |
| `e_title`（默认） | `剧名 - E03 - 第三次相遇.mp4` |
| `ep_only` | `第03集.mp4` |
| `season_episode` | `剧名 S01E03.mp4`（Emby / Jellyfin / Kodi 刮削用） |

`sanitize_part` 会把 `\ / : * ? " < > |` 与 ASCII 控制字符替换掉、
去掉结尾的空格与点、限制 80 字符，并给 Windows 保留名（`CON`、`NUL`、
`COM1`…）补下划线。

## 失败与取消

* 任何一集失败不会中断整批：该集计入 `failed` 并把原因推进 `errors`，
  事件里也有一条 `ep_error`。
* 取消走 `download_cancel`，置 `CANCEL` 标志；循环在每个分片边界检查，
  停止后清除该集临时目录，已完成的集保留。
* 同一时间只允许一个任务（`RUNNING` 用 `compare_exchange` 抢占），
  第二次调用直接返回「已有下载任务在进行」。

## 进度事件

事件名 `hg-download`，Rust 侧 `download::EVENT` 与前端
`api.ts` 的 `DOWNLOAD_EVENT` 必须一致。`phase` 依次为：

`start` → (`ep_start` → `progress`* → `ep_done` | `ep_skip` | `ep_error`)* → `done` | `cancelled`

`progress` 按 ~220ms 节流，`bytes` 是该集已落盘的累计字节数。

## 已知边界

* **BYTERANGE 列表**直接报错而不是产出坏文件；当前源站未使用这种切片方式。
* **SAMPLE-AES / DRM** 不支持，同样显式报错。
* 移动端不提供入口：Android / iOS 没有用户可直接访问的公共下载目录，
  `download_reveal` 在移动端也会返回不支持。
* 分片并发上限 12，默认 4 —— 再高容易触发源站限速，反而更慢。

## 调试

```bash
# 指定 ffmpeg 路径（默认从 PATH 找）
HGHUB_FFMPEG=/usr/local/bin/ffmpeg pnpm app:dev

# 只看下载相关的单元测试（离线，不联网）
cd src-tauri && cargo test --lib download::
```

单元测试覆盖：加密列表解析、主列表选流、BYTERANGE/SAMPLE-AES 拒绝、
引号内逗号、IV 回落、AES-CBC 往返、文件名清洗、以及临时分片拼接顺序。
