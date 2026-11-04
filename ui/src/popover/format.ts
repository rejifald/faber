// Every duration and time the popover shows goes through these (docs/design/guidelines.md,
// Copy). Pure, so stories and tests format exactly as the app does.

/** A duration, days-aware: 45s, 12m, 5h 20m, 3d 4h. */
export function fmtUptime(seconds: number | undefined): string {
  const s = Math.max(0, Math.floor(seconds || 0));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

/** The coarse form for an age, one unit only: 40s, 7m, 3h, 2d. */
export function fmtAgo(seconds: number | undefined): string {
  const s = Math.max(0, Math.floor(seconds || 0));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** A duration in words for a screen reader: "6 minutes", "45 seconds", "3 hours". */
export function spokenAgo(seconds: number | undefined): string {
  const s = Math.max(0, Math.floor(seconds || 0));
  if (s < 60) return plural(s, 'second');
  if (s < 3600) return plural(Math.floor(s / 60), 'minute');
  if (s < 86400) return plural(Math.floor(s / 3600), 'hour');
  return plural(Math.floor(s / 86400), 'day');
}

const pad2 = (n: number) => String(n).padStart(2, '0');
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

/** "HH:MM" for hold rows: 24-hour, in the viewer's local time, the same in every locale. */
export function fmtClock(ms: number | undefined): string {
  if (!ms) return '';
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? '' : `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/**
 * A date and time in the viewer's local time, the same in every locale: "28 Oct, 13:00", with the
 * year ("28 Oct 2025, 13:00") only when it is not the current one. Empty for a missing time.
 */
export function fmtDateTime(ms: number | undefined, now: number = Date.now()): string {
  if (!ms) return '';
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return '';
  const year = d.getFullYear() === new Date(now).getFullYear() ? '' : ` ${d.getFullYear()}`;
  return `${d.getDate()} ${MONTHS[d.getMonth()]}${year}, ${fmtClock(ms)}`;
}

export const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/**
 * A byte count in the unit that keeps it readable: whole bytes, then KiB, MiB, GiB or TiB with one
 * decimal. A small figure never reads `0.0 GiB`.
 */
export function bytes(n: number | undefined): string {
  const v = n || 0;
  if (v < 1024) return `${v} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let x = v / 1024;
  let i = 0;
  // Compared as it will print, so 1023.96 KiB reads 1.0 MiB, not 1024.0 KiB.
  while (Number(x.toFixed(1)) >= 1024 && i < units.length - 1) {
    x /= 1024;
    i += 1;
  }
  return `${x.toFixed(1)} ${units[i]}`;
}

/** A VM's size from MiB: no decimal when whole, one otherwise ("8 GiB", "12.5 GiB"). */
export function gib(mib: number): string {
  const g = mib / 1024;
  return `${Number.isInteger(g) ? g : g.toFixed(1)} GiB`;
}

/** GiB with one decimal, from MiB. */
export const gib1 = (mib: number | undefined) => ((mib || 0) / 1024).toFixed(1);

/** One shell word: as it is when plain, else single-quoted. */
export const shellWord = (v: string) => (/^[A-Za-z0-9._,/:@=+-]+$/.test(v) ? v : `'${v.replace(/'/g, "'\\''")}'`);

/** How long until an update installs: 3 h, 12 min, 40 s. */
export function waitIn(s: number): string {
  if (s >= 3600) return `${Math.round(s / 3600)} h`;
  if (s >= 60) return `${Math.ceil(s / 60)} min`;
  return `${s} s`;
}

/**
 * When the host re-arms, for a cool-off of 1 to 7 days: "at 14:20" today, "at Sat 14:20" within
 * the next 6 days, "at 24 Oct 14:20" beyond that, and "shortly" once the time has passed.
 */
export function rearmWhen(ms: number, now: number): string {
  if (ms <= now) return 'shortly';
  try {
    const d = new Date(ms);
    const t = new Date(now);
    const clock = fmtClock(ms);
    if (d.toDateString() === t.toDateString()) return `at ${clock}`;
    const startOfDay = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
    const days = Math.round((startOfDay(d) - startOfDay(t)) / 86400000);
    if (days <= 6) return `at ${WEEKDAYS[d.getDay()]} ${clock}`;
    return `at ${d.getDate()} ${MONTHS[d.getMonth()]} ${clock}`;
  } catch {
    return 'shortly';
  }
}

/** A version without its `+build` metadata, to compare the app's build with the daemon's. */
export const baseVersion = (v: string | undefined) => String(v || '').split('+')[0];
