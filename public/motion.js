/* Minorminor · 轻量动效层（纸张 × 像素 × 字符）
   · 标题出现：打字机逐字显示 + 方块光标（页面标题、区块标题、空状态标题）
   · 区块出现：3 档阶梯「显影」，没有位移、没有缓动（.mm-enter，见 style.css）
   切换页面时登记本页标题的打字时间表；页面在打字期间被重新渲染（取数后刷新）时，
   新节点按时间表接着打，不会从头重来，也不会被跳过。
   prefers-reduced-motion: reduce 时全部关闭。 */

const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

const ENTER = [
  '.panel', '.storage-card', '.stats-strip>div', '.command-card', '.background-progress',
  '.terminal-panel', '.empty-state', '.scan-controls-panel'
].join(',');

const TYPE = [
  '.page-heading h1', '.section-header h2', '.visual-heading h2', '.card-top h3',
  '.scan-controls-panel h2', '.empty-state h2', '.visual-empty h3', '.terminal-welcome'
].join(',');

const CHAR_MS = 38;       // 每个字的间隔
const STAGGER_MS = 90;    // 同屏多个标题依次开打
const MAX_LEN = 40;       // 过长的标题直接显示

const done = new WeakSet();
let schedule = new Map();  // 本页标题文字 → 开打时间
let lastBatch = 0;
let route = null;

function routeKey() {                              // 首屏加载占位时没有导航，首次渲染即视为「换页」
  const active = document.querySelector('.nav-item.active');
  return active ? active.getAttribute('href') : null;
}

function typewrite(el, text, start) {
  const caret = document.createElement('i');
  caret.className = 'mm-caret';
  caret.setAttribute('aria-hidden', 'true');
  el.setAttribute('aria-label', text);
  const paint = () => {
    const shown = Math.max(0, Math.floor((performance.now() - start) / CHAR_MS));
    if (shown >= text.length) { el.textContent = text; el.removeAttribute('aria-label'); return true; }
    el.textContent = text.slice(0, shown);
    el.appendChild(caret);
    return false;
  };
  if (paint()) return;
  const timer = setInterval(() => { if (!el.isConnected || paint()) clearInterval(timer); }, CHAR_MS);
}

function run() {
  const key = routeKey();
  const routeChanged = key !== route;
  route = key;
  const now = performance.now();

  if (routeChanged) {                              // 新页面：按出现顺序登记打字时间表
    schedule = new Map();
    let order = 0;
    document.querySelectorAll(TYPE).forEach((el) => {
      const text = el.textContent;
      if (!el.children.length && text.trim() && text.length <= MAX_LEN && !schedule.has(text)) {
        schedule.set(text, now + Math.min(order++, 6) * STAGGER_MS);
      }
    });
  }

  document.querySelectorAll(TYPE).forEach((el) => {
    if (done.has(el)) return;
    done.add(el);
    if (reduced || el.children.length) return;    // 标题内含标签时不改动结构
    const text = el.textContent;
    const start = schedule.get(text);
    if (start != null && now < start + text.length * CHAR_MS) typewrite(el, text, start);
  });

  const batch = routeChanged || now - lastBatch > 600;
  const blocks = document.querySelectorAll(ENTER);
  if (!batch) {                                   // 高频重渲染：区块直接显示，事后也不补动画
    blocks.forEach((el) => done.add(el));
    return;
  }
  lastBatch = now;
  blocks.forEach((el) => {
    if (done.has(el)) return;
    done.add(el);
    if (!reduced) el.classList.add('mm-enter');
  });
}

run();
new MutationObserver(run).observe(document.getElementById('app'), { childList: true });
