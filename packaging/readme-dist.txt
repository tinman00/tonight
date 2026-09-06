「今晚玩什么」——Steam 库内推荐 Agent
================================================

三步上手
--------
1. Windows：双击「start.bat」；Linux / macOS：终端进入本目录执行「sh start.sh」。
   首次运行如遇 Windows SmartScreen 蓝色提示，点「更多信息」→「仍要运行」。
   浏览器会自动打开 http://127.0.0.1:8668（没自动打开就手动访问该地址）。
2. 首次打开会出现引导，粘贴两个免费密钥即可：
   · Steam Web API Key（30 秒申请）：https://steamcommunity.com/dev/apikey
   · LLM API Key（DeepSeek 或任意 OpenAI 兼容服务）：https://platform.deepseek.com
3. 引导最后一步自动抓取你的 Steam 游戏库（首次约 2–5 分钟），
   之后在对话页问一句「今晚玩什么」就能拿到推荐。

注意
----
· 需要你的 Steam 资料对公众可见：Steam 个人资料 → 隐私设置，
  把「我的资料」和「游戏详情」都设为「公开」，否则 Steam API 会拒绝查询。
· 所有数据（游戏库、画像、会话记录）只保存在本目录 data\ 下，不上传任何服务器。
· 两个密钥保存在本目录 .env 文件中；推荐时只发送匿名的游戏画像摘要，不发送你的账号信息。
· 停止服务：在黑窗口按 Ctrl+C，或直接关闭窗口。
· 换电脑：把整个文件夹拷走即可，配置和数据都在里面。

Linux / macOS 补充
------------------
· 运行方式：终端进入本目录，执行 sh start.sh（或先 chmod +x start.sh tonight
  再 ./start.sh）。
· macOS 首次运行如提示「无法验证开发者」（安装包未签名）：终端在本目录执行
  xattr -cr . 后重试；或系统设置 → 隐私与安全性 → 仍要打开。
· Steam 安装目录若未被自动检测到，编辑 config.toml 的 [steam] install_dir 指定：
  Linux 常见为 ~/.local/share/Steam 或 ~/.steam/steam；macOS 为
  ~/Library/Application Support/Steam。
· 代理兜底：Linux/macOS 只读环境变量（HTTPS_PROXY / ALL_PROXY 等），
  不读取系统代理设置。
