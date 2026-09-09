# 今晚玩什么（tonight）

Steam 游戏库推荐 Agent——回答「我的下一个小时应该花在哪款已拥有的游戏上」。

完全本地运行：数据不出本机，推荐只依赖你自己的库。

## 功能

- **玩家画像**：Bartle 四维动机 + 深度分层（未开封/试玩即弃/活跃/暂离/已通关；通关判定含剧情覆盖率，通关但没全成就不误判弃坑）+ 注水时长甄别；机器提议、玩家确认，手动标注永远优先。
- **成就分类**：同步期间 LLM 把成就分为剧情/挑战/收集/社交，驱动评分与画像；失败自动回退启发式。
- **对话式推荐**：口语需求 → 意图解析（品类同义组模糊匹配，"肉鸽"=「类 Rogue」）→ 六分量确定性评分（动机/口味/可达/会话/即玩/积压）→ LLM 生成推荐卡（海报标语 + 文案 + 四色数据标签，只引用真实数字）。
- **猜你想玩**：打开页面自动推一轮，无需打字（可关）。
- **探索与反馈**：连续「换一批」进入探索模式（提温 + 换口味探索位）；「不感兴趣」疲劳降权、启动游戏提升即玩倾向——只影响排序，画像页可重置。
- **玩家标注**：卡片与画像页可直接改深度档、「已玩完/没玩完」，标注立即生效。
- **时间与带宽感知**：按时段问候与快捷需求；未安装游戏按存储/磁盘余量/带宽估算下载时长。
- **用量透明**：每次 LLM 调用的 token 与费用落库（含缓存命中分档与高峰/空闲时段计价），日预算硬停。

## 快速开始

**方式 A：发行包（无需装 Rust，推荐）**

1. Windows：解压 `tonight-windows-x64.zip`，双击 `start.bat`；
   Linux/macOS：解压对应 `.tar.gz`，执行 `sh start.sh`（macOS 首次需 `xattr -cr .`）。
   浏览器自动打开 http://127.0.0.1:8668；
2. 首次引导：粘贴两个密钥（[Steam Web API Key](https://steamcommunity.com/dev/apikey) + LLM API Key）→ 可选勾项 → 确认 Steam 资料公开 → 自动抓库。不用 DeepSeek 时展开「自定义服务」填端点/密钥变量名/价格。

**方式 B：源码运行（开发）**

要求 Rust stable（Windows 用 MSVC）。

```bash
cp .env.example .env      # 填入 STEAM_WEB_API_KEY 与 DEEPSEEK_API_KEY（也可在网页里填）
cargo run --release -- serve --open
```

> **Windows Smart App Control**：SAC 会拦截 rustc 构建脚本（错误 4551），需先关闭；发行包不受影响。
>
> **大陆网络**：`api.steampowered.com` 可能被干扰（证书错误），自动走系统代理，或 `config.toml [network] proxy` 显式指定；Linux/macOS 只认环境变量（`HTTPS_PROXY`/`ALL_PROXY`）。
>
> **Steam 目录**：Windows 走注册表，Linux/macOS 探测默认路径；检测不到时在 `config.toml [steam] install_dir` 指定。

## Web 界面

`tonight serve` 后访问 http://127.0.0.1:8668（默认 8668，占用自动顺延；支持 `#profile` 深链）。

| 页面 | 内容 |
|---|---|
| 对话 | 纵向整页翻轮 + 横向 peek 轮播；轨道末尾 CTA：有余量「换一批」、可放宽「放宽条件」、到头「换个说法」；每轮可折叠工具轨迹（默认收起）：阶段标记、推荐语逐张进度（1/3…，流式）、每笔 token 与费用 |
| 库存 | 全量游戏网格（封面/时长/深度/安装状态），深度角标点击手动标注，注水角标 + 批量勾选标注；异常清单区分两类——**数据类可自动修复**（商店详情缺失、成就拉取失败、成就类型标注缺失，一键修复带实时日志）、**判断类手动标注**（琥珀色：疑似已玩完、疑似时长注水）；一键同步（进度条 + ETA + 实时日志） |
| 画像 | 四维、深度分布、品类偏好、证据、注水提议确认、行为反馈与重置 |
| 历史 | 会话按访问分组；每轮封面+标题缩略图横排，点击浮层回放原卡 |
| 设置 | 密钥与端点（**两个「测试连接」按钮**：Steam 一次探测；LLM 免费探测 + 拉模型列表）、自定义 LLM 服务与三档价格、用量与预算、模型热切换、数据与文件绝对路径 |

## CLI

```bash
tonight sync                     # 全量同步（Tier A–F；--skip-llm 跳过成就分析）
tonight sync --local-only        # 只刷新本地安装状态
tonight profile [--review-idle]  # 终端画像 / 确认注水提议
tonight recommend [--tag 冒险 --max-session 60 --instant-only --llm-positioning]
tonight ask                      # 终端对话式推荐
tonight serve [--open] [--port 8668]  # Web 界面
```

## 文件与数据目录

所有文件相对**程序工作目录**；启动时若当前目录没有 `web/` 自动切到 exe 所在目录。设置页「数据与文件」可看绝对路径。

| 路径 | 内容 | 首次运行 |
|---|---|---|
| `config.toml` | 全部可调参数（带注释自动生成） | 自动创建 |
| `.env` | 两个 API 密钥 | 网页保存密钥时创建 |
| `data/tonight.db3` | 游戏缓存、画像、标注、用量、会话 | 自动创建 |
| `data/sessions/` | 对话会话存档 | 按需创建 |
| `data/logs/tonight.log` | 运行日志（>1MB 轮转；排障首选） | 自动创建 |
| `web/` | 前端静态资源 | 随程序分发 |

换电脑 = 拷走整个目录。CLI 可用 `--config` / `--db` 覆盖位置。

## Troubleshooting

先点设置页的两个「测试连接」：

- **Steam 测试**：401 = Key 无效；网络错误 = 连不上 Steam（大陆需代理）。
- **LLM 测试**：免费探测（0 token）。401 = Key 无效；402 = 余额不足；404 = base_url 大概率缺 `/v1`；429 = 限流（已自动退避）。

| 症状 | 处理 |
|---|---|
| 同步报 403 | Steam 资料未公开：把「我的资料」与「游戏详情」都设为公开 |
| 证书错误/超时 | 走系统代理或显式配置 `proxy`；Linux/macOS 只认环境变量 |
| 首次同步慢 | 商店接口限速所致（36/60 次每分双通道，LLM 分析 4 路并行），进度条与 ETA 实时可见；之后增量秒级 |
| 商店详情零星失败 | 间歇性风控，自动退避 + 30 秒补试，仍失败下次同步自动补 |
| 白屏/异常 | 看 `data/logs/tonight.log`；`RUST_LOG=debug` 提级 |

## 配置

- `.env`：两个密钥（网页保存时原子写入、保留注释）；仓库不存任何真实密钥。
- `config.toml`：`[llm.*]` 端点与价格、`[agent]` 预算与定位增强、`[profile]` 判定阈值、`[recommender]` 权重与探索感、`[network]` 代理与带宽、`[steam]` 安装目录。
- 价格：内置常用模型价格表（DeepSeek 自动分高峰/空闲），Web 端可按平台定价覆盖三档（输入/缓存/输出，元/百万 tokens）；设置页修改热生效，不动 config.toml。

## 隐私与安全

- 原始数据仅存本地 SQLite；发给 LLM 的只有画像摘要与候选事实包，不含 SteamID 与完整库清单。
- `.env`、数据库、日志均已 gitignore；错误信息与日志中的密钥自动掩码（URL 查询串 / Bearer / `sk-` / `key=` 四形态）。
- Web 只绑 `127.0.0.1` + Host 校验（含静态资源），防跨站改写与 DNS rebinding；密钥永不回显，查询只返回尾 4 位。

## 测试

```bash
cargo test   # 80 项：VDF/manifest、账号选择、存储、密钥脱敏、代理归一化、成就分类与流式解析、
             # 四维合成、三源定位、意图品类匹配（同义组/防误并）、软件类目、.env 写入、
             # 同步/LLM 错误映射、抽样与探索位、计费（分档/覆盖/时段/预算）、剧情覆盖率、玩家标注等
```

## 打包发行

```bash
packaging\dist.bat        # Windows x64 → dist/ + tonight-windows-x64.zip
sh packaging/dist.sh      # Linux x64 + macOS x64/arm64（cargo-zigbuild 交叉编译）
```

发行包不含任何用户状态（`.env`/`config.toml`/`data/` 打包前自动挪出），首次引导必在新机器正常弹出。

## 项目结构

```
src/
  vdf.rs          # Valve KeyValues 解析（本地清单文件）
  steam_local.rs  # 本地解析：库目录/安装清单/账号（三平台）
  steam_client.rs # Steam API：库/成就/全球完成度/商店详情（双通道限流+退避）
  store.rs        # SQLite：缓存/标注/会话/用量/行为反馈
  llm.rs          # OpenAI 兼容客户端：计价 + 流式 + 错误脱敏与友好映射
  profiler.rs     # 成就类型分析（4 路并行）+ 四维画像 + 注水提议
  recommender.rs  # 六分量评分 + 抽样 + 探索位 + 品类同义组匹配
  agent.rs        # 意图解析 → 评分 → 推荐卡（流式 + 校验 + 模板兜底），CLI/Web 共用
  server.rs       # axum：REST + SSE + Host 校验 + 密钥/设置/测试/路径端点
  secrets.rs      # .env 原子写入与密钥校验
  sync.rs         # 分层同步编排（三通道并行 + 结构化进度 + 取消令牌）
web/              # 原生 HTML/CSS/JS（无构建链）
packaging/        # 发行打包脚本与启动模板
```
