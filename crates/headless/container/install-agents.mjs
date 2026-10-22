import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { constants as fsConstants } from 'node:fs';
import { access, mkdir, mkdtemp, readFile, readdir, rename, rm, symlink } from 'node:fs/promises';
import path from 'node:path';

const agents = JSON.parse(await readFile(new URL('./agents.json', import.meta.url)));
const selected = [...new Set((process.env.TCODE_AGENTS ?? Object.keys(agents).join(','))
  .split(',').map(name => name.trim()).filter(Boolean))];

for (const name of selected) {
  if (!Object.hasOwn(agents, name)) {
    throw new Error(`Unknown TCODE_AGENTS entry ${JSON.stringify(name)}; choose ${Object.keys(agents).join(',')}`);
  }
}

const [store, bundle] = process.argv.slice(2);
if (!store || !path.isAbsolute(store) || (bundle && !path.isAbsolute(bundle))) {
  throw new Error('Usage: install-agents.mjs ABSOLUTE_STORE [ABSOLUTE_BUNDLE]');
}

function run(program, args) {
  const result = spawnSync(program, args, { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} exited with ${result.status ?? result.signal}`);
}

async function executable(file) {
  try {
    await access(file, fsConstants.X_OK);
    return true;
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'EACCES') return false;
    throw error;
  }
}

const bin = path.join(store, 'bin');
await mkdir(bin, { recursive: true });
// Only this directory is managed: user-installed ACP commands have their own PATH entries.
for (const { commands } of Object.values(agents)) {
  for (const command of commands) await rm(path.join(bin, command), { force: true });
}

for (const name of selected) {
  const spec = agents[name];
  // A cache volume can move between hosts of different architectures.
  const relative = path.join(process.arch, name, spec.version);
  const cached = path.join(store, relative);
  const commandPath = spec.package ? path.join('bin', spec.commands[0]) : 'cursor-agent';
  let installation = cached;

  if (!await executable(path.join(cached, commandPath))) {
    const bundled = bundle && path.join(bundle, relative);
    if (bundled && await executable(path.join(bundled, commandPath))) {
      installation = bundled;
    } else {
      const parent = path.dirname(cached);
      await mkdir(parent, { recursive: true });
      // A killed first-start installation must not accumulate partial downloads.
      for (const entry of await readdir(parent)) {
        if (entry.startsWith('.install-')) await rm(path.join(parent, entry), { recursive: true, force: true });
      }
      const staging = await mkdtemp(path.join(parent, '.install-'));
      try {
        console.log(`Installing ${name} ${spec.version} (${process.arch})`);
        if (spec.package) {
          run('npm', ['install', '--global', '--prefix', staging, '--no-audit', '--no-fund',
            `--allow-scripts=${spec.package}`, '--', `${spec.package}@${spec.version}`]);
        } else {
          const digest = spec.sha256[process.arch];
          if (!digest) throw new Error(`Unsupported ${name} architecture: ${process.arch}`);
          const archive = path.join(staging, 'archive.tar.gz');
          run('curl', ['--fail', '--silent', '--show-error', '--location', '--retry', '3',
            '--output', archive, spec.archive.replace('{version}', spec.version).replace('{arch}', process.arch)]);
          const actual = createHash('sha256').update(await readFile(archive)).digest('hex');
          if (actual !== digest) throw new Error(`${name} archive digest ${actual} does not match ${digest}`);
          run('tar', ['--extract', '--gzip', '--file', archive, '--strip-components=1', '--directory', staging]);
          await rm(archive);
        }
        if (!await executable(path.join(staging, commandPath))) {
          throw new Error(`${name} did not install ${commandPath}`);
        }
        await rm(cached, { recursive: true, force: true });
        await rename(staging, cached);
      } finally {
        await rm(staging, { recursive: true, force: true });
      }
    }
  }

  for (const command of spec.commands) {
    await symlink(path.join(installation, commandPath), path.join(bin, command));
  }
}
console.log(`Available agents: ${selected.join(',') || '(none)'}`);
