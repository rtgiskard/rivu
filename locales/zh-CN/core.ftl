core-ready = Rivu 核心已就绪。数据目录：{ $path }
core-command-accepted = 命令已接受，修订号 { $revision }
core-stats = 曲目：{ $tracks }
    播放次数：{ $plays }
core-listing-changed = 列表未完成：读取期间数据已更改，请重试命令。
core-missing = [缺失]
core-favorite = [收藏]
core-unknown = 未知
core-playing = 播放中
core-paused = 已暂停
core-stopped = 已停止
core-play = 播放
core-pause = 暂停
core-previous = 上一首
core-next = 下一首
core-shuffle = 随机播放
core-shuffle-on = 随机播放 ✓
core-repeat = 循环播放
core-repeat-on = 循环播放 ✓
core-quit = 退出
core-playlist-summary = { $id }	{ $name }	{ $count } 首
core-playlist-entry = { $index }	条目 { $entry }	曲目 { $track }
core-relocation-accepted = 已接受迁移验证，任务 { $id }
core-relocation-completed = 迁移完成：原曲目 { $source } 已使用目标 { $target } 的路径
core-duplicate-groups = 历史同内容分组：{ $count }（使用缓存哈希，未经重新验证）
core-duplicate-members = 历史同内容成员：{ $count }
core-duplicate-group = { $fingerprint }	{ $total } 个成员	{ $missing } 个缺失	{ $present } 个可用
core-duplicate-member = { $id }	{ $path }	{ $status }	收藏={ $favorite } 修改={ $overrides } 播放={ $plays } 播放列表引用={ $playlists } 队列引用={ $queue } 独立={ $independent }
core-present = 可用
core-optimization = 数据库优化完成
    数据库：{ $before } -> { $after } 字节
    WAL：{ $wal-before } -> { $wal-after } 字节
    共释放：{ $reclaimed } 字节
core-state-track = { $status }：{ $artist } — { $title }  { $position }/{ $duration } 秒  音量 { $volume }%
core-state-empty = { $status } · { $tracks } 首 · 队列 { $queued } 首
core-ffmpeg = FFmpeg 扩展音频解码：{ $status }
core-scan-progress = 扫描：{ $phase } · { $processed }/{ $total } · { $errors } 个错误
core-probe = { $path }
      标题：{ $title }
      艺术家：{ $artist }
      专辑：{ $album }
      编码：{ $codec }
      声道：{ $channels }
      采样率：{ $sample-rate } Hz
      时长：{ $duration }
      码率：{ $bitrate }
      源位深：{ $bits }
      曲目编号：{ $track }
      碟片编号：{ $disc }
      发行日期：{ $date }
      探测后端：{ $probe }
      解码后端：{ $decoder }
      FFmpeg 扩展音频解码：{ $ffmpeg }
core-help-about = 安静、以本地为先的音乐播放器
core-help-long = Rivu 使用共享 Rust 核心播放本地音频。不带子命令启动桌面界面；命令行和终端界面控制同一实例。可选 FFmpeg 支持按需加载，用于备用解码或增强探测。
core-help-usage = 用法：
core-help-gui = 打开桌面界面，可同时导入路径。
core-help-serve = 在前台运行核心，不打开图形界面。
core-help-tui = 使用简洁的终端界面控制运行中的实例。
core-help-playback = 控制播放、音量和音频输出设备。
core-help-library = 导入和管理曲目，查看元数据和收听信息。
core-help-queue = 管理当前播放队列。
core-help-playlist = 管理、导入和导出播放列表。
core-help-quit = 退出运行中的图形界面或后台核心。
core-help-playback-play = 播放指定曲目，或继续当前队列。
core-help-playback-volume = 设置音量百分比，范围 0..100。
core-help-playback-devices = 列出输出设备，不启动实例。
core-help-playback-device = 按完整名称选择输出设备，或使用默认设备。
core-help-library-scan = 导入音频文件、CUE 文件或目录，不中断播放。
core-help-library-list = 列出曲库，可按文本筛选。
core-help-library-edit = 编辑曲库元数据，不修改媒体文件。
core-help-library-remove = 移除曲库记录，不删除媒体文件。
core-help-library-favorite = 收藏或取消收藏，不修改媒体文件。
core-help-library-clean-missing = 清理缺失记录及其引用，不删除媒体文件。
core-help-library-status = 显示播放和曲库状态。
core-help-library-stats = 显示曲库规模和累计播放次数。
core-help-library-history = 显示最近播放的曲目，每首仅列一次。
core-help-library-probe = 读取元数据并检查解码支持，不启动实例。
core-help-library-optimize = 回收数据库空间并更新查询统计；请先停止播放和扫描。
core-help-library-duplicates = 只读查看历史内容哈希相同的文件分组。
core-help-library-relocate = 在已验证的目标路径保留缺失曲目的身份。
core-help-library-relocation-status = 查看保留的迁移验证和完成状态。
core-help-library-cancel-relocation = 取消待处理的迁移验证，不撤销已提交的迁移。
core-help-library-keep-independent = 禁止此文件参与后续身份复用。
core-help-playlist-import = 按曲目顺序导入 M3U/M3U8 播放列表或 CUE 文件。
core-help-library-scan-paths = 绝对路径，或相对此客户端工作目录的路径。
core-help-library-scan-force = 即使文件大小和修改时间未变，也重新探测元数据。
core-help-library-probe-enhanced = 忽略已保存的设置，加载 FFmpeg 进行增强探测。
core-help-library-duplicates-fingerprint = 列出一个哈希的成员，而非分组摘要。
core-help-library-relocate-wait = 等待重新验证和数据库原子提交。
core-help-playlist-import-path = 播放列表绝对路径，或相对此客户端工作目录的路径。
core-help-playlist-export-path = 相对此客户端工作目录的目标路径，无需预先存在。
core-help-commands = 命令
core-help-options = 选项
core-help-arguments = 参数
core-help-help = 显示帮助
core-help-version = 显示版本
core-help-help-subcommand = 要查看帮助的命令
core-relocation-status = 迁移 { $id }：{ $source } -> { $target } · { $phase }
core-verifying = 验证中
core-completed = 已完成
core-cancelled = 已取消
core-failed = 失败
core-discovering = 查找文件
core-reading-metadata = 读取元数据
core-saving = 保存中
core-yes = 是
core-no = 否
core-missing-marker = { " " }[缺失]
core-favorite-marker = { " " }[收藏]
core-last-scan-missing = 缺失（上次扫描）
core-last-scan-present = 可用（上次扫描）
core-help-data-dir = Rivu 数据目录。
core-help-config = 要加载的配置文件。
core-help-json = 输出供程序读取的 JSON。
