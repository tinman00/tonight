// 「今晚玩什么」Chat Box 前端（原生 JS，无构建链；design.md F13 / R2·R4·R5·R6）
"use strict";

const $ = (sel) => document.querySelector(sel);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};
// Steam CDN 封面（header.jpg 460×215，无需 API；加载失败时隐藏，保留渐变底）
const cover = (app_id) => `https://cdn.akamai.steamstatic.com/steam/apps/${app_id}/header.jpg`;

async function api(path, opts) {
  const resp = await fetch(path, opts);
  const data = await resp.json().catch(() => ({}));
  if (data.error) throw new Error(data.error);
  return data;
}

// fetch POST + SSE 流式读取（服务端用默认事件类型的 data: 行）
async function sseStream(url, body, onEvent) {
  const resp = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    onEvent({ type: "error", payload: { message: "HTTP " + resp.status } });
    return;
  }
  const reader = resp.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf("\n\n")) >= 0) {
      const chunk = buf.slice(0, i);
      buf = buf.slice(i + 2);
      for (const line of chunk.split("\n")) {
        if (line.startsWith("data:")) {
          try {
            onEvent(JSON.parse(line.slice(5).trim()));
          } catch (e) { /* 忽略坏行 */ }
        }
      }
    }
  }
}

// ============ Tab 切换 ============
document.querySelectorAll("#nav button").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll("#nav button").forEach((b) => b.classList.toggle("active", b === btn));
    document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t.id === "tab-" + btn.dataset.tab));
    if (btn.dataset.tab === "library") loadLibrary();
    if (btn.dataset.tab === "profile") loadProfile();
    if (btn.dataset.tab === "history") loadSessions();
    if (btn.dataset.tab === "settings") loadSettings();
  });
});

// ============ 侧栏用量速览 ============
async function loadMini() {
  try {
    const [u, s] = await Promise.all([api("/api/usage"), api("/api/settings")]);
    $("#mini-today").textContent = `¥${u.today_cost_cny.toFixed(2)} / ¥${u.daily_budget_cny}`;
    $("#mini-model").textContent = s.llm.model || "未配置";
  } catch (e) { /* 忽略 */ }
}

// 未同步时对话页顶部引导横幅（dc1076b 重构中误删定义，恢复）
async function checkSyncBanner() {
  try {
    const st = await api("/api/sync/status");
    const b = $("#sync-banner");
    if (!b) return;
    if (!st.owned) {
      b.classList.remove("hidden");
      b.innerHTML =
        "还没有同步过游戏库——到「库存」页填 SteamID64 同步一次即可开始" +
        "（需要 Steam Web API Key，且游戏详情设为公开）。" +
        '<button class="btn primary" id="goto-sync">去同步</button>';
      $("#goto-sync").addEventListener("click", () =>
        document.querySelector('[data-tab="library"]')?.click()
      );
    } else {
      b.classList.add("hidden");
      // 库已就绪：自动「猜你想玩」（内部自判 LLM/引导/开关/页面状态；引导完成后的
      // checkSyncBanner 重调也会走到这里，时机自然衔接）
      autoRecommend();
    }
  } catch (e) { /* 忽略 */ }
}

// ============ 下载带宽（localStorage 持久化，修改即时刷新卡片） ============
function getDownloadSpeed() {
  return parseFloat(localStorage.getItem("dl_speed")) || 100;
}
function setDownloadSpeed(mbps) {
  localStorage.setItem("dl_speed", String(mbps));
  turns.forEach((t) => rebuildTrack(t, true));
}
// 前端本地计算下载时间（改带宽立即刷新，不用重新生成卡片）
function calcEstMinutes(storage_gb) {
  const speed = getDownloadSpeed();
  if (!storage_gb || speed <= 0) return null;
  return Math.round((storage_gb * 8192 / speed) / 60);
}
function formatDuration(min) {
  if (min == null) return "";
  if (min < 60) return `≈${min} 分钟`;
  if (min < 1440) return `≈${(min / 60).toFixed(1)} 小时`;
  return `≈${(min / 1440).toFixed(1)} 天`;
}

// ============ 时段感知（问候语 / 快捷 chips / 输入提示随时间变化） ============
function periodInfo() {
  const h = new Date().getHours();
  if (h >= 5 && h < 11) return { word: "上午", chips: ["来局 30 分钟的", "换个口味", "随便来点"] };
  if (h >= 11 && h < 14) return { word: "中午", chips: ["来局轻松的", "换个口味", "随便来点"] };
  if (h >= 14 && h < 18) return { word: "下午", chips: ["来局轻松的", "换个口味", "随便来点"] };
  if (h >= 18 && h < 23) return { word: "晚上", chips: ["来局轻松的", "换个口味", "随便来点"] };
  return { word: "深夜", chips: ["睡前轻松局", "换个口味", "随便来点"] };
}

function initEmptyPage() {
  const { word, chips } = periodInfo();
  const title = $("#empty-title");
  if (title) title.textContent = `${word}好——这个${word}，玩什么？`;
  const input = $("#chat-input");
  if (input) input.placeholder = `说说这个${word}想玩什么…（Enter 发送，留空=随便来点）`;
  const row = $("#quick-row");
  if (row) {
    row.innerHTML = "";
    for (const c of chips) {
      const b = el("button", "chip-btn", c);
      b.addEventListener("click", () => sendText(c));
      row.appendChild(b);
    }
  }
}
initEmptyPage();

// ============ 对话（纵向 fullpage 整页 + 横向 peek 轮播，CTA 卡滑到即换批） ============
const SESSION = (() => {
  let id = localStorage.getItem("wtp_session");
  if (!id) {
    id = String(Date.now());
    localStorage.setItem("wtp_session", id);
  }
  return id;
})();

let busy = false;
let turns = []; // 每轮对话的轮播状态

// ============ 行为反馈埋点（P1-d：impression/launch/dismiss/skip/download） ============
// fire-and-forget：事件只影响运行时评分（疲劳/即玩倾向/探索模式），失败静默
function feedback(app_id, kind) {
  api("/api/feedback", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ app_id, kind, session_id: SESSION }),
  }).catch(() => {});
}

// 该轮轨道上的全部卡片（跨批次扁平化）
const flatCards = (t) => t.batches.flat();

// 不感兴趣：上报 dismiss → 卡片淡出 → 从轨道移除并重排
function dismissCard(t, app_id) {
  feedback(app_id, "dismiss");
  const slide = [...t.track.querySelectorAll(".slide")].find((s) =>
    s.querySelector(`[data-app="${app_id}"]`) || s.dataset.app === String(app_id)
  );
  if (!slide) return;
  slide.classList.add("dismissing");
  let removedIdx = -1;
  let gi = 0;
  for (const b of t.batches) {
    const i = b.findIndex((c) => c.app_id === app_id);
    if (i >= 0) {
      removedIdx = gi + i;
      b.splice(i, 1);
      break;
    }
    gi += b.length;
  }
  setTimeout(() => {
    if (removedIdx >= 0 && removedIdx < t.flat) t.flat--;
    t.flat = Math.min(t.flat, totalCards(t));
    rebuildTrack(t);
    position(t);
  }, 260);
}

function badgeKind(badge) {
  if (badge === "立即可玩" || badge === "已安装") return "好";
  if (badge === "需更新") return "更新";
  return "默认";
}

// 推荐点标签（Rust 计算的 CardPoint，kind 决定配色）
function pointsHtml(points) {
  const list = points || [];
  if (!list.length) return "";
  return `<div class="points">` + list
    .map((p) => `<span class="pt ${esc(p.kind)}">${esc(p.text)}</span>`)
    .join("") + `</div>`;
}

// rankText：跨批次连续编号由前端计算传入；不传则用卡片自带 rank
function cardHtml(c, rankText) {
  const kind = badgeKind(c.badge);
  const dl = c.download_info;
  let mainAction = "";
  let dlHint = "";
  if (dl) {
    // 未安装：下载按钮 + 空间/时间权衡提示
    const parts = [];
    if (dl.storage_gb) parts.push(`约 ${dl.storage_gb} GB`);
    if (dl.disk_free_gb) parts.push(`盘剩 ${dl.disk_free_gb} GB`);
    const est = calcEstMinutes(dl.storage_gb) ?? dl.est_minutes;
    const dur = formatDuration(est);
    if (dur) parts.push(dur);
    const warn = dl.disk_ok === false ? ' <span class="dl-warn">⚠ 磁盘不足</span>' : "";
    dlHint = parts.length ? `<div class="dl-info">${parts.join(" · ")}${warn}</div>` : "";
    mainAction = `<a href="${esc(dl.install_url)}" class="dl-btn" data-act="download" data-app="${c.app_id}">⬇ 下载</a>`;
  } else {
    mainAction = `<a href="${esc(c.launch_url)}" data-act="launch" data-app="${c.app_id}">▶ 启动游戏</a>`;
  }
  const exploreFlag = c.explore ? '<span class="explore-flag">🎲 换个口味</span>' : "";
  return `<div class="card">
    <div class="cover-wrap">
      <img class="cover" src="${cover(c.app_id)}" loading="lazy" onerror="this.remove()">
      <span class="rank">${rankText || "#" + c.rank}</span>
      <span class="cbadge b-${kind}">${esc(c.badge)}</span>
      ${exploreFlag}
    </div>
    <div class="cbody">
      <div class="card-head"><span class="name">《${esc(c.name)}》</span></div>
      <div class="title">${esc(c.title)}</div>
      <div class="blurb">${esc(c.blurb)}</div>
      ${pointsHtml(c.points)}
      ${dlHint}
      <div class="card-foot"><span>${esc(c.session_hint || "约 " + c.suggested_session_min + " 分钟")}</span>
        <span class="foot-ops">
          <button class="dismiss-btn" data-act="dismiss" data-app="${c.app_id}" title="不想看到它（近期降低权重）">不感兴趣</button>
          ${mainAction}
        </span>
      </div>
    </div>
  </div>`;
}

// ===== 纵向整页（fullpage 式：固定视口；动画中收到新输入立即重定向过渡目标） =====
let pageIndex = 0;
let wheelAcc = 0;
let wheelAccTimer = null;

const pageCount = () => document.querySelectorAll("#pages > .fullpage").length;

function goToPage(i) {
  i = Math.max(0, Math.min(pageCount() - 1, i));
  pageIndex = i;
  $("#pages").style.transform = `translateY(-${i * 100}%)`;
  renderDots();
}

function renderDots() {
  const rail = $("#page-dots");
  rail.innerHTML = "";
  for (let i = 0; i < pageCount(); i++) {
    const d = el("button", "dot" + (i === pageIndex ? " on" : ""));
    d.title = i === 0 ? "首页" : `第 ${i} 轮对话`;
    d.addEventListener("click", () => goToPage(i));
    rail.appendChild(d);
  }
}

// 单步翻页。CSS transition 支持中途改目标值——动画进行中再次触发时，
// 页面从当前帧平滑续滑向新目标，多页连滑是一段连续动画而非多段拼接。
function stepPage(dir) {
  const target = Math.max(0, Math.min(pageCount() - 1, pageIndex + dir));
  if (target === pageIndex) return;
  pageIndex = target;
  $("#pages").style.transform = `translateY(-${pageIndex * 100}%)`;
  renderDots();
}

// ===== 横向轮播（中置 + 两侧露出相邻卡；轨道末尾 CTA 卡，滑到即发起换批） =====

const totalCards = (t) => t.batches.reduce((a, b) => a + b.length, 0);

function rebuildTrack(t, instant) {
  let html = "";
  if (t.generating) {
    // 首批生成中：占位卡与推荐卡同位同高，完成后原位替换
    html = `<div class="slide gen-slide"><div class="gen-box"><span class="spin"></span><span>正在为你挑游戏…</span></div></div>`;
  } else {
    let gi = 0;
    for (const b of t.batches) {
      for (const c of b) {
        html += `<div class="slide" data-app="${c.app_id}">${cardHtml(c, "#" + (gi + 1))}</div>`;
        gi++;
      }
    }
    if (!html) {
      // 无候选：CTA 三态——可放宽（带品类条件且未放宽）/ 真尽头（已放宽）/ 重试
      if (t.relaxable) {
        html = `<div class="slide cta-slide only"><button class="cta relax">符合条件的都看完了<br><span class="cta-ic">⤢</span> 放宽条件，看看其他游戏</button></div>`;
      } else if (t.relaxed) {
        html = `<div class="slide cta-slide only"><div class="cta dead">整个库都看完了<br><span class="cta-ic">🎲</span> 换个说法再试试</div></div>`;
      } else {
        html = `<div class="slide cta-slide only"><button class="cta">没有符合条件的候选<br><span class="cta-ic">↻</span> 再试一次</button></div>`;
      }
    } else {
      // 轨道末尾的「换一批」CTA 卡：滑到它即自然发起下一批请求
      html += `<div class="slide cta-slide"><button class="cta" title="拉取下一批推荐">换一批<br><span class="cta-ic">↻</span></button></div>`;
    }
  }
  t.track.innerHTML = html;
  [...t.track.children].forEach((s, i) => {
    s.addEventListener("click", (e) => {
      if (e.target.closest("a")) return;
      const isCta = !t.generating && i === totalCards(t);
      if (i !== t.flat) {
        t.flat = i;
        position(t);
      }
      if (isCta) {
        // 空轨道上的 CTA：可放宽 → 发起放宽；真尽头 → 无动作；默认 → 重试
        const ctaEl = t.track.querySelector(".cta");
        if (ctaEl && ctaEl.classList.contains("relax")) fetchNextBatch(t, "放宽条件");
        else if (ctaEl && ctaEl.classList.contains("dead")) { /* 尽头，无动作 */ }
        else fetchNextBatch(t);
      }
    });
  });
  position(t, instant);
}

function position(t, instant) {
  const slides = [...t.track.children];
  if (!slides.length) return;
  const gap = 24;
  const w = slides[0].offsetWidth;
  const offset = t.viewport.clientWidth / 2 - (t.flat * (w + gap) + w / 2);
  if (instant) {
    // 首次定位不做滑动动画（避免"跳一下再滑"），原地出现
    t.track.style.transition = "none";
    t.track.style.transform = `translateX(${offset}px)`;
    void t.track.offsetHeight; // 强制 reflow 后恢复过渡
    t.track.style.transition = "";
  } else {
    t.track.style.transform = `translateX(${offset}px)`;
  }
  slides.forEach((s, i) => s.classList.toggle("current", i === t.flat));
  const total = totalCards(t);
  const atCta = !t.generating && t.flat === total;
  // 轮播中心位 → 曝光事件（服务端按 卡×会话 去重；seen 供换批时的 skip 批量上报）
  if (!t.generating && !atCta) {
    const card = flatCards(t)[t.flat];
    if (card) {
      feedback(card.app_id, "impression");
      t.seen.add(card.app_id);
    }
  }
  t.prev.disabled = t.loading || t.generating || t.flat <= 0;
  t.next.disabled = t.loading || t.generating;
  t.info.textContent = t.generating
    ? "正在为你挑游戏…"
    : atCta
      ? (t.loading ? "正在获取下一批…"
        : t.lastEmpty
          ? (t.relaxable ? "符合条件的都看完了 · 点 CTA 放宽条件"
            : t.relaxed ? "整个库都看完了 · 换个说法试试" : "没有符合条件的候选 · 点 CTA 卡重试")
          : "已看完本批推荐 · CTA 卡可再来一批")
      : `第 ${t.flat + 1} 张 · 共 ${total} 张`;
}

function handleNav(t, d) {
  if (t.loading || t.generating) return;
  const total = totalCards(t);
  if (d < 0) {
    if (t.flat > 0) {
      t.flat--;
      position(t);
    }
  } else if (t.flat < total) {
    // 滑向下一张；滑到末尾 CTA 卡的动作本身触发请求
    t.flat++;
    position(t);
    if (t.flat === total) fetchNextBatch(t);
  }
}

async function streamAsk(message, t) {
  await sseStream("/api/ask", { message, session_id: SESSION }, (ev) => {
    if (ev.type === "trace") {
      t.trace.appendChild(el("div", "trace", ev.payload.text));
      t.trace.scrollTop = t.trace.scrollHeight;
    } else if (ev.type === "cards") {
      t.generating = false;
      t.relaxable = !!ev.payload.relaxable;
      t.relaxed = !!ev.payload.relaxed;
      if (ev.payload.empty) {
        t.lastEmpty = true;
        rebuildTrack(t, true); // 原位显示 CTA（放宽 / 尽头 / 重试 三态）
      } else {
        t.lastEmpty = false;
        const first = t.batches.length === 0;
        const start = totalCards(t);
        t.batches.push(ev.payload.cards);
        t.flat = start; // 新批第一张
        rebuildTrack(t, first); // 首批原位替换占位卡；换批保留滑动动画
      }
    } else if (ev.type === "done") {
      t.trace.appendChild(
        el("div", "trace", `本轮 ${ev.payload.calls} 次调用 · ${ev.payload.prompt_tokens} 入 / ${ev.payload.completion_tokens} 出 tokens`)
      );
      t.trace.scrollTop = t.trace.scrollHeight;
    } else if (ev.type === "error") {
      t.info.textContent = "⚠ " + ev.payload.message;
    }
  });
}

// 一轮对话 = 一个整页；页内推荐轨道中置轮播，末尾 CTA 卡滑到即换批
function newTurnPage(text) {
  $("#chat-empty")?.remove();
  const page = el("div", "fullpage turn-page");
  const urow = el("div", "turn-user");
  urow.appendChild(el("span", "bubble", text));
  const trace = el("div", "traces");
  const viewport = el("div", "carousel");
  const track = el("div", "track");
  viewport.appendChild(track);
  const prev = el("button", "nav prev");
  prev.textContent = "‹";
  prev.title = "上一张";
  const next = el("button", "nav next");
  next.textContent = "›";
  next.title = "下一张（本批最后一张后为 CTA 卡）";
  const stage = el("div", "stage");
  stage.append(prev, viewport, next);
  const info = el("div", "stage-info");
  page.append(urow, trace, stage, info);
  $("#pages").appendChild(page);
  const t = {
    page, trace, viewport, track, prev, next, info,
    batches: [], flat: 0, loading: false,
    generating: true, // 首批生成中：轨道显示占位卡
    lastEmpty: false,
    seen: new Set(),     // 本轮到过中心位的卡（换批时未启动的批量上报 skip）
    launched: new Set(), // 本轮启动过的卡（不参与 skip）
  };
  prev.addEventListener("click", () => handleNav(t, -1));
  next.addEventListener("click", () => handleNav(t, 1));
  // 触控板横向滑动直接驱动轮播
  stage.addEventListener("wheel", (e) => {
    if (Math.abs(e.deltaX) > Math.abs(e.deltaY) && Math.abs(e.deltaX) > 20) {
      e.preventDefault();
      e.stopPropagation();
      handleNav(t, e.deltaX > 0 ? 1 : -1);
    }
  }, { passive: false });
  turns.push(t);
  rebuildTrack(t, true); // 占位卡内容就绪
  renderDots();
  // 等一帧让布局完成后再定位（否则 offsetWidth=0，占位卡尺寸/位置错误）
  requestAnimationFrame(() => {
    position(t, true);
    goToPage(pageCount() - 1);
  });
  return t;
}

async function fetchNextBatch(t, msg) {
  if (busy || t.loading) return;
  busy = true;
  t.loading = true;
  // 换批：看过未启动的卡批量上报 skip（弱负信号 → 疲劳惩罚；启动过的豁免）
  for (const id of t.seen) {
    if (!t.launched.has(id)) feedback(id, "skip");
  }
  t.seen.clear();
  t.launched.clear();
  const cta = t.track.querySelector(".cta");
  if (cta) cta.innerHTML = '<span class="spin"></span> 正在生成…';
  position(t);
  $("#chat-send").disabled = true;
  await streamAsk(msg || "换一批", t);
  t.loading = false;
  position(t); // 空结果时刷新 info（恢复 CTA 默认态由 rebuild 负责）
  if (!t.lastEmpty) {
    rebuildTrack(t); // 新卡已插入，恢复 CTA 默认态并校正位置
    position(t);
  }
  busy = false;
  $("#chat-send").disabled = false;
  loadMini();
}

// 卡片操作埋点（事件委托：启动 → launch 强正、下载 → download 中正、不感兴趣 → dismiss）
$("#pages").addEventListener("click", (e) => {
  const target = e.target.closest("[data-act]");
  if (!target) return;
  const app_id = Number(target.dataset.app);
  const act = target.dataset.act;
  const page = target.closest(".fullpage");
  const t = page ? turns.find((x) => x.page === page) : null;
  if (act === "dismiss") {
    e.preventDefault();
    if (t) dismissCard(t, app_id);
  } else if (act === "launch" || act === "download") {
    feedback(app_id, act);
    if (t) t.launched.add(app_id);
  }
});

// actual 可选：气泡显示 text、实际发给后端 actual（「猜你想玩」显示口语化文案、走「随便来点」空筛选触发词）
async function sendText(text, actual) {
  if (!text || busy) return;
  busy = true;
  $("#chat-send").disabled = true;
  // 新消息 = 离开当前轨道：看过未启动的卡与「换一批」一样计为 skip（弱负信号 → 疲劳惩罚）
  for (const t of turns) {
    for (const id of t.seen) {
      if (!t.launched.has(id)) feedback(id, "skip");
    }
    t.seen.clear();
    t.launched.clear();
  }
  const t = newTurnPage(text);
  await streamAsk(actual || text, t);
  busy = false;
  $("#chat-send").disabled = false;
  loadMini();
}

// ===== 打开页面自动「猜你想玩」（默认开，设置页可关；每次页面加载至多发一次）=====
let autoRecommendDone = false;
async function autoRecommend() {
  if (autoRecommendDone || busy) return;
  try {
    const b = await api("/api/bootstrap");
    // 无 LLM key / 设置已关：一次性放弃（条件不会自行变化）
    if (!b.llm_key_set || b.auto_recommend === false) {
      autoRecommendDone = true;
      return;
    }
    // 用户已自己开聊 / 首页已被替换：不打扰
    if (!$("#chat-empty") || turns.length) return;
    // 引导向导还在屏上：等它 complete() 后重调的 checkSyncBanner 再来
    const onb = $("#onboarding");
    if (onb && !onb.classList.contains("hidden")) return;
    if (!$("#tab-chat").classList.contains("active")) return;
    autoRecommendDone = true;
    await sendText("猜你想玩", "随便来点");
  } catch (e) {
    autoRecommendDone = true; // bootstrap 拉取失败也只试一次，保留空态
  }
}

async function send() {
  const input = $("#chat-input");
  let text = input.value.trim();
  if (!text || busy) {
    if (busy) return;
    text = "随便来点"; // 空输入 = 默认推荐（时段感知入口之一）
  }
  input.value = "";
  input.blur(); // 发送后释放焦点，方向键即可驱动轮播/翻页
  await sendText(text);
}

$("#chat-form").addEventListener("submit", (e) => { e.preventDefault(); send(); });

// 键盘：←→ 驱动当前可见页对应轮次的轮播，↑↓ 切换轮次页
function currentPageTurn() {
  const homeGone = !$("#chat-empty"); // 首页移除后页号与 turns 下标一一对应
  const idx = pageIndex - (homeGone ? 0 : 1);
  return idx >= 0 ? turns[idx] || null : null;
}

document.addEventListener("keydown", (e) => {
  const tag = (e.target && e.target.tagName) || "";
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  if (!$("#tab-chat").classList.contains("active") || !turns.length) return;
  const t = currentPageTurn();
  if (!t) return;
  if (e.key === "ArrowRight") handleNav(t, 1);
  else if (e.key === "ArrowLeft") handleNav(t, -1);
  else if (e.key === "ArrowDown") stepPage(1);
  else if (e.key === "ArrowUp") stepPage(-1);
});

// 固定视口：滚轮整页切换。一次拨动（含惯性余量）只翻一页：翻页后进入
// 250ms 冷却，冷却期内丢弃滚轮输入，防止一连滚滑好几页。
let pageCooldownUntil = 0;
$("#chat-viewport").addEventListener("wheel", (e) => {
  if (Math.abs(e.deltaX) >= Math.abs(e.deltaY)) return;
  e.preventDefault();
  if (Date.now() < pageCooldownUntil) return;
  wheelAcc += e.deltaY;
  if (wheelAcc > 50) {
    stepPage(1);
    wheelAcc = 0;
    pageCooldownUntil = Date.now() + 250;
  } else if (wheelAcc < -50) {
    stepPage(-1);
    wheelAcc = 0;
    pageCooldownUntil = Date.now() + 250;
  }
  clearTimeout(wheelAccTimer);
  wheelAccTimer = setTimeout(() => (wheelAcc = 0), 200);
}, { passive: false });

window.addEventListener("resize", () => turns.forEach((t) => position(t)));

// ============ 库存 ============
const LIB_INPUT_STYLE = "flex:1;min-width:260px;background:var(--bg3);border:1px solid var(--border);color:var(--fg);padding:8px 10px;border-radius:8px;font-size:13px";
async function loadLibrary() {
  const body = $("#library-body");
  try {
    const lib = await api("/api/library");
    if (!lib.total) {
      renderLibraryEmpty(body);
      return;
    }
    const when = lib.last_sync ? new Date(Number(lib.last_sync) * 1000).toLocaleString("zh-CN") : "从未";
    let html = `
    <div class="lib-head">
      <div><b style="font-size:17px">游戏库存</b> <span class="muted">共 ${lib.total} 款 · 上次同步：${when}</span></div>
      <div style="display:flex;gap:10px;align-items:center">
        <button class="btn" id="lib-local" title="只重新扫描本机 Steam 的安装/更新状态，不联网">⌂ 更新本地状态</button>
        <button class="btn primary" id="lib-sync">⟳ 更新游戏库</button>
        <span class="muted hidden" id="lib-syncing">同步中…</span>
      </div>
    </div>
    <div class="muted" style="margin:6px 0 2px">点击游戏左上角的深度角标可手动标注（无成就游戏标「已完成」、长线游戏标「暂离」——手动标注永远优先于自动判定）</div>
    <div id="lib-sync-log" class="hidden"></div>`;
    // 识别异常管理：显式列出自动识别可能有问题的游戏（数据类可自动修复，判断类快捷手动标注）
    const anomalies = lib.anomalies || [];
    if (anomalies.length) {
      const judge = anomalies.filter((a) => a.kind === "suspect_finished");
      const dataKind = anomalies.length - judge.length;
      html += `<div class="panel anomaly-panel">
        <div style="display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap">
          <h4 style="margin:0">⚠ 识别异常（${anomalies.length}）</h4>
          <button class="btn primary" id="lib-repair" ${dataKind ? "" : "disabled title='没有可自动修复的数据异常，用列表中的手动标注处理'"}>⟳ 尝试自动修复${dataKind ? `（${dataKind} 项数据缺失）` : ""}</button>
        </div>
        <div class="muted" style="margin:6px 0 10px">数据缺失的重新联网拉取；疑似漏识别的用右侧按钮手动标注，标注后不再提示。</div>
        <div id="lib-repair-log" class="hidden"></div>
        <div class="anomaly-list">`;
      for (const a of anomalies) {
        const quick = a.kind === "suspect_finished"
          ? `<span class="ops">
              <button class="btn green" data-app="${a.app_id}" data-act="mark-done" data-depth="已完成">已玩完</button>
              <button class="btn" data-app="${a.app_id}" data-act="ignore-anomaly">没玩完</button>
             </span>`
          : `<span class="muted">点上方「尝试自动修复」</span>`;
        html += `<div class="anomaly-item"><div class="txt"><b>《${esc(a.name)}》</b> ${esc(a.hint)}</div>${quick}</div>`;
      }
      html += `</div></div>`;
    }
    html += `<div class="lib-grid">`;
    for (const g of lib.games) {
      const dim = g.excluded ? " dimmed" : "";
      const comp = g.completion !== null && g.completion !== undefined ? `<span>成就 ${g.completion}%</span>` : "";
      html += `
      <div class="lib-item${dim}" title="${esc(g.excluded || "")}">
        <div class="lib-cover-wrap">
          <img class="lib-cover" src="${cover(g.app_id)}" loading="lazy" onerror="this.remove()">
          <span class="depth-chip d-${g.depth}${g.depth_override ? " manual" : ""}" data-app="${g.app_id}" data-depth="${esc(g.depth)}" title="点击手动标注深度">${esc(g.depth)}${g.depth_override ? " ✎" : ""}</span>
        </div>
        <div class="lib-name">${esc(g.name)}</div>
        <div class="lib-meta">
          <span>${g.hours} 小时</span>
          ${comp}
          <span class="badge b-${badgeKind(g.badge)}">${esc(g.badge)}</span>
        </div>
      </div>`;
    }
    html += `</div>`;
    body.innerHTML = html;
    $("#lib-sync").addEventListener("click", () => runLibrarySync());
    // 本地状态刷新（零网络、秒级）：local_only 同步——装了/卸了/更新了 Steam 游戏后即时反映
    $("#lib-local").addEventListener("click", async () => {
      const btn = $("#lib-local");
      btn.disabled = true;
      btn.textContent = "⌂ 扫描中…";
      try {
        await sseStream("/api/sync/start", { local_only: true }, () => {});
      } catch (e) { /* 静默，下面重渲染反映真实状态 */ }
      btn.disabled = false;
      btn.textContent = "⌂ 更新本地状态";
      loadLibrary();
    });
    const repairBtn = $("#lib-repair");
    if (repairBtn) {
      repairBtn.addEventListener("click", async () => {
        const log = $("#lib-repair-log");
        log.classList.remove("hidden");
        log.textContent = "开始修复…";
        repairBtn.disabled = true;
        await sseStream("/api/repair", {}, (ev) => {
          if (ev.type === "progress") {
            log.textContent += "\n" + ev.payload.text;
          } else if (ev.type === "done") {
            log.textContent += "\n✔ " + ev.payload.message;
          } else if (ev.type === "error") {
            log.textContent += "\n✖ " + ev.payload.message;
          }
          log.scrollTop = log.scrollHeight;
        });
        repairBtn.disabled = false;
        loadLibrary(); // 修复后异常清单/深度即时刷新
      });
    }
    // 疑似漏识别的快捷处理：「已玩完」= 手动标已完成；「没玩完」= 忽略提示（深度保持自动判定）
    body.querySelectorAll(".anomaly-item .ops button").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const app_id = Number(btn.dataset.app);
        const body =
          btn.dataset.act === "ignore-anomaly"
            ? { app_id, kind: "anomaly_done", value: "done" }
            : { app_id, kind: "depth", value: btn.dataset.depth };
        await api("/api/override", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        });
        loadLibrary();
      });
    });
    bindDepthMenu(body, lib.depth_options || []);
  } catch (e) {
    body.innerHTML = `<div class="error-note">加载失败：${esc(e.message)}</div>`;
  }
}

// ===== 深度角标手动标注（修正层：manual_overrides kind=depth，永远覆盖自动判定）=====
let depthMenuBound = false;
let depthOptionsCache = [];
function closeDepthMenu() {
  document.querySelectorAll(".depth-menu").forEach((m) => m.remove());
}
function bindDepthMenu(body, options) {
  depthOptionsCache = options;
  if (depthMenuBound) return;
  depthMenuBound = true;
  body.addEventListener("click", (e) => {
    const chip = e.target.closest(".depth-chip");
    if (!chip) return;
    e.stopPropagation();
    closeDepthMenu();
    const appId = Number(chip.dataset.app);
    const current = chip.dataset.depth;
    const menu = el("div", "depth-menu");
    for (const opt of [...depthOptionsCache, "恢复自动"]) {
      const item = el("button", "depth-opt" + (opt === current ? " on" : ""), opt);
      item.addEventListener("click", async (ev) => {
        ev.stopPropagation();
        const value = opt === "恢复自动" ? "" : opt;
        try {
          await api("/api/override", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ app_id: appId, kind: "depth", value }),
          });
        } catch (err) { /* 静默：重渲染仍显示当前真实状态 */ }
        closeDepthMenu();
        loadLibrary();
      });
      menu.appendChild(item);
    }
    document.body.appendChild(menu);
    const r = chip.getBoundingClientRect();
    menu.style.left = Math.min(r.left, window.innerWidth - 150) + "px";
    menu.style.top = (r.bottom + 6) + "px";
    setTimeout(() => document.addEventListener("click", closeDepthMenu, { once: true }), 0);
  });
}

// 空库不是死胡同：直接给出可操作的同步面板（本机检测不到账号时必须手填 SteamID64）
function renderLibraryEmpty(body) {
  body.innerHTML = `
  <div class="panel" style="max-width:660px;margin-top:24px">
    <h4>同步你的 Steam 游戏库</h4>
    <div class="muted" style="line-height:1.8">
      库为空——先同步一次才能生成画像与推荐。开始前确认：
      ① 已在「设置 → API 密钥与模型」填好 Steam Web API Key；
      ② Steam 个人资料 → 隐私设置中「我的资料」与「游戏详情」均为<b>公开</b>。
    </div>
    <div style="display:flex;gap:10px;margin-top:14px;flex-wrap:wrap">
      <input id="lib-steamid" style="${LIB_INPUT_STYLE}" placeholder="SteamID64（17 位数字；本机检测不到 Steam 账号时必填）">
      <button class="btn primary" id="lib-sync">⟳ 开始同步</button>
      <span class="muted hidden" id="lib-syncing">同步中…</span>
    </div>
    <div id="lib-sync-log" class="hidden"></div>
    <div class="muted" style="margin-top:8px">
      SteamID64 在哪看：Steam 个人资料页地址 …/profiles/ 后面的 17 位数字。首次同步约 2–5 分钟，之后增量秒级。
    </div>
  </div>`;
  $("#lib-sync").addEventListener("click", () => runLibrarySync());
}

async function runLibrarySync() {
  const log = $("#lib-sync-log");
  const steamid = $("#lib-steamid") ? $("#lib-steamid").value.trim() : "";
  log.classList.remove("hidden");
  log.textContent = "开始同步…";
  $("#lib-sync").disabled = true;
  $("#lib-syncing").classList.remove("hidden");
  let failed = false;
  await sseStream("/api/sync/start", { steamid: steamid || null }, (ev) => {
    if (ev.type === "progress") {
      log.textContent += "\n" + ev.payload.text;
    } else if (ev.type === "done") {
      log.textContent += "\n✔ " + ev.payload.message;
    } else if (ev.type === "error") {
      failed = true;
      log.textContent += "\n✖ " + ev.payload.message;
    }
    log.scrollTop = log.scrollHeight;
  });
  $("#lib-sync").disabled = false;
  $("#lib-syncing").classList.add("hidden");
  // 失败时保留日志与错误原因（重渲染会把它们吞掉）；成功才刷新库存视图
  if (!failed) loadLibrary();
  checkSyncBanner();
}

// ============ 画像 ============
async function loadProfile() {
  const body = $("#profile-body");
  try {
    const p = await api("/api/profile");
    if (!p.total_games) {
      body.innerHTML = '<div class="muted">库为空：请先在「游戏库存」页同步。</div>';
      return;
    }
    const axes = [
      ["成就完成", p.axes.achiever],
      ["探索发现", p.axes.explorer],
      ["竞争对抗", p.axes.killer],
      ["社交合作", p.axes.socializer],
    ];
    const mainAxis = axes.reduce((a, b) => (b[1] > a[1] ? b : a));
    const html = [];
    html.push(`<div class="muted">${p.total_games} 款游戏 · 有效时长 ${p.effective_hours} 小时 · 主型倾向 <b style="color:var(--accent)">${mainAxis[0]}型</b> · 稀有成就 ${p.rare_achievements_owned} 枚</div>`);
    html.push(`<h3 class="sec">Bartle 四维</h3><div class="axes">`);
    for (const [name, v] of axes) {
      html.push(`<div class="axis"><span>${name}</span><div class="track"><div class="fill" style="width:${(v * 100).toFixed(0)}%"></div></div><span class="val">${v.toFixed(2)}</span></div>`);
    }
    html.push(`</div>`);
    html.push(`<h3 class="sec">深度分布</h3><div class="chips">`);
    for (const [k, v] of Object.entries(p.depth_counts)) {
      html.push(`<span class="chip depth">${esc(k)} <b>${v}</b></span>`);
    }
    html.push(`</div>`);
    html.push(`<h3 class="sec">行为特征</h3><div class="muted">活跃度 ${p.behavior.activity.toFixed(2)} · 深度 ${p.behavior.depth.toFixed(2)} · 广度 ${p.behavior.breadth.toFixed(2)} · 典型会话 ~${p.behavior.typical_session_min} 分钟 · 积压率 ${p.behavior.backlog_ratio.toFixed(2)}</div>`);
    if (p.tag_weights.length) {
      html.push(`<h3 class="sec">品类偏好（Top 20 / 全 ${p.tag_weights.length} 项）</h3><div class="chips">`);
      for (const [k, v] of p.tag_weights) {
        html.push(`<span class="chip">${esc(k)} <b>${v.toFixed(2)}</b></span>`);
      }
      html.push(`</div>`);
    }
    if (p.evidence.length) {
      html.push(`<h3 class="sec">证据</h3><ul class="plain">`);
      for (const e of p.evidence) html.push(`<li>${esc(e)}</li>`);
      html.push(`</ul>`);
    }
    if (p.idle_proposals.length) {
      html.push(`<h3 class="sec">疑似注水提议（机器提议 · 你来确认，修正层永远覆盖自动判定）</h3>`);
      for (const it of p.idle_proposals) {
        const done = it.status !== "proposed";
        html.push(`<div class="proposal" data-app="${it.app_id}">
          <div><b>《${esc(it.name)}》</b>：${esc(it.evidence)}</div>
          <div class="ops">${done
            ? `<span class="muted">已${it.status === "confirmed" ? "确认（剔除出画像）" : "否决（保留在画像）"}</span>`
            : `<button class="btn green" data-act="confirmed">确认注水</button><button class="btn red" data-act="rejected">不是挂机</button>`}
          </div></div>`);
      }
    }
    if (p.taste_excludes && p.taste_excludes.length) {
      html.push(`<h3 class="sec">口味排除（对话中说"记住我不喜欢 X"即生效 · 点击撤销）</h3><div class="chips">`);
      for (const t of p.taste_excludes) {
        html.push(`<span class="chip taste-exclude" data-tag="${esc(t.tag)}" title="点击撤销排除">${esc(t.tag)} ✕</span>`);
      }
      html.push(`</div>`);
    }
    if (p.behavior_feedback) {
      const bf = p.behavior_feedback;
      const aff = bf.install_affinity;
      const affLabel = aff >= 0.65 ? "明显偏好即开即玩" : aff <= 0.35 ? "不介意先下载再玩" : "中性";
      const ev = bf.events;
      html.push(`<h3 class="sec">行为反馈（运行时微调 · 不回写基础画像）</h3>
      <div class="panel" style="max-width:760px">
        <div class="bf-row"><span class="bf-k">即玩倾向</span>
          <div class="axis" style="grid-template-columns:1fr 150px"><div class="track"><div class="fill" style="width:${(aff * 100).toFixed(0)}%"></div></div><span class="val">${aff.toFixed(2)} · ${affLabel}</span></div>
        </div>
        <div class="bf-row"><span class="bf-k">疲劳降权中</span><span>${bf.fatigue_apps} 款（近期被跳过/不感兴趣，启动后自动清零）</span></div>
        <div class="bf-row"><span class="bf-k">事件计数</span><span>启动 ${ev.launch} · 不感兴趣 ${ev.dismiss} · 跳过 ${ev.skip} · 下载 ${ev.download} · 曝光 ${ev.impression}</span></div>
        <div class="bf-row" style="align-items:center"><span class="bf-k"></span>
          <button class="btn red" id="bf-reset">重置行为学习</button>
          <span class="muted" style="margin-left:10px">清空事件并把即玩倾向恢复 0.5</span>
        </div>
      </div>`);
    }
    if (p.excluded.length) {
      html.push(`<h3 class="sec">剔除名单（误剔可点「这是游戏」找回）</h3><ul class="plain">`);
      for (const e of p.excluded) {
        const isNongame = e.reason && e.reason.includes("非游戏");
        html.push(`<li>《${esc(e.name)}》（${esc(e.reason)}）${
          isNongame && e.app_id
            ? ` <button class="btn" data-app="${e.app_id}" data-act="force-game">这是游戏</button>`
            : ""
        }</li>`);
      }
      html.push(`</ul>`);
    }
    body.innerHTML = html.join("");
    body.querySelectorAll('[data-act="force-game"]').forEach((btn) => {
      btn.addEventListener("click", async () => {
        await api("/api/override", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ app_id: Number(btn.dataset.app), kind: "game", value: "force" }),
        });
        loadProfile();
      });
    });
    body.querySelectorAll(".proposal button").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const app_id = Number(btn.closest(".proposal").dataset.app);
        await api("/api/annotation", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ app_id, status: btn.dataset.act }),
        });
        loadProfile();
      });
    });
    body.querySelectorAll(".taste-exclude").forEach((chip) => {
      chip.addEventListener("click", async () => {
        await api("/api/annotation", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ app_id: 0, status: "rejected", revoke_tag: chip.dataset.tag }),
        });
        loadProfile();
      });
    });
    const bfReset = body.querySelector("#bf-reset");
    if (bfReset) {
      bfReset.addEventListener("click", async () => {
        await api("/api/feedback/reset", { method: "POST" });
        loadProfile();
      });
    }
  } catch (e) {
    body.innerHTML = `<div class="error-note">加载失败：${esc(e.message)}</div>`;
  }
}

// ============ 历史 ============
async function loadSessions() {
  const list = $("#session-list");
  const view = $("#session-view");
  view.classList.add("hidden");
  list.classList.remove("hidden");
  try {
    const { sessions } = await api("/api/sessions");
    if (!sessions.length) {
      list.innerHTML = '<div class="muted">还没有会话记录。去「对话推荐」聊一轮吧。</div>';
      return;
    }
    list.innerHTML = "";
    for (const s of sessions) {
      const t = new Date((s.meta.created_at || 0) * 1000);
      const item = el("div", "session-item");
      item.innerHTML = `<div>${t.toLocaleString("zh-CN")}</div><div class="meta">${s.meta.turns} 轮对话 ›</div>`;
      item.addEventListener("click", () => showSession(s.id));
      list.appendChild(item);
    }
  } catch (e) {
    list.innerHTML = `<div class="error-note">加载失败：${esc(e.message)}</div>`;
  }
}

async function showSession(id) {
  const list = $("#session-list");
  const view = $("#session-view");
  try {
    const s = await api("/api/sessions/" + id);
    list.classList.add("hidden");
    view.classList.remove("hidden");
    const back = el("button", "btn", "‹ 返回列表");
    back.addEventListener("click", loadSessions);
    view.innerHTML = "";
    view.appendChild(back);
    for (const turn of s.turns || []) {
      const t = el("div", "turn");
      t.innerHTML = `<div class="u">你：${esc(turn.user)}</div><div class="i">${esc(turn.intent)}</div>` +
        (turn.cards || []).map(cardHtml).join("");
      view.appendChild(t);
    }
  } catch (e) {
    view.innerHTML = `<div class="error-note">加载失败：${esc(e.message)}</div>`;
  }
}

// ============ 设置 ============
async function loadSettings() {
  const body = $("#settings-body");
  try {
    const [s, u] = await Promise.all([api("/api/settings"), api("/api/usage")]);
    const llmOptions = s.available
      .map((n) => `<option value="${esc(n)}" ${n === s.active_llm ? "selected" : ""}>${esc(n)}</option>`)
      .join("");
    const inputStyle = "background:var(--bg3);border:1px solid var(--border);color:var(--fg);padding:8px 10px;border-radius:8px;font-size:13px;width:100%;box-sizing:border-box";
    body.innerHTML = `
    <div class="panel" style="max-width:940px">
      <h4>API 密钥与模型（OpenAI 兼容）</h4>
      <div class="muted" id="keys-status">查询中…</div>
      <div class="grid2" style="margin-top:10px">
        <div class="field"><label>Steam Web API Key（<a href="https://steamcommunity.com/dev/apikey" target="_blank" rel="noopener">steamcommunity.com/dev/apikey</a> 免费申请）</label>
          <input type="password" id="set-steam-key" style="${inputStyle}" autocomplete="off" placeholder="留空 = 不修改"></div>
        <div class="field"><label>LLM API Key（DeepSeek 或任意 OpenAI 兼容服务）</label>
          <input type="password" id="set-llm-key" style="${inputStyle}" autocomplete="off" placeholder="留空 = 不修改"></div>
        <div class="field"><label>服务名称（自定义服务起个名字，如 kimi / glm；留空沿用默认）</label>
          <input id="set-llm-name" style="${inputStyle}" value="${esc(s.active_llm || "")}" placeholder="deepseek"></div>
        <div class="field"><label>密钥变量名（自定义服务写到 .env 的变量名；留空沿用默认 ${esc(s.llm_key_env || "DEEPSEEK_API_KEY")}）</label>
          <input id="set-key-env" style="${inputStyle}" value="${esc(s.llm_key_env || "")}" placeholder="DEEPSEEK_API_KEY"></div>
        <div class="field"><label>接口地址 base_url</label>
          <input id="set-base-url" style="${inputStyle}" value="${esc(s.base_url || "")}" placeholder="https://api.deepseek.com/v1"></div>
        <div class="field"><label>模型名 model</label>
          <input id="set-model" style="${inputStyle}" value="${esc(s.llm.model || "")}" placeholder="deepseek-chat"></div>
      </div>
      <div class="grid2" style="margin-top:2px">
        <div class="field"><label>价格 · 输入（元/百万 tokens，缓存未命中）</label>
          <input type="number" id="set-price-in" style="${inputStyle}" step="0.05" min="0" value="${s.price_input ?? ""}" placeholder="内置价格表自动"></div>
        <div class="field"><label>价格 · 输入缓存命中（元/百万 tokens；无缓存计费可留空）</label>
          <input type="number" id="set-price-cache" style="${inputStyle}" step="0.05" min="0" value="${s.price_cache ?? ""}" placeholder="留空 = 按全价保守计"></div>
        <div class="field"><label>价格 · 输出（元/百万 tokens）</label>
          <input type="number" id="set-price-out" style="${inputStyle}" step="0.05" min="0" value="${s.price_output ?? ""}" placeholder="内置价格表自动"></div>
      </div>
      <div style="display:flex;gap:12px;align-items:center;flex-wrap:wrap">
        <button class="btn primary" id="set-keys-save">保存密钥与端点</button>
        <button class="btn" id="set-price-reset">恢复内置价格</button>
        <span class="muted" id="keys-msg">密钥只写入本机 .env，不进入任何对话内容；保存后即时生效，无需重启。价格留空 = 用内置表（快照 ${esc(u.price_note.includes("快照") ? "官方定价页" : "")}），输入/输出都填写后自定义价格生效。</span>
      </div>
    </div>
    <div class="grid2" style="margin-top:16px">
      <div class="panel">
        <h4>用量与预算（R6）</h4>
        <div class="stat-grid">
          <div class="stat"><div class="v">${u.calls}</div><div class="k">调用次数</div></div>
          <div class="stat"><div class="v">${(u.prompt_tokens + u.completion_tokens).toLocaleString()}</div><div class="k">tokens 总量</div></div>
          <div class="stat"><div class="v">¥${u.cost_cny === null ? "—" : u.cost_cny.toFixed(4)}</div><div class="k">累计费用</div></div>
          <div class="stat"><div class="v">¥${u.today_cost_cny.toFixed(4)}</div><div class="k">今日 / 预算 ¥${u.daily_budget_cny}</div></div>
        </div>
        <div class="muted" style="margin-top:10px">定价来源：${esc(u.price_note)}。今日费用达到预算后，调用将被拒绝（预算硬停）。${u.unpriced_calls ? `<br><b style="color:var(--orange)">另有 ${u.unpriced_calls} 次调用未计价（模型不在价格表且未配置价格），未计入费用与预算——可在上方填写价格。</b>` : ""}</div>
      </div>
      <div class="panel">
        <h4>模型与定位（R3）</h4>
        <div class="field"><label>当前模型</label><select id="set-llm">${llmOptions}</select></div>
        <div class="check"><input type="checkbox" id="set-pos" ${s.llm_positioning ? "checked" : ""}><label for="set-pos">LLM 游戏定位增强（生成画像/推荐时融合，一游戏一次缓存）</label></div>
        <div class="field"><label>日预算（CNY，0 = 不限）</label><input type="number" id="set-budget" step="0.5" min="0" value="${s.daily_budget_cny}"></div>
        <div class="field"><label>下载带宽参考（Mbps，用于估算下载时间）</label><input type="number" id="set-dl-speed" step="10" min="1" value="${getDownloadSpeed()}"></div>
        <div class="field"><label>探索感：<b id="set-rand-val">${Math.round((s.randomness ?? 0) * 100)}%</b>（0 = 每次同一榜单，越高换口味越多）</label>
          <input type="range" id="set-rand" min="0" max="100" step="5" value="${Math.round((s.randomness ?? 0) * 100)}"></div>
        <div class="check"><input type="checkbox" id="set-auto" ${s.auto_recommend !== false ? "checked" : ""}><label for="set-auto">打开页面自动推荐「猜你想玩」（按画像直接推一轮，省一次输入）</label></div>
        <button class="btn primary" id="set-save">保存设置</button>
        <div class="muted" style="margin-top:8px">当前生效：${esc(s.llm.model || "未配置")}。密钥与接口地址在上方「API 密钥与模型」填写。</div>
      </div>
    </div>
    <div class="panel" style="margin-top:16px;max-width:940px">
      <h4>数据同步 <button class="btn" id="rerun-onboarding" style="float:right">重新运行首次引导</button></h4>
      <div class="muted" id="sync-status">查询中…</div>
      <div style="display:flex;gap:10px;margin:10px 0;flex-wrap:wrap">
        <input id="sync-steamid" placeholder="SteamID64（留空自动检测本机账号）" style="flex:1;min-width:240px;background:var(--bg3);border:1px solid var(--border);color:var(--fg);padding:8px 10px;border-radius:8px;font-size:13px">
        <button class="btn primary" id="sync-start">开始同步</button>
        <button class="btn red" id="sync-stop">停止</button>
      </div>
      <div id="sync-log">（同步日志）</div>
      <div class="muted" style="margin-top:8px">首次同步含商店标签与成就类型分析（LLM），约 2–5 分钟；此后增量秒级。所有数据仅存本地。也可以在「游戏库存」页直接点「更新游戏库」。</div>
    </div>`;
    $("#set-save").addEventListener("click", async () => {
      await api("/api/settings", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          active_llm: $("#set-llm").value,
          llm_positioning: $("#set-pos").checked,
          daily_budget_cny: Number($("#set-budget").value) || 0,
          randomness: Number($("#set-rand").value) / 100,
          auto_recommend: $("#set-auto").checked,
        }),
      });
      loadMini();
      loadSettings();
    });
    // 密钥与端点：key 有输入才 POST /api/secrets；base_url/model 走 /api/settings（meta 热更新）
    $("#set-keys-save").addEventListener("click", async () => {
      const msg = $("#keys-msg");
      const steamKey = $("#set-steam-key").value.trim();
      const llmKey = $("#set-llm-key").value.trim();
      const num = (id) => {
        const v = $(id).value.trim();
        return v === "" ? null : Number(v);
      };
      try {
        // 顺序关键：先保存设置（可能改了密钥变量名，服务端热更新 env 名），
        // 再写密钥——/api/secrets 按最新的变量名写入 .env，避免 key 落到旧变量断链
        const pin = num("#set-price-in");
        const pout = num("#set-price-out");
        await api("/api/settings", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            llm_base_url: $("#set-base-url").value.trim(),
            llm_model: $("#set-model").value.trim(),
            llm_name: $("#set-llm-name").value.trim(),
            llm_key_env: $("#set-key-env").value.trim(),
            // 价格：输入/输出都填了才提交覆盖（半填状态视为未改）
            price_input: pin !== null && pout !== null ? pin : undefined,
            price_cache: pin !== null && pout !== null ? num("#set-price-cache") : undefined,
            price_output: pin !== null && pout !== null ? pout : undefined,
          }),
        });
        if (steamKey || llmKey) {
          await api("/api/secrets", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ steam_key: steamKey || null, llm_key: llmKey || null }),
          });
        }
        msg.textContent = "✔ 已保存并即时生效";
        $("#set-steam-key").value = "";
        $("#set-llm-key").value = "";
        loadMini();
        refreshKeysStatus();
        loadSettings();
      } catch (e) {
        msg.textContent = "✖ " + e.message;
      }
    });
    // 恢复内置价格（清空覆盖，回到快照表/config 默认）
    $("#set-price-reset").addEventListener("click", async () => {
      await api("/api/settings", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ price_reset: true }),
      }).catch(() => {});
      loadSettings();
    });
    refreshKeysStatus();
    $("#rerun-onboarding").addEventListener("click", () => {
      if (window.TonightOnboarding) window.TonightOnboarding.open();
    });
    // 探索感滑条：松手即持久化（下一轮推荐生效），标签实时跟随
    const randInput = $("#set-rand");
    if (randInput) {
      randInput.addEventListener("input", () => {
        $("#set-rand-val").textContent = randInput.value + "%";
      });
      randInput.addEventListener("change", () => {
        api("/api/settings", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ randomness: Number(randInput.value) / 100 }),
        }).catch(() => {});
      });
    }
    // 带宽修改即时生效（localStorage + 重渲染所有卡片，不用重新生成）
    const speedInput = $("#set-dl-speed");
    if (speedInput) {
      speedInput.addEventListener("change", () => {
        const v = Math.max(1, Number(speedInput.value) || 100);
        speedInput.value = v;
        setDownloadSpeed(v);
      });
    }
    refreshSyncStatus();
    $("#sync-start").addEventListener("click", startSync);
    $("#sync-stop").addEventListener("click", async () => {
      const r = await api("/api/sync/stop", { method: "POST" });
      $("#sync-log").textContent += "\n[停止] " + r.message;
    });
  } catch (e) {
    body.innerHTML = `<div class="error-note">加载失败：${esc(e.message)}</div>`;
  }
}

async function refreshKeysStatus() {
  const n = $("#keys-status");
  if (!n) return;
  try {
    const k = await api("/api/secrets");
    const fmt = (set, tail, env) =>
      set ? `已设置 ••••${esc(tail || "")}（${esc(env)}）` : `未设置（${esc(env)}）`;
    n.textContent = `Steam：${fmt(k.steam_key_set, k.steam_key_tail, k.steam_key_env)} · LLM：${fmt(k.llm_key_set, k.llm_key_tail, k.llm_key_env || "—")}`;
  } catch (e) {
    n.textContent = "密钥状态查询失败";
  }
}

async function refreshSyncStatus() {
  const n = $("#sync-status");
  if (!n) return;
  try {
    const st = await api("/api/sync/status");
    const when = st.last_sync ? new Date(Number(st.last_sync) * 1000).toLocaleString("zh-CN") : "从未";
    n.textContent = `库内 ${st.owned} 款 · 上次同步：${when}${st.syncing ? " · 同步进行中…" : ""}`;
  } catch (e) {
    n.textContent = "状态查询失败";
  }
}

async function startSync() {
  const log = $("#sync-log");
  const steamid = $("#sync-steamid").value.trim();
  log.textContent = "开始同步…";
  $("#sync-start").disabled = true;
  await sseStream("/api/sync/start", { steamid: steamid || null }, (ev) => {
    if (ev.type === "progress") {
      log.textContent += "\n" + ev.payload.text;
    } else if (ev.type === "done") {
      log.textContent += "\n✔ " + ev.payload.message;
    } else if (ev.type === "error") {
      log.textContent += "\n✖ " + ev.payload.message;
    }
    log.scrollTop = log.scrollHeight;
    refreshSyncStatus();
  });
  $("#sync-start").disabled = false;
  refreshSyncStatus();
  checkSyncBanner();
}

// ============ 启动 ============
checkSyncBanner();
loadMini();

// hash 路由：#profile / #library / #history / #settings 直达对应页（可深链，演示脚本用）
function activateTabFromHash() {
  const name = location.hash.slice(1);
  if (name) document.querySelector(`#nav button[data-tab="${name}"]`)?.click();
}
window.addEventListener("hashchange", activateTabFromHash);
if (location.hash) activateTabFromHash();
