import english from './locales/en.js';

export const languages = Object.freeze([['zh-CN', '简体中文'], ['en', 'English']]);
const storageKey = 'minorminor.language';
const supported = value => languages.some(([locale]) => value === locale);
let locale = 'zh-CN';
try { const saved = globalThis.document ? globalThis.localStorage?.getItem(storageKey) : null; if (supported(saved)) locale = saved; } catch {}

export const getLocale = () => locale;
export function setLocale(value) {
  if (!supported(value)) throw new RangeError('Unsupported language');
  locale = value;
  try { if (globalThis.document) globalThis.localStorage?.setItem(storageKey, value); } catch {}
  if (globalThis.document) document.documentElement.lang = value;
}

// Translate authored UI fragments only. File names, paths and content are interpolation values.
export function tr(value) {
  if (locale !== 'en' || typeof value !== 'string') return value;
  return value.replace(/[^<>="'\n]+/g, part => {
    const key = part.trim();
    if (!Object.hasOwn(english, key)) return part;
    return part.slice(0, part.indexOf(key)) + english[key] + part.slice(part.indexOf(key) + key.length);
  });
}

export function ui(strings, ...values) {
  return strings.reduce((result, part, index) => {
    let text = tr(part);
    if (locale === 'en' && index > 0 && String(values[index - 1]) === '1') {
      text = text.replace(/^(\s*(?:<\/[^>]+>\s*)*)(files|projects|sessions|folders|subfolders|items|entries|records|categories)\b/, (_, prefix, noun) => prefix + ({ entries: 'entry', categories: 'category' }[noun] || noun.slice(0, -1)));
    }
    return result + text + (index < values.length ? String(values[index]) : '');
  }, '');
}

export function languagePicker() {
  return `<label class="language-picker"><span>${tr('语言')}</span><select data-language aria-label="${tr('语言')}">${languages.map(([id, label]) => `<option value="${id}"${id === locale ? ' selected' : ''}>${label}</option>`).join('')}</select></label>`;
}

export function restoreLocale(preferences) {
  if (!supported(preferences?.locale) || preferences.locale === locale) return false;
  setLocale(preferences.locale);
  globalThis.location.reload();
  return true;
}

if (globalThis.document) {
  document.documentElement.lang = locale;
  const loading = document.querySelector('.initial-loading p');
  if (loading) loading.textContent = tr('正在连接…');
  document.addEventListener('change', async event => {
    const select = event.target.closest?.('[data-language]');
    if (!select || !supported(select.value)) return;
    event.stopImmediatePropagation();
    select.disabled = true;
    try {
      const bootstrap = await fetch('/api/bootstrap', { cache: 'no-store' }).then(response => response.json());
      const response = await fetch('/api/preferences', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Minorminor-Token': bootstrap.token }, body: JSON.stringify({ locale: select.value }) });
      if (!response.ok) throw new Error((await response.json()).error || tr('保存失败'));
      setLocale(select.value);
      globalThis.location.reload();
    } catch (error) {
      select.disabled = false;
      select.value = locale;
      const toast = document.querySelector('#toast');
      if (toast) { toast.textContent = error.message; toast.classList.add('show'); }
    }
  });
}
