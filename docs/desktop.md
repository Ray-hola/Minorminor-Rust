# 桌面版

[项目首页](../README.md) · [English](desktop.en.md)

## 运行要求

- macOS 12 或更新版本；目前在 Apple Silicon Mac 上验证，Intel 构建尚未验证。
- 源码开发需要 Git、Rust 1.88+ 稳定版和 Xcode Command Line Tools（`xcode-select --install`）。Rust 安装见[官方说明](https://rust-lang.org/tools/install/)。
- 首次构建需要联网下载 Cargo 依赖。已构建的 `.app` 不依赖 Node.js 或 Rust 安装。

```sh
git clone --recurse-submodules https://github.com/Ray-hola/Minorminor-Rust.git
cd Minorminor-Rust
./scripts/run-native.sh
```

`start.command` 是相同入口。如果 `vendor/mole/` 为空，运行 `git submodule update --init --recursive`。

## 使用

选择扫描范围，完成扫描后展开项目或工具目录，查看文件夹和文件大小。默认审阅缓存与日志；会话和记忆需要单独启用。预览内再次勾选具体文件，确认后移到废纸篓，通过逐文件去向核对原路径和目标路径。

界面侧栏提供简体中文和 English。切换会重载界面并清空未确认的勾选，后台任务继续运行；语言选择随应用记录保存。文件名、正文、项目介绍、模型回复和原始命令输出保留原文。

未识别用途、凭据、数据库及其 WAL/SHM、模型和依赖等内容保留。近期文件、身份已变化、正在使用或未通过保护检查的文件会跳过；不会移动项目根目录或清空废纸篓。移动后需重新扫描更新占用。可依据逐文件记录从废纸篓手动移回，不承诺 Finder 的「放回原处」。

AI 判读只接收经你审阅的元数据，读取文档和调用工具的权限固定禁用。AI 总结只对你点选并确认的资源抽读：最多 12 个内容文件、160 个条目、32 个目录、3 层深度、48,000 字符；文档抽取和压缩读取另有限制，预览显示实际覆盖。模型不会自动勾选或清理。

## 记录与权限

记录保存在 `~/Library/Application Support/com.minorminor.desktop/`，包含 SQLite 索引、设置和逐文件操作记录；API Key 只在进程内存。服务只监听 `127.0.0.1` 的临时端口，窗口关闭时停止后台任务。应用不会自动申请管理员权限。

扫描错误会区分权限拒绝、路径失效和其他读取错误。需要查看受 macOS 保护的目录时，可在「设置 → 文件访问权限」打开系统设置，自行决定是否授予完全磁盘访问权限，再重新扫描。

本仓库只维护 Rust + WebKit 桌面应用。[Node.js Web 版](https://github.com/Ray-hola/Minorminor)在独立仓库维护，记录、接口与清理流程分别演进。

## 验证与构建

```sh
cargo test --locked --manifest-path native/Cargo.toml --no-default-features
cargo check --locked --manifest-path native/Cargo.toml
./scripts/build-macos.sh
python3 scripts/test-native-smoke.py
```

冒烟测试仅在临时目录生成模拟项目，验证打包进程、扫描同步、目录大小、Mole 保护、废纸篓记录和重扫，不处理真实用户文件。

最后一项使用 Python 3 对打包后的应用执行临时样例验证。

生成 `dist/Minorminor.app`，只包含当前机器架构。构建脚本复制固定版本 Mole、保护脚本和许可文件，并验证 ad-hoc 签名；这不等于 Apple Developer ID 签名或公证。当前未发布通用二进制或正式安装包。

文案位于 `public/locales/en.js`；界面使用 `tr` / `ui` 翻译固定文案，文件路径与内容作为插值保留，不对整个 DOM 执行翻译。
