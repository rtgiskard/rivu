# Rivu

面向 Linux 的本地音乐播放器。原生 Rust 播放核心，桌面 GUI、CLI 和极简 TUI 共用同一实例、队列与数据库。

**只播放音频。** MP4、MKV、WebM 可以作为音频容器导入；视频轨道不显示、不解码。没有 FFmpeg、GStreamer 或 GLib 媒体后端，也不会启动外部播放器。

## 构建与运行

工具链固定为 Rust 1.99.0，依赖由 `Cargo.lock` 锁定。需要 ALSA、桌面图形与字体开发库；Arch Linux 可安装：

```sh
sudo pacman -S --needed rust pkgconf alsa-lib opus fontconfig libxkbcommon-x11 libxcb
cargo build --release --locked
./target/release/rivu
```

GUI 使用固定上游提交的 GPUI + `gpui_wgpu`，支持 X11 和 Wayland，需要可用的 Vulkan/OpenGL 驱动。同时锁定 GPUI 上游的 calloop 补丁，保证嵌套空闲回调不必等到下一次鼠标事件才执行。中文优先使用系统已安装的无衬线字体；缺少中文字体可安装 `noto-fonts-cjk`。

```sh
# GUI；启动时导入文件或文件夹
./target/release/rivu gui ~/Music

# 不开 GUI，前台运行核心
./target/release/rivu serve ~/Music

# 连接已经运行的 GUI 或核心
./target/release/rivu tui
./target/release/rivu status
./target/release/rivu scan ~/Music --wait
./target/release/rivu list --query piano
./target/release/rivu play 12
./target/release/rivu seek 30
./target/release/rivu volume 60
./target/release/rivu devices
./target/release/rivu device '设备的准确名称'
./target/release/rivu device                 # 恢复默认输出
./target/release/rivu quit
```

`--data-dir DIRECTORY` 为 GUI、CLI、TUI 选择同一个独立数据目录；适合测试或管理不同曲库。`--json` 输出机器可读的状态或探测结果。

默认数据在 XDG 数据目录的 `rivu` 下，通常为 `~/.local/share/rivu`。数据库是 `library.db`，新桌面的布局是 `workspace.json`，本地控制套接字是 `rivu.sock`。旧版 `layout.json` 不再使用。GUI 关闭会停止该实例；需要无 GUI 的持续运行时明确使用 `serve`。TUI 的退出只关闭 TUI。

## 曲库、歌单与统计

- 后台扫描目录，读取真实元数据；文件大小和修改时间未变时复用解码探测与内容指纹。
- 曲目拥有持久 ID。明确识别到原文件消失且指纹唯一时，移动后的文件保留编辑、歌单引用和统计；仍存在的副本不会合并。
- 缺失文件保留记录并标记。移除曲目只删除曲库记录，不删除媒体文件。
- 标题、艺术家、专辑编辑是 **Rivu 数据库覆盖值**，不是写入媒体标签；重新扫描保留覆盖值。
- 播放队列与保存歌单分离；同一曲目可以出现多次，每个出现位置有独立 ID，可独立移动或移除。
- M3U/M3U8 导入保留顺序和重复项，解析相对路径；导出写入明确指定的路径。

```sh
./target/release/rivu queue add 12 18 12
./target/release/rivu queue list
./target/release/rivu queue move 3 0          # 出现位置 ID，目标索引从 0 开始
./target/release/rivu playlist new '安静'
./target/release/rivu playlist add 1 12 18 12
./target/release/rivu playlist play 1
./target/release/rivu playlist import mix.m3u8 --wait
./target/release/rivu playlist export 1 exported.m3u8
./target/release/rivu edit 12 --title '新的标题'
./target/release/rivu stats
./target/release/rivu history
```

收听时长按输出回调实际消费的 PCM 帧累计，不按墙钟时间或跳转后的播放位置累计。暂停、缓冲空洞、跳转不增加收听时长。每次播放会话达到 `min(240 秒, 曲长 / 2)` 时计一次播放；未知曲长阈值为 240 秒。同一会话只计一次，停止、切歌、自然结束和退出保存最终帧计数。增量统计每秒落库；异常退出后将未结束会话标记为 interrupted，最多损失最后一次增量保存之后的时长。界面统计在会话结束时刷新，持久化增量不触发整库重建。

## 桌面与终端

GUI 提供搜索、多选、双击播放、队列/歌单顺序调整、元数据编辑、设备选择和历史统计。选中曲目后显示对应操作按钮；文件路径输入和文件拖放均可导入。

中文优先采用系统已安装的黑体/无衬线字体，正确选择 TTC 字体集合中的字面。曲库、队列与歌单曲目使用固定行高；长标题截断显示，悬停可读完整文本，避免换行破坏虚拟列表定位。侧栏队列独立滚动，不挤走频谱区。

`+ Panel` 菜单添加曲库、队列、歌单、曲目详情、历史、频谱、频谱历史或设置面板。拖动标签到另一组中心可合并为标签页，拖到四边可分割停靠；拖到标签上可调整标签顺序。拖动分隔线调整尺寸，标签上的关闭按钮移除面板；操作后立即保存布局，重启恢复。`Settings` 中可重置工作区。新增面板类型只需在 `gui.rs` 的 `PANELS` 注册表中注册渲染函数。

频谱和频谱历史独立开关。分析使用 8192 点 Hann FFT，保留左右声道功率；频谱图横轴为时间，新声音从右侧进入，纵轴低频在下、高频在上。历史采用固定长度环形缓冲和缓存的单列图像，只更新新增列，不重建整幅图。分析帧率可设为 5–60，默认 20；分析面板不可见、窗口隐藏或播放暂停时停止相应工作。静止界面不运行全局定时刷新。曲库、队列、歌单和历史使用虚拟化行。

TUI：`Space` 播放/暂停，`n`/`p` 下一首/上一首，`+`/`-` 跳转 ±5 秒，`[`/`]` 音量，`q`/`Esc` 退出。TUI 使用紧凑状态查询，不持续传输整个曲库。

## 配置与桌面媒体控制

配置默认位于 `~/.config/rivu/config.toml`（遵循 XDG 配置目录），也可用 `--config FILE` 指定。`--data-dir` 不改变配置路径；隔离运行时应同时指定两者。首次启动创建配置；缺失字段使用默认值，未知字段、非法值和格式错误明确报错，不覆盖原文件。

```toml
library_roots = ["/home/you/Music"]
# output_device = "PipeWire Sound Server"  # 省略时使用系统默认输出
volume = 0.7
shuffle = false
repeat = "off"                          # off / all / one
mpris_enabled = true
ui_scale = 1.0                          # 0.75–2.0
analysis_fps = 20                       # 5–60
```

路径是字面文件路径，不展开 `~` 或环境变量。没有启动导入参数时扫描配置中的曲库目录；设置界面中多个目录用分号分隔。`Apply & save` 校验并原子保存、立即应用配置，`Reload file` 重新读取磁盘配置，`Discard edits` 丢弃尚未应用的表单修改。界面音量用 0–100%，TOML 用 0.0–1.0。输出设备不可用时显示错误，可在设备列表选取有效设备，不静默替换输出。

默认启用 MPRIS：桌面媒体键和 `playerctl -p rivu` 可控制播放、暂停、停止、切歌、跳转、音量、循环和随机播放；曲目元数据与播放状态双向同步。配置开关可即时注册或释放总线名称 `org.mpris.MediaPlayer2.rivu`。没有会话总线或名称已被另一实例占用时，设置面板显示原因，播放器仍可使用。仅支持本地已导入曲目；不提供 MPRIS `OpenUri` 或 TrackList 接口。

## 格式边界

Symphonia 负责容器与内置解码器：PCM/WAV、FLAC、MP3、Vorbis、AAC-LC、ALAC 等；libopus 负责 Ogg/WebM/MKV 的 Opus。Opus 支持 mono/stereo mapping family 0，处理编码器预跳过与包裁剪。单声道复制到左右声道，多声道降混为立体声，设备采样率不同则使用 sinc 重采样。

容器后缀不保证音轨可解码。视频专用文件、图片、HE-AAC 或未支持的 AC-3/DTS 等音轨会明确报错，不伪装成成功、不静默交给外部程序。曲目内部采样率变化、Opus 多声道映射、视频显示和跨曲目无缝播放不在当前支持范围。

```sh
./target/release/rivu probe 'concert.mkv'
./target/release/rivu --json probe 'track.opus'
cargo test --locked
```

## 结构

- `audio.rs`：探测、解码、降混、重采样、CPAL 输出与实际帧计数。
- `analysis.rs`：有界、可关闭的音频分析工作线程。
- `library.rs` / `store.rs`：后台扫描与单拥有者 SQLite 事务。
- `core.rs` / `model.rs`：共享命令、状态、队列、会话与生命周期。
- `gui.rs` / `gui/`：GPUI 桌面、面板注册、布局树、Unicode/IME 输入和 GPU 可视化。
- `config.rs` / `mpris.rs`：配置校验与原子保存、事件驱动的桌面媒体控制。
- `terminal.rs` / `main.rs`：TUI 与 CLI，复用同一播放核心。
- `ipc.rs`：权限为 0600 的本地 Unix socket、有限请求大小和并发连接。
