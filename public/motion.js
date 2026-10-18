/* Minorminor · 轻量动效层（纸张 × 像素 × 字符）
   · 切换页面：页面标题与区块标题依次逐字打出，方块光标随字前进
   · 新出现的区块：3 档阶梯「显影」，没有位移和缓动（.mm-enter，见 style.css）
   界面按增量更新渲染（native.js 的 patch），轮询刷新不会重建节点，
   因此这里只对真正新插入的节点生效，刷新时不会反复闪烁。
   prefers-reduced-motion: reduce 时全部关闭。 */

const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

const ENTER = [
  '.panel', '.storage-card', '.stats-strip>div', '.native-progress', '.native-scan-report',
  '.cleanup-actions', '.empty-state', '.scan-controls-panel', '.project-grid>article', '.native-disk-row'
].join(',');

const TYPE = [
  '.page-heading h1', '.section-header h2', '.storage-card>h3', '.scan-controls-panel h2', '.empty-state h2'
].join(',');

const CHAR_MS = 38;       // 每个字的间隔
const STAGGER_MS = 90;    // 同屏多个标题依次开打
const MAX_LEN = 40;       // 过长的标题直接显示
const MAX_ENTER = 24;     // 一次最多显影的区块数，长列表其余直接出现

let route = null;

const routeKey = () => document.querySelector('.nav-item.active')?.getAttribute('href') || null;

// 打字期间给标题加 data-typing，增量更新会跳过它，避免刷新与打字互相覆盖。
function typewrite(el, start) {
  const text = el.textContent;
  if (el.children.length || !text.trim() || text.length > MAX_LEN) return;
  const caret = document.createElement('i');
  caret.className = 'mm-caret';
  caret.setAttribute('aria-hidden', 'true');
  el.setAttribute('aria-label', text);
  el.setAttribute('data-typing', '');
  const paint = () => {
    const shown = Math.max(0, Math.floor((performance.now() - start) / CHAR_MS));
    if (shown >= text.length || !el.isConnected) {
      el.textContent = text;
      el.removeAttribute('aria-label');
      el.removeAttribute('data-typing');
      return true;
    }
    el.textContent = text.slice(0, shown);
    el.appendChild(caret);
    return false;
  };
  if (paint()) return;
  const timer = setInterval(() => { if (paint()) clearInterval(timer); }, CHAR_MS);
}

function reveal(nodes) {
  let n = 0;
  for (const el of nodes) {
    if (n++ >= MAX_ENTER) break;
    el.classList.add('mm-enter');
    el.addEventListener('animationend', () => el.classList.remove('mm-enter'), { once: true });
  }
}

function collect(added) {
  const out = new Set();
  for (const node of added) {
    if (node.nodeType !== 1) continue;
    if (node.matches(ENTER)) out.add(node);
    node.querySelectorAll(ENTER).forEach((el) => out.add(el));
  }
  return [...out];
}

function onChange(records) {
  if (reduced) return;
  const key = routeKey();
  if (key !== route) {                       // 新页面：整页显影，标题依次打字
    route = key;
    const now = performance.now();
    reveal(document.querySelectorAll(`#main :is(${ENTER}), .cleanup-actions`));
    [...document.querySelectorAll(`#main :is(${TYPE})`)].slice(0, 7)
      .forEach((el, i) => typewrite(el, now + i * STAGGER_MS));
    return;
  }
  const added = [];
  for (const r of records) added.push(...r.addedNodes);
  reveal(collect(added));                    // 同一页面内新出现的内容
}

new MutationObserver(onChange).observe(document.getElementById('app'), { childList: true, subtree: true });
