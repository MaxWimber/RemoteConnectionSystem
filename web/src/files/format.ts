// Sizes, speeds and times for the file manager, in Russian.

const UNITS = ['Б', 'КБ', 'МБ', 'ГБ', 'ТБ'];

function number(value: number, digits: number): string {
  return value.toFixed(digits).replace('.', ',');
}

/** Binary units, as Explorer shows them: 1536 → «1,5 КБ». */
export function formatSize(bytes: number): string {
  let v = bytes;
  let unit = 0;
  while (v >= 1024 && unit < UNITS.length - 1) {
    v /= 1024;
    unit++;
  }
  const digits = unit === 0 || v >= 100 ? 0 : v >= 10 ? 1 : 2;
  return `${number(v, digits)} ${UNITS[unit]}`;
}

export function formatSpeed(bytesPerSec: number): string {
  return `${formatSize(Math.max(0, bytesPerSec))}/с`;
}

/** Time left: «2 ч 05 мин», «3 мин 07 с», «12 с»; nothing when unknown. */
export function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '';
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const two = (n: number) => String(n).padStart(2, '0');
  if (h > 0) return `${h} ч ${two(m)} мин`;
  if (m > 0) return `${m} мин ${two(sec)} с`;
  return `${sec} с`;
}

export function formatDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const two = (n: number) => String(n).padStart(2, '0');
  return `${two(d.getDate())}.${two(d.getMonth() + 1)}.${d.getFullYear()} ${two(d.getHours())}:${two(d.getMinutes())}`;
}
