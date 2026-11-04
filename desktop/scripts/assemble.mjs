// Assembles the app's frontend into desktop/dist, the `frontendDist` Tauri embeds:
//
//   ui/dist/popover/      the tray popover, built by `npm run build` in ui/ (ADR-0042), as popover/
//
// and stamps the build: src-tauri/gen/build.plist, which tauri.conf.json merges into the bundle's
// Info.plist, carries `FaberBuild`, this build's id in the daemon's own format (`<version>+g<sha>`,
// the root build.rs). `sudo faber setup` replaces an installed app whose stamp is not the binary's
// it runs from (ADR-0034, amended 2026-10-31), so the app moves with every daemon update.
//
// The popover is the app's only local page. The panel window loads the daemon's dashboard on a
// one-time link (ADR-0028 addendum C), so nothing of it is embedded here. tauri.conf.json runs
// this before every build, after building ui/. It refuses to run without the popover bundle
// rather than ship an app whose popover is blank.
import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const desktop = join(import.meta.dirname, '..');
const out = join(desktop, 'dist');
const popover = join(desktop, '..', 'ui', 'dist', 'popover');

if (!existsSync(join(popover, 'index.html'))) {
  console.error(`no popover bundle at ${popover}: run \`npm ci && npm run build\` in ui/ first`);
  process.exit(1);
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out);
// Kept so cargo, which embeds this directory at compile time, always finds it (desktop/.gitignore).
writeFileSync(join(out, '.gitkeep'), '');
cpSync(popover, join(out, 'popover'), { recursive: true });
console.log(`assembled ${out}: the popover from ui/dist/popover`);

// The same id the daemon's build.rs derives: the app's version, then `+g` and the short commit when
// there is a checkout to ask. The two versions move together (release.yml stamps both), so an app
// and a daemon built from one tree carry one id.
const { version } = JSON.parse(readFileSync(join(desktop, 'src-tauri', 'tauri.conf.json'), 'utf8'));
let build = version;
try {
  const sha = execFileSync('git', ['rev-parse', '--short=7', 'HEAD'], { cwd: desktop, encoding: 'utf8' }).trim();
  if (sha) build = `${version}+g${sha}`;
} catch {
  // No git: the bare version, as build.rs does.
}
const gen = join(desktop, 'src-tauri', 'gen');
mkdirSync(gen, { recursive: true });
writeFileSync(
  join(gen, 'build.plist'),
  `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>FaberBuild</key>
    <string>${build}</string>
</dict>
</plist>
`,
);
console.log(`stamped the build: FaberBuild ${build}`);
