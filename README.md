# 今晚玩什么（tonight）

Steam 游戏库推荐 Agent——回答「我的下一个小时应该花在哪款已拥有的游戏上」。

## 它能做什么

从你的 Steam 库真实数据（时长、成就、全球完成度、商店标签、本地安装状态）出发：

- **玩家画像**：Bartle 四维动机画像（成就完成/探索发现/竞争对抗/社交合作）+ 行为特征 + 深度分层（未开封/试玩即弃/活跃/暂离/已通关）+ **注水时长甄别**（挂卡时长不污染口味）——机器提议、玩家确认，修正层永远覆盖自动判定；
- **成就分类**：LLM 在同步期间把成就分为「剧情推进/挑战技巧/收集解锁/社交趣味」，分类结果驱动成就维度的评分与画像，失败自动回退启发式；
- **对话式推荐**：口语需求（"今晚想玩点轻松的，一个小时"）→ 意图解析 → 六分量确定性评分（动机/口味/可达/会话/即玩/积压）→ LLM 生成推荐卡（海报标语 + 广告文案），四色数据标签由 Rust 从事实包计算、只引用真实数字；
- **猜你想玩**：打开页面自动给出一轮零输入推荐（意图来自画像与时段，无需打字，可在引导/设置中关闭）；
- **探索感与行为反馈**：连续「换一批」自动进入探索模式（提高抽样温度 + 强制一席「换个口味」探索位，新意图或启动游戏后退出）；卡片可「不感兴趣」（累积疲劳降权）、启动游戏提升即玩倾向——反馈只影响排序与展示，画像页一键重置；
- **玩家标注**：卡片与画像页可直接修正机器判定——深度档改判（含「暂离」）、「已玩完/没玩完」快捷按钮、证据存档可回溯，标注立即生效并参与后续推荐；
- **时间与带宽感知**：按时段给出问候与快捷需求（夜间默认 30 分钟）；"未安装"卡片按存储需求、磁盘余量与下载带宽参考估算下载时长，`steam://install` 一键预约下载；
- **即点即玩**：卡片标注"立即可玩/需更新/未安装"，`steam://` 一键启动；
- **用量透明**：每次 LLM 调用的 token 与费用落库（含缓存命中分档计价），三层定价（内置价格表/自定义覆盖/未知仅计数），日预算硬停，Web 端可完整自定义 LLM 服务（服务档案名/密钥变量名/输入·缓存·输出三档价格）。

## 快速开始

**方式 A：发行包（无需装 Rust，推荐给使用者）**

1. Windows：解压 `tonight-windows-x64.zip`，双击 **`start.bat`**；
   Linux / macOS：解压 `tonight-linux-x64.tar.gz` / `tonight-macos-{x64,arm64}.tar.gz`，
   终端进入目录执行 **`sh start.sh`**（macOS 首次运行需 `xattr -cr .` 解除未签名限制）。
   浏览器都会自动打开 http://127.0.0.1:8668；
2. 首次打开会出现**一次性引导**：粘贴两个密钥（Steam Web API Key + LLM API Key）→ 可选：勾选「打开页面自动推荐」与「LLM 游戏定位增强」；不用 DeepSeek 时展开「自定义服务」填写端点/密钥变量名/价格 → 确认 Steam 资料公开 → 自动抓取游戏库。完成后即可开聊。

**方式 B：源码运行（开发）**

要求：Rust stable（Windows 用 MSVC 工具链；Linux/mac 用各自默认工具链）。

```bash
# 1. 配置密钥（复制模板并填入两个 key；也可直接在 Web 设置页/引导里填写）
#    STEAM_WEB_API_KEY: https://steamcommunity.com/dev 申请（需登录 Steam）
#    DEEPSEEK_API_KEY:  DeepSeek 开放平台（任意 OpenAI 兼容服务均可，见 config.toml [llm.*]）
cp .env.example .env

# 2. 编译并启动 Web 界面（--open 自动打开浏览器）
cargo run --release -- serve --open
# 浏览器打开 http://127.0.0.1:8668
```

首次同步会自动带出本机 Steam 账号（也可在设置页手填 SteamID64；Steam 档案「游戏详情」需公开），然后去「对话」页开聊。

> **Windows + Smart App Control 注意**：若系统开启了 SAC，会拦截 rustc 现场编译的构建脚本（错误 4551），需先在"Windows 安全中心 → 应用和浏览器控制"关闭；发行包方式不受影响（SmartScreen 提示时选「更多信息 → 仍要运行」）。
>
> **大陆网络**：`api.steampowered.com` 直连可能被干扰（表现为证书错误），工具会自动走系统代理（如 Clash）；也可在 `config.toml [network] proxy` 显式指定。Linux/macOS 不读系统代理设置，只认环境变量（`HTTPS_PROXY`/`ALL_PROXY` 等）或显式配置。
>
> **Steam 目录检测**：Windows 走注册表；Linux 探测 `~/.steam/steam`、`~/.local/share/Steam` 等；macOS 探测 `~/Library/Application Support/Steam`。检测不到时在 `config.toml [steam] install_dir` 直接指定即可（全平台生效）。

## Web 界面

`tonight serve` 后浏览器访问 http://127.0.0.1:8668（默认 8668，被占用自动顺延，`--port` 可指定；左侧竖版导航，支持 `#profile` 等 hash 深链直达）：

- **对话**：核心体验——纵向整页翻轮（滚轮/方向键/圆点导航），每轮推荐为横向 peek 轮播（当前卡居中、两侧露相邻卡），轨道末尾的虚线 CTA 卡滑过去即"换一批"；卡片含封面、海报标语、文案、四色推荐点（动机/口味/积压/成就中位）、`▶ 启动游戏`，探索位卡带「🎲 换个口味」角标；符合条件的游戏看完后可一键放宽筛选；
- **猜你想玩**：进入对话页即自动生成一轮推荐（基于画像与当前时段），关闭开关则回到纯对话流；
- **库存**：全量游戏网格（封面/时长/深度/安装状态），非游戏软件自动剔除，头部一键更新；库为空时直接给出可操作的同步面板（SteamID64 输入 + 前置条件清单 + 失败原因保留）；
- **画像**：四维、深度分布、标签级品类偏好、证据、疑似注水提议一键确认/否决；深度档可直接改判（含「暂离」）、「已玩完/没玩完」快捷标注；行为反馈区（即玩倾向条 / 疲劳命中 / 事件计数 / 一键重置）；
- **历史**：全部会话回放（每轮意图、候选、卡片）；
- **设置**：API 密钥（写入本机 `.env`，即时生效，只显示尾 4 位）与 OpenAI 兼容端点（base_url/model）、**自定义 LLM 服务**（服务档案名/密钥变量名/输入·缓存·输出三档价格，一键恢复内置价格表）、用量与预算（日预算硬停 + 未计价调用计数）、模型热切换、LLM 定位增强开关、探索感滑条、下载带宽参考、数据同步（实时进度 + 停止，失败原因可读）、重新运行首次引导。

## CLI

```bash
tonight sync                     # 全量同步（Tier A–F；--skip-llm 跳过成就类型分析）
tonight sync --local-only        # 只刷新本地安装状态
tonight profile [--review-idle]  # 终端画像 / 逐项确认注水提议
tonight recommend [--tag 冒险 --max-session 60 --instant-only --llm-positioning]
tonight ask                      # 终端对话式推荐
tonight serve [--open] [--port 8668]  # Web 界面（--open 自动开浏览器；端口被占自动顺延）
```

## 配置

- **`.env`**（不进仓库）：两个 API key，见 `.env.example`；Web 设置页/引导可直接填写并自动写入（原子写、保留手写注释；改密钥变量名时先更新变量名再写 key，不断链）；
- **`config.toml`**：模型端点与价格（任意 OpenAI 兼容服务可配多个）、评分权重、注水判定阈值、代理等——所有参数均可调，注释齐全；
- **价格**：内置常用模型价格表（快照随版本更新），Web 端可按第三方平台定价页覆盖三档（输入/缓存命中/输出，元/百万 tokens）——输入与输出都填才生效，留空缓存档按全价保守计；
- Web「设置」页的修改存本地数据库（meta 表）热生效，不改动手写的 config.toml。

## 隐私与安全

- 所有原始数据仅存本地 SQLite（`data/`，已 gitignore）；发给 LLM 的只有画像摘要与候选事实包，不含 SteamID 与完整库清单；
- `.env`、密钥文件、数据库、日志均已列入 `.gitignore`，仓库中不得出现任何真实密钥；
- 错误信息中的 API Key 自动脱敏为 `key=***`；
- Web 服务只绑 `127.0.0.1`，并做 **Host 校验**（非本机 Host 一律 403）——防止恶意网页借用户浏览器跨站 POST 改写 `.env` 或接口地址（DNS rebinding 同样被拦）；
- 密钥通过 Web 填写时：请求值永不进日志、永不回显，查询接口只返回「已设置 + 尾 4 位」。

## 测试

```bash
cargo test   # 62 项：VDF/manifest 解析、账号选择（含 Timestamp 兜底）、存储、key 脱敏、代理归一化、
             # 成就分类与 LLM 输出解析、四维合成、三源定位映射、软件类目判定、
             # .env 写入与密钥校验、同步错误友好映射、抽样与探索位、
             # 计费（缓存命中分档/价格覆盖/预算）、玩家标注覆盖、引导状态等
```

## 打包发行

```bash
packaging\dist.bat        # Windows x64：dist/tonight-win64/ + tonight-windows-x64.zip
sh packaging/dist.sh      # Linux x64 + macOS x64/arm64：cargo-zigbuild 交叉编译，产出三个 .tar.gz
```

发行包不含任何用户状态（`.env`/`config.toml`/`data/` 打包前自动挪出并校验），首次引导一定会在新机器上正常弹出。交叉编译要求 cargo-zigbuild + zig（`pip install ziglang` 即可）+ rustup 目标；Windows 上路径含空格会自动建 subst 虚拟盘。

## 项目结构

```
src/
  vdf.rs          # Valve KeyValues 极简解析器（本地 Steam 清单文件）
  steam_local.rs  # 本地解析：库目录/安装清单/账号（Windows/Linux/macOS 三平台探测）
  steam_client.rs # Steam API：库/成就/全球完成度/商店详情与用户标签（限流+退避）
  store.rs        # SQLite：缓存/标注/会话/用量/行为反馈（Mutex<Connection> 跨 await 共享）
  llm.rs          # OpenAI 兼容客户端：usage 记账 + 缓存命中分档计价
  profiler.rs     # 成就类型在线分析（C4）+ 四维画像 + 注水提议
  recommender.rs  # 六分量确定性评分 + 受控随机抽样 + 探索位注入
  agent.rs        # 意图解析 → 评分 → 推荐卡（schema 校验 + 模板兜底），CLI/Web 共用
  server.rs       # axum：REST + SSE + Host 校验 + 密钥/引导/设置端点
  secrets.rs      # .env 原子写入与密钥校验（Web 设置/引导共用）
  sync.rs         # 分层同步编排（Tier A–F，进度回调 + 取消令牌）
web/              # 原生 HTML/CSS/JS 前端（无构建链；onboarding.js 为首次引导）
packaging/        # 发行打包：dist.bat（Windows）/ dist.sh（Linux/mac，zigbuild 交叉编译）、启动脚本模板、使用说明
```
