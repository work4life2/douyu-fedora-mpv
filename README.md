# douyu —— 斗鱼直播客户端（原生 GUI / 视频内嵌）

Rust 写的原生斗鱼直播客户端。**单窗口**：视频内嵌在 GUI 里，输入房间号播放，侧边栏切换画质，弹幕叠加，无缝续流。

## 特性

- **视频内嵌单窗口**：libmpv 软件渲染 → egui 纹理显示，视频和 UI 在同一个窗口（可调大小、f/双击全屏）。
- **双击启动，GUI 输入房间号**：粘贴直播间链接也行，回车或点「确定」即播。
- **侧边栏画质按钮**：原画/蓝光4M/超清/高清，点击无缝切换。
- **实时弹幕**：mpv 的 libass 直接渲染进视频帧，可开关。
- **NVDEC 硬解**：hwdec=nvdec-copy。
- **无缝续流**：本地 FLV 中转，解决 300 秒强制断开（无需登录）。
- **中文字体**：自动加载系统 Noto Sans CJK。

## 依赖（运行时）

| 依赖 | 用途 |
|---|---|
| `mpv-libs`（libmpv.so.2） | 视频解码/渲染内核 |
| `mpv`（可选） | 不再必需（内嵌方案不用外部 mpv 进程） |
| 系统 openssl (libssl) | 弹幕 TLS |
| Noto Sans CJK 字体 | 中文显示（Fedora 默认自带） |

```bash
sudo dnf install mpv-libs    # 唯一需要装的
```

## 编译

```bash
cargo build --release
# 产物：target/release/douyu（单文件，双击运行）
```

> **WorkBuddy 沙箱编译**：用 `LD_PRELOAD=$PWD/shim.so cargo build`（见 shim.c）。真机直接 `cargo build`。

## 使用

```bash
./douyu                    # 双击启动 → GUI 输入房间号 → 确定播放
./douyu danmaku-test <房间号>   # 调试：验证弹幕
./douyu relay-test <房间号>     # 调试：验证续流中转
```

播放中：`f` 或双击画面全屏，`ESC` 退出全屏。

## 架构

```
egui (eframe) 单窗口
   ├─ 中央：视频纹理（libmpv render API 软件渲染 rgb0 → egui ColorImage）
   ├─ 侧边栏：画质按钮 / 弹幕开关 / 全屏
   ├─ 顶部：房间号输入 + 确定
   │
   ├─ mpvffi.rs   libmpv dlopen FFI：vo=libmpv + hwdec=nvdec-copy + render(SW)
   ├─ relay.rs    续流中转：本地 HTTP + FLV tag 拼接（300s 断流解法）
   ├─ danmaku.rs  弹幕：WSS(TLS1.2/openssl) + STT 协议
   └─ douyu.rs    取流：homeH5Enc → rquickjs 签名 → getH5Play → FLV 直链
```

弹幕/换房/换画质都通过 libmpv 直接命令（osd-overlay / relay 状态热切换），单进程无外部依赖。

## 已实测（沙箱内）

| 功能 | 结果 |
|---|---|
| 取流（签名 + getH5Play） | ✅ |
| 弹幕（WSS + STT） | ✅ 真实弹幕 |
| 续流中转（FLV 解析） | ✅ |
| libmpv 加载失败降级 | ✅ 优雅提示 |
| GUI 显示 + 内嵌渲染 + 硬解 | 需真机（沙箱无显示/GPU） |

## 仅供学习研究

不包含录制、转分发、账号登录、付费画质绕过。直播内容版权归平台与主播所有。
