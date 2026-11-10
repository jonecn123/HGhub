# HGhub

**基于 Rust 与 Tauri 2 的跨平台界面与交互实践项目。**

HGhub 起源于个人对 Tauri 2 和移动端开发的学习与尝试。感谢大家的关注与贡献；项目由个人利用业余时间维护，关注度的增长不会改变维护范围，也不代表服务或支持承诺。



## 技术与目录

| 层 | 技术 |
| --- | --- |
| 应用框架 | Rust、Tauri 2 |
| 界面 | React、TypeScript、Vite、Tailwind CSS |
| 播放组件 | hls.js |
| 网络 | reqwest、rustls |

```text
src/                    界面、交互、状态管理与组件
src-tauri/              原生能力与 Rust 实现
scripts/                开发、构建与验证脚本
tests/                  本地测试页面与回归用例
docs/                   技术说明与维护记录
```

## 本地开发

基础环境：Node.js 22、pnpm 10、Rust 1.90 或更高版本，以及目标平台所需的 Tauri 开发环境。

```bash
pnpm install --frozen-lockfile
pnpm dev             # 前端开发
pnpm app:dev         # 桌面应用开发
pnpm build           # 类型检查与前端构建
```

各平台配置位于 `src-tauri/tauri*.conf.json`。移动端及 macOS 开发需要相应平台的 SDK 和工具链。

- exFAT 等不支持符号链接的文件系统需保留 `.npmrc` 中的 `node-linker=hoisted`；部分原生构建步骤仍可能受文件系统限制。
- 依赖安装脚本的允许列表见 `package.json` 与 `pnpm-workspace.yaml`。
- 内容处理、网络安全边界与本地回归说明见[客户端内容与网络安全](docs/security.md)。

## 许可证

当前版本适用 [PolyForm Noncommercial License 1.0.0](LICENSE)。本页说明不替代许可证全文，也不另行增加用户或开发者行为规范。

历史版本 v1.0.1 及更早版本曾以 MIT 许可证发布，已取得的相应授权不因后续换证而撤回；自 v1.1.0 起的新版本适用 PolyForm Noncommercial 1.0.0。


