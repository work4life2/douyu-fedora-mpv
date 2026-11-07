# douyu —— 斗鱼直播客户端（原生 GUI / 视频内嵌）

Rust 写的原生斗鱼直播客户端。**单窗口**：视频内嵌在 GUI 里，输入房间号播放，侧边栏切换画质，弹幕叠加，无缝续流。

## 特性

- **视频内嵌单窗口**：libmpv 渲染（OpenGL 零拷贝优先，失败回退软件渲染）→ egui 纹理显示，视频和 UI 在同一个窗口（可调大小、f/双击全屏）。
- **双击启动，GUI 输入房间号**：粘贴直播间链接也行，回车或点「确定」即播。
- **记住上次的直播间**：退出后再打开直接接着看，房间号、画质、弹幕开关都沿用上次（配置文件见下）。
- **播放时不待机**：默认开启，看完不因为屏保/自动休眠黑屏中断；侧边栏可随时关掉。
- **侧边栏画质按钮**：原画/蓝光4M/超清/高清，点击无缝切换。
- **实时弹幕**：egui 直接叠在视频上（不经过 libass），可开关；密集时按车道排程不重叠。
- **硬解自动选择**：hwdec=auto-copy-safe（vaapi/nvdec/vdpau 依次探测，失败自动回退软解）。实际生效值看日志 `[fps]` 行末尾的「硬解」字段——之前写死 nvdec 时探测失败完全无感知。
- **按显示区选档**：加载房间时按视频区宽度封顶取流档位（<1100px 取超清、<1700px 取蓝光、再大才取原画）。900px 宽的窗口解码 1440p 原画 = 解码面积 8 倍于显示，纯浪费算力。点侧边栏画质按钮随时可手动改回。
- **无缝续流**：本地 FLV 中转，解决 300 秒强制断开（无需登录）。
- **扫码登录**：侧边栏「扫码登录」→ 手机斗鱼 App 扫码确认，登录态存本地，解锁原画并减少断流；也支持手动粘贴 Cookie。
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
./douyu login-test             # 调试：验证二维码接口 + 首次轮询
./douyu login-exchange <URL>   # 调试：验证「扫码确认后换登录态」的 HTTPS 通路
./douyu danmaku-stress [条/秒]  # 调试：往 GUI 里注入合成弹幕（默认 60 条/秒），肉眼验证密集场景
```

> 真实热门直播间对未登录游客会限流（实测 380 万人气的房间只有约 1 条/秒），
> 想看密集效果用 `danmaku-stress`：它走的是和真实弹幕完全相同的队列/上限/丢弃计数。

播放中：`f` 或双击画面全屏，`ESC` 退出全屏。

### 本地配置（记住上次直播间）

退出前看的房间、画质、弹幕开关、待机策略都会存下来，下次打开自动接着放。
文件位置（与 cookie 同一套优先级，取最近修改的那个）：

1. 可执行文件同级 `douyu_config.txt`（绿色版最直观）
2. `$XDG_CONFIG_HOME/douyu/config.txt`，默认 `~/.config/douyu/config.txt`（装到只读目录时用）

```ini
last_room=9999        # 上次成功播放的房间号
prevent_sleep=1       # 1=播放时阻止息屏/休眠
danmaku_on=1          # 弹幕总开关
quality=0             # 0 原画 / 4 蓝光4M / 3 超清 / 2 高清
```

手改坏了不要紧：解析容忍垃圾行，非法画质档位会被忽略，删掉文件即恢复默认。

### 阻止自动待机

`systemd-inhibit --what=idle:sleep --mode=block`，锁由 systemd-logind 持有，
KDE（PowerDevil）/GNOME 都认。三条要点：

- **不引依赖**：不做 D-Bus 直连。`gdbus call ... Inhibit` 这类一次性命令退出即断连，
  锁立刻失效，必须有个常驻壳 —— `systemd-inhibit` 本身就是。
- **不自作多情**：不含 `handle-lid-switch`，合盖照常睡眠（那是用户的明确意图）。
- **不留下副作用**：子进程每 3 秒 `kill -0` 探活一次，本进程被 `kill -9` 后子进程自己退出，
  不会出现「关了客户端系统再也不待机」。程序内退出时还会整组 SIGTERM。

没有 `systemd-inhibit`（非 systemd 发行版）时侧边栏会标红说明原因，不影响播放。

## 内存（实测与看门狗）

症状：开播约 300MB，看一小时涨到 1.4GB。已在真机复现并定位到 **libmpv 的
render-API→外部 FBO 路径**，不是本项目的 Rust 代码。

### 实测数据

用内置的 `[mem]` 诊断行（每秒一行）在真机（mpv 0.41 + GTX 1650S）上量：

| 场景 | RSS 增速 | 备注 |
|---|---|---|
| 不开播（只有 UI，60fps 重绘） | 0 KB/s | `[heap]` 完全不动，Rust 活字节 96.0MB 恒定 |
| 播放 1280×720@30，渲染到 904×508 | +280 KB/s | 堆 +155 KB/s |
| 播放 1920×1080 原画@30，渲染到 904×508 | **+1.2 MB/s** | 比 720p 快 4 倍（跟码率/源分辨率走） |
| `pause=yes`（流已连，不解码不渲染） | 0 KB/s | **只有帧真往下走才漏** |
| `hwdec=no`（纯软解） | +155 KB/s | 与硬解无关 |
| `ao=null`（不出声） | +172 KB/s | 与音频无关 |
| 独立 `mpv --vo=gpu` 自己的窗口 | 0 KB/s（50 秒共 +240KB） | mpv 本身不漏 |

结论：每渲染一帧漏一小块 C 侧内存，**速率跟源码率/分辨率走**，原画 1080p 能到 1MB/s
量级（几 GB/小时）。桶在 `mpv_render_context` 里。已逐项排除：`report_swap`、
`block-for-target-time`、`gpu-api=opengl`、`target-trc`/`video-output-levels`、
`cache`、缩放算法、音频、网络。同一时间段内 Rust 侧分配计数（`[mem]` 的「Rust 活」）
**一个字节都没涨**。

另外两笔「涨上去就不还」的固定占用，也顺手处理了：

- mpv `cache=auto` 默认要 150MiB 解复用缓存 + 75MiB 回看，直播流用不到，
  基线因此凭空高 120~150MB。改成 `demuxer-max-bytes=32MiB` / `demuxer-max-back-bytes=8MiB`。
- Noto Sans CJK 字体整套文件 93.5MB（`[mem]` 的「最大档」就是它），换子集字体可再省 ~90MB。

### 处理：渲染上下文定期重建（有坑）

有画面后记 RSS 基线，涨过 `MEM_GROWTH_MB`（默认 200MB）就
`mpv_render_context_free` + `create` 重建一次渲染上下文，泄漏对象随之释放。

> **坑：只重建上下文会把视频打死。** 渲染上下文一释放，挂在它上面的硬解设备
> （vulkan/vaapi interop）就没了，解码链跟着断——现象是 `width/height` 变 0、
> `hwdec-current` 变空、画面定格（日志里 `视频 0x0`）。所以重建后必须
> `set vid no` → `set vid auto` 把视频轨重新挂一遍，硬解会在 2~4 秒内自动回到
> `vulkan-copy`。少这一步，看起来就像「修完更糟」。

实测：强制 8 秒重建一次，`视频 1280x720` 全程稳定、渲染 60fps、丢帧 0，
RSS 在 663~693MB 之间锯齿；正常 200MB 阈值下原画约 3 分钟一次、720p 约 11 分钟一次。

```bash
DOUYU_MEM_LIMIT_MB=400 ./douyu   # 放宽阈值（锯齿更大，但重建更少）
DOUYU_RC_RECYCLE=8 ./douyu       # 调试：固定周期重建，肉眼可见 RSS 锯齿
```

> 真正的修法在 mpv 侧（换 `--wid` 嵌入或等上游修 render API），
> 这里是可控的兜底，代价是每次重建后硬解要重新初始化 2~4 秒。
> 侧边栏底部直接显示「内存 xxxMB」，不用再去 htop 猜。

### `[mem]` 诊断行

```
[mem] Rust 活 96.0MB/1537块 (+2KB/s) | RSS 545MB (+0KB/s) | 最大档 32MB=93.5MB ...
```

- **Rust 活**：全局分配器统计的活字节/块数。一路涨 = 我们自己漏；平的 = 不是我们。
- **RSS**：读 `/proc/self/statm`。
- **最大档**：活字节最大的尺寸档。这里的 93.5MB 是 Noto Sans CJK 字体文件本身，
  想再省 ~90MB 可以换子集字体。

诊断/救急还可以直接往 mpv 塞选项，不用改代码重编：

```bash
DOUYU_MPV_OPTS="hwdec=no,cache=no" ./douyu
```


### 登录排障

同目录 `douyu.log` 里所有登录动作都带 `[login]` 前缀，出问题先看那里：

```bash
grep '\[login\]' douyu.log | tail -20
```

常见链路：二维码已生成 → 轮询: 等待扫码 → 已扫待确认 → 扫码已确认 →
回调第 N 跳（逐跳状态 + cookie 名）→ 登录成功 uid=…。

> 注意：`login.rs` 里的 HTTP agent **必须显式 `.tls_connector()`**。
> 本项目 ureq 用的是 `native-tls` feature，而 ureq 的 `default_tls_config()`
> 只认 `tls`（rustls），不显式设置会退化成 NoTls，所有 HTTPS 直接报
> `cannot make HTTPS request because no TLS backend is configured`。

## 弹幕渲染（密集场景）

`main.rs` 里弹幕不进 libass，而是 egui 直接画在视频层上，自己管排程：

- **车道排程**：屏幕按字号切成 N 条横车道，每条记「下次可发车时刻」= 发车时刻 +
  `(文本宽度 + 间距) / 速度`。同车道后车必须等前车尾部完全进场才发车，
  两车同速 → 之后永不重叠。宽度用 layout 后的真实像素宽（不是「字符数 × 字号」，
  那对中文/emoji 会差一倍）。
- **密度自适应**：1 秒内丢够 8 条就升档（字号 ×0.88/0.80/0.72，横穿 8→5.8 秒），
  连续 8 秒零丢弃再降档。1280×720 下四档吞吐约 10 / 15 / 20 / 28 条/秒。
- **背压**：队列上限 400 条（丢最旧），每帧最多上新 16 条，在屏上限 200 条。
  热房每秒几百条也只是「看起来在动」，不会撑爆内存，也不会让某一帧卡死。
- **文本清洗**：控制字符（尤其 `\n`，`layout_no_wrap` 会排成多行压到别的车道）转空格、
  合并连续空白、超长截断到 60 字；`@S`/`@A` 转义按协议还原成 `/` 和 `@`。
- **零分配解析**：非 `chatmsg` 帧不碰 UTF-8 解码；字段查找按字节切片，
  不再为每个 key 拼 `format!`；日志 10 秒一行（原来每 20 条一行，热房等于持续写盘反压读线程）。

## 架构

```
egui (eframe) 单窗口
   ├─ 中央：视频纹理（libmpv render API 软件渲染 rgb0 → egui ColorImage）
   ├─ 侧边栏：画质按钮 / 弹幕开关 / 待机开关 / 全屏
   ├─ 顶部：房间号输入 + 确定
   │
   ├─ mpvffi.rs   libmpv dlopen FFI：vo=libmpv + hwdec=auto-copy-safe + render API（GL 零拷贝优先）
   ├─ relay.rs    续流中转：本地 HTTP + FLV tag 拼接（300s 断流解法）
   ├─ danmaku.rs  弹幕：WSS(TLS1.2/openssl) + STT 协议（字节级零拷贝解析）
   ├─ login.rs    扫码登录：generateCode + 轮询 + 回调逐跳收 Cookie
   ├─ config.rs   本地配置：上次直播间 / 待机策略 / 画质 / 弹幕开关
   ├─ power.rs    待机抑制：systemd-inhibit + 探活看门狗
   └─ douyu.rs    取流：homeH5Enc → rquickjs 签名 → getH5Play → FLV 直链
```

换房/换画质通过 relay 状态热切换，单进程无外部依赖。

## 测试

```bash
cargo test --release     # 15 个单元测试
```

覆盖：STT 帧切片（含畸形帧不 panic）、字段查找不误命中、`@S/@A` 转义、
弹幕文本清洗（换行/空白/超长）、队列上限与丢弃计数、车道排程不追尾、
四档密度吞吐单调递增、配置文件读写与容错。

## 已实测（沙箱内）

| 功能 | 结果 |
|---|---|
| 取流（签名 + getH5Play） | ✅ |
| 弹幕（WSS + STT，字节级解析） | ✅ 实连 9999 房间，中文/emoji/转义正常 |
| 续流中转（FLV 解析） | ✅ |
| 扫码登录换登录态（回调逐跳收 Cookie） | 🔧 命令行已验证 HTTPS/跳转通路，完整扫码待真机确认 |
| 阻止系统待机 | ✅ `systemd-inhibit --list` 可见 `sleep:idle block` 条目，退出即释放 |
| 记住上次直播间 | ✅ 配置文件读写 + 单元测试 |
| 弹幕密集渲染（车道 + 密度自适应） | ✅ 单元测试仿真不重叠；GUI 观感需真机 |
| libmpv 加载失败降级 | ✅ 优雅提示 |
| GUI 显示 + 内嵌渲染 + 硬解 | 需真机（沙箱无显示/GPU） |

## 仅供学习研究

不包含录制、转分发、付费画质绕过。直播内容版权归平台与主播所有。
