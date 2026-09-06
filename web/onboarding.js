// 首次打开一次性引导：欢迎 → 填密钥（写本机 .env，即时生效）→ 同步 Steam 库。
// 依赖 app.js 的全局助手：$ / esc / api / sseStream。
"use strict";

(function () {
  let boot = null; // GET /api/bootstrap 的聚合状态
  let step = 0;

  const dots = () =>
    `<div class="ob-steps">${[0, 1, 2].map((i) => `<i class="${i <= step ? "on" : ""}"></i>`).join("")}</div>`;
  // 外层 #onboarding 只做全屏遮罩，卡片单独一层（否则 flex 布局会挤压内容）
  const card = (html) => `<div class="ob-card">${html}</div>`;

  function box() {
    return $("#onboarding");
  }

  function render() {
    const b = box();
    if (!b) return;
    if (step === 0) renderWelcome(b);
    else if (step === 1) renderKeys(b);
    else renderSync(b);
  }

  // ---- 第 1 步：欢迎 + 隐私说明 ----
  function renderWelcome(b) {
    b.innerHTML = card(`
      ${dots()}
      <div class="ob-logo">🎮</div>
      <h2>欢迎使用「今晚玩什么」</h2>
      <p>这是一个<b>完全本地运行</b>的 Steam 库内推荐 Agent：读取你自己的游戏库与成就数据，
         回答「今晚玩什么」。所有数据只保存在本机 data\\ 目录；推荐时也只发送匿名的游戏画像摘要，不发送账号信息。</p>
      <p>开始前需要两样东西：<b>① Steam Web API Key</b>　<b>② 一个 LLM API Key</b>（DeepSeek 或任意 OpenAI 兼容服务）。全程约 2 分钟。</p>
      <div class="ob-actions">
        <button class="btn primary" id="ob-next">开始配置 →</button>
        <button class="btn ob-skip" id="ob-skip">跳过引导</button>
      </div>`);
    b.querySelector("#ob-next").onclick = () => { step = 1; render(); };
    b.querySelector("#ob-skip").onclick = () => complete(true);
  }

  // ---- 第 2 步：密钥（POST /api/secrets 落 .env 并即时生效）----
  function renderKeys(b) {
    const st = boot || {};
    const state = (set, tail) => (set ? `已设置 ••••${esc(tail || "")}` : "未设置");
    b.innerHTML = card(`
      ${dots()}
      <h2>填写 API 密钥</h2>
      <p>密钥只写入本目录的 <code>.env</code> 文件，不会出现在对话或日志里；本页也只显示尾 4 位。</p>
      <div class="field"><label>Steam Web API Key（<a href="https://steamcommunity.com/dev/apikey" target="_blank" rel="noopener">点此免费申请</a>）· ${state(st.steam_key_set, st.steam_key_tail)}</label>
        <input type="password" id="ob-steam-key" class="ob-input" autocomplete="off" placeholder="32 位字母数字"></div>
      <div class="field"><label>LLM API Key（<a href="https://platform.deepseek.com" target="_blank" rel="noopener">DeepSeek 申请页</a>，或任意 OpenAI 兼容服务）· ${state(st.llm_key_set, st.llm_key_tail)}</label>
        <input type="password" id="ob-llm-key" class="ob-input" autocomplete="off" placeholder="sk-…"></div>
      <div class="grid2">
        <div class="field"><label>接口地址 base_url</label>
          <input id="ob-base-url" class="ob-input" value="${esc(st.base_url || "https://api.deepseek.com/v1")}" placeholder="https://api.deepseek.com/v1"></div>
        <div class="field"><label>模型名 model（可点「获取模型」拉取候选）</label>
          <div style="display:flex;gap:8px">
            <input id="ob-model" class="ob-input" value="${esc(st.model || "deepseek-chat")}" placeholder="deepseek-chat" list="ob-model-list">
            <button class="btn" id="ob-model-fetch" style="white-space:nowrap">获取模型</button>
            <datalist id="ob-model-list"></datalist>
          </div></div>
      </div>
      <details class="ob-adv">
        <summary>自定义服务（非 DeepSeek：服务名 / 密钥变量名 / 价格）</summary>
        <div class="grid2">
          <div class="field"><label>服务名称（如 glm / kimi；留空沿用默认）</label>
            <input id="ob-llm-name" class="ob-input" value="${esc(st.llm_name || "")}" placeholder="deepseek"></div>
          <div class="field"><label>密钥变量名（key 写入 .env 的变量名；留空沿用默认）</label>
            <input id="ob-key-env" class="ob-input" value="${esc(st.llm_key_env || "")}" placeholder="DEEPSEEK_API_KEY"></div>
          <div class="field"><label>价格 · 输入（元/百万 tokens，缓存未命中）</label>
            <input type="number" id="ob-price-in" class="ob-input" step="0.05" min="0" value="${st.price_input ?? ""}" placeholder="内置价格表自动"></div>
          <div class="field"><label>价格 · 输入缓存命中（元/百万 tokens；无缓存计费可留空）</label>
            <input type="number" id="ob-price-cache" class="ob-input" step="0.05" min="0" value="${st.price_cache ?? ""}" placeholder="留空 = 按全价保守计"></div>
          <div class="field"><label>价格 · 输出（元/百万 tokens）</label>
            <input type="number" id="ob-price-out" class="ob-input" step="0.05" min="0" value="${st.price_output ?? ""}" placeholder="内置价格表自动"></div>
        </div>
        <p>常见模型已内置价格（快照 2026-09），留空即可；第三方平台价差大，建议按其定价页填写，否则费用统计会失真。</p>
      </details>
      <div class="ob-msg" id="ob-llm-hint" style="display:none"></div>
      <div class="check"><input type="checkbox" id="ob-auto-rec" ${st.auto_recommend !== false ? "checked" : ""}><label for="ob-auto-rec">打开页面自动推荐「猜你想玩」——按画像直接推一轮，省一次输入</label></div>
      <div class="check"><input type="checkbox" id="ob-llm-pos" ${st.llm_positioning ? "checked" : ""}><label for="ob-llm-pos">LLM 游戏定位增强——同步时对每款游戏做一次定位分析（缓存复用，少量额外 LLM 费用），画像与推荐更贴合动机类型</label></div>
      <div class="ob-msg" id="ob-msg"></div>
      <div class="ob-actions">
        <button class="btn primary" id="ob-save">保存并继续 →</button>
        <button class="btn ob-skip" id="ob-later">稍后再填</button>
      </div>`);
    const savePrefs = () =>
      api("/api/settings", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          auto_recommend: b.querySelector("#ob-auto-rec").checked,
          llm_positioning: b.querySelector("#ob-llm-pos").checked,
        }),
      });
    // 获取模型列表：用刚填还没保存的 base_url/key，成功后填入下拉建议（手动输入不受影响）
    b.querySelector("#ob-model-fetch").onclick = async () => {
      const msg = b.querySelector("#ob-msg");
      msg.textContent = "获取模型列表中…";
      try {
        const r = await api("/api/llm/models", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            base_url: b.querySelector("#ob-base-url").value.trim(),
            key: b.querySelector("#ob-llm-key").value.trim() || null,
          }),
        });
        b.querySelector("#ob-model-list").innerHTML = r.models.map((m) => `<option value="${esc(m)}">`).join("");
        msg.textContent = `✔ 已获取 ${r.models.length} 个模型，点击模型名输入框即可选择`;
      } catch (e) {
        msg.textContent = "✖ " + e.message +
          (e.message.includes("404") ? "（确认 base_url 是否包含 /v1）" : "");
      }
    };
    // 价格字段只在用户改过后才提交（预填值原样回发会把旧价固化成 meta 覆盖）；
    // 声明在 llmHint 之前——提示刷新会读它判断是否同步价格预填
    let priceDirty = false;
    ["#ob-price-in", "#ob-price-cache", "#ob-price-out"].forEach((id) =>
      b.querySelector(id).addEventListener("input", () => { priceDirty = true; }));
    // 参数约束提示 + 价格跟随：规则匹配在后端，前端只展示 notice；
    // syncPrice=true（模型/base_url 被改动时）且价格未被手动改过 → 预填同步为该模型
    // 将生效的价格（绑定覆盖优先，否则内置表，未知则清空待填）。初始渲染不同步，
    // 保留 bootstrap 返回的当前生效价。
    const hintEl = b.querySelector("#ob-llm-hint");
    const llmHint = async (syncPrice) => {
      try {
        const r = await api(`/api/llm/hint?base_url=${encodeURIComponent(b.querySelector("#ob-base-url").value.trim())}&model=${encodeURIComponent(b.querySelector("#ob-model").value.trim())}`);
        hintEl.textContent = r.notice || "";
        hintEl.style.display = r.notice ? "block" : "none";
        if (syncPrice && !priceDirty) {
          b.querySelector("#ob-price-in").value = r.price ? r.price.input : "";
          b.querySelector("#ob-price-cache").value = r.price && r.price.cache != null ? r.price.cache : "";
          b.querySelector("#ob-price-out").value = r.price ? r.price.output : "";
          const ph = r.price ? "内置价格表自动" : "该模型不在内置价格表，建议按官方定价填写";
          b.querySelector("#ob-price-in").placeholder = ph;
          b.querySelector("#ob-price-out").placeholder = ph;
        }
      } catch { /* 提示失败不影响使用 */ }
    };
    b.querySelector("#ob-base-url").addEventListener("input", () => llmHint(true));
    b.querySelector("#ob-model").addEventListener("input", () => llmHint(true));
    llmHint();
    b.querySelector("#ob-later").onclick = async () => {
      try { await savePrefs(); } catch (e) { /* 偏好保存失败不阻塞进入 */ }
      step = 2;
      render();
    };
    b.querySelector("#ob-save").onclick = async () => {
      const msg = b.querySelector("#ob-msg");
      msg.textContent = "保存中…";
      try {
        const sk = b.querySelector("#ob-steam-key").value.trim();
        const lk = b.querySelector("#ob-llm-key").value.trim();
        const num = (id) => {
          const v = b.querySelector(id).value.trim();
          return v === "" ? null : Number(v);
        };
        const pin = num("#ob-price-in");
        const pout = num("#ob-price-out");
        // 顺序关键：先保存设置（可能改了密钥变量名，服务端热更新 env 名），
        // 再写密钥——/api/secrets 按最新的变量名写入 .env，避免 key 落到旧变量断链
        await api("/api/settings", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            llm_base_url: b.querySelector("#ob-base-url").value.trim(),
            llm_model: b.querySelector("#ob-model").value.trim(),
            llm_name: b.querySelector("#ob-llm-name").value.trim(),
            llm_key_env: b.querySelector("#ob-key-env").value.trim(),
            // 价格：用户改过且输入/输出都填了才提交（预填回发与半填状态都视为未改）
            ...(priceDirty && pin !== null && pout !== null ? {
              price_input: pin,
              price_cache: num("#ob-price-cache"),
              price_output: pout,
            } : {}),
            auto_recommend: b.querySelector("#ob-auto-rec").checked,
            llm_positioning: b.querySelector("#ob-llm-pos").checked,
          }),
        });
        if (sk || lk) {
          await api("/api/secrets", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ steam_key: sk || null, llm_key: lk || null }),
          });
        }
        boot = await api("/api/bootstrap");
        step = 2;
        render();
      } catch (e) {
        msg.textContent = "✖ " + e.message;
      }
    };
  }

  // ---- 第 3 步：同步游戏库（SSE 进度；要求资料公开）----
  function renderSync(b) {
    const st = boot || {};
    const local = st.local_account;
    b.innerHTML = card(`
      ${dots()}
      <h2>抓取你的 Steam 游戏库</h2>
      <p>最后一步：抓取游戏库并生成玩家画像（首次约 2–5 分钟，之后增量秒级）。</p>
      <p class="ob-warn">⚠ 需要你的 Steam 资料对公众可见：Steam 个人资料 → 隐私设置，
         把「我的资料」与「游戏详情」都设为「公开」，否则 Steam API 会拒绝查询（403）。</p>
      <div class="field"><label>SteamID64${local ? `（检测到本机账号：${esc(local.persona_name || local.steamid)}）` : "（留空自动检测本机登录的 Steam）"}</label>
        <input id="ob-steamid" class="ob-input" value="${esc(local ? local.steamid : "")}" placeholder="17 位 SteamID64"></div>
      <div class="field"><label>下载带宽参考（Mbps，用于估算"未安装"游戏的下载时长）</label>
        <input id="ob-dl-speed" class="ob-input" type="number" min="1" step="10" value="${Number((typeof getDownloadSpeed === "function" && getDownloadSpeed()) || 100)}"></div>
      <div class="ob-log hidden" id="ob-log"></div>
      <div class="ob-msg" id="ob-msg"></div>
      <div class="ob-actions">
        <button class="btn primary" id="ob-sync">开始同步</button>
        <button class="btn ob-skip" id="ob-finish">暂不同步，直接进入</button>
      </div>`);
    b.querySelector("#ob-finish").onclick = () => complete(true);
    b.querySelector("#ob-sync").onclick = async () => {
      const btn = b.querySelector("#ob-sync");
      const log = b.querySelector("#ob-log");
      const msg = b.querySelector("#ob-msg");
      const steamid = b.querySelector("#ob-steamid").value.trim();
      // 带宽参考即时持久化（localStorage，设置页同源）
      const speed = Math.max(1, Number(b.querySelector("#ob-dl-speed").value) || 100);
      if (typeof setDownloadSpeed === "function") setDownloadSpeed(speed);
      btn.disabled = true;
      btn.textContent = "同步中…";
      log.classList.remove("hidden");
      log.textContent = "开始同步…";
      await sseStream("/api/sync/start", { steamid: steamid || null }, (ev) => {
        if (ev.type === "progress") {
          log.textContent += "\n" + ev.payload.text;
        } else if (ev.type === "done") {
          log.textContent += "\n✔ 同步完成";
          setTimeout(() => complete(false), 800);
        } else if (ev.type === "error") {
          log.textContent += "\n✖ " + ev.payload.message;
          msg.textContent = "同步失败：可按上方提示修正后重试，或先直接进入。";
          btn.disabled = false;
          btn.textContent = "重试同步";
        }
        log.scrollTop = log.scrollHeight;
      });
    };
  }

  // ---- 完成 / 跳过：落 meta（下次不再弹），刷新横幅与侧栏 ----
  // skipped=true 表示用户主动跳过：状态记为 skip，之后不再自动弹（设置页可手动重开）
  async function complete(skipped) {
    try {
      await api("/api/onboarding/complete", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ skipped: !!skipped }),
      });
    } catch (e) { /* 服务异常也不阻塞进入 */ }
    const b = box();
    if (b) b.classList.add("hidden");
    if (typeof checkSyncBanner === "function") checkSyncBanner();
    if (typeof loadMini === "function") loadMini();
  }

  // 打开前拉最新状态（设置页「重新运行首次引导」也走这里）
  function open() {
    api("/api/bootstrap")
      .then((d) => { boot = d; })
      .catch(() => {})
      .finally(() => {
        const b = box();
        if (!b) return;
        b.classList.remove("hidden");
        render();
      });
  }

  // 启动：没完成过引导才自动弹
  api("/api/bootstrap")
    .then((d) => {
      boot = d;
      // none=从未走完 → 弹；done 但什么都没配（历史/带进来的旧库）→ 补弹；
      // skip=用户主动跳过 → 不再打扰
      const unconfigured = !d.steam_key_set && !d.llm_key_set && !d.owned;
      const st = d.onboarded_state || (d.onboarded ? "done" : "none");
      if (st === "none" || (st === "done" && unconfigured)) {
        const b = box();
        if (b) {
          b.classList.remove("hidden");
          render();
        }
      }
    })
    .catch(() => {}); // 服务未就绪/请求失败时保持安静，不打扰

  window.TonightOnboarding = { open };
})();
