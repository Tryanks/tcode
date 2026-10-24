// Installs the agent CLIs TCODE_AGENTS selects into the data volume: each
// one's current release, the first time it is selected. From then on it is
// reused offline and updates itself with its own updater.
import { spawnSync } from 'node:child_process';
import { constants as fsConstants } from 'node:fs';
import { access, chmod, mkdir, mkdtemp, readFile, readdir, rename, rm, symlink, writeFile } from 'node:fs/promises';
import path from 'node:path';

const agents = JSON.parse(await readFile(new URL('./agents.json', import.meta.url)));
const selected = [...new Set((process.env.TCODE_AGENTS ?? Object.keys(agents).join(','))
  .split(',').map(name => name.trim()).filter(Boolean))];

for (const name of selected) {
  if (!Object.hasOwn(agents, name)) {
    throw new Error(`Unknown TCODE_AGENTS entry ${JSON.stringify(name)}; choose ${Object.keys(agents).join(',')}`);
  }
}

const [store] = process.argv.slice(2);
const home = process.env.HOME;
if (!store || !path.isAbsolute(store) || !home) {
  throw new Error('Usage: HOME=... install-agents.mjs ABSOLUTE_STORE');
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

/// An npm package in a prefix of its own, which its commands name to npm: a
/// CLI that updates itself with `npm install --global` updates this copy. A
/// volume can move between hosts of different architectures.
async function installPackage(name, spec) {
  const prefix = path.join(store, process.arch, name);
  if (!await executable(path.join(prefix, 'bin', spec.commands[0]))) {
    const parent = path.dirname(prefix);
    await mkdir(parent, { recursive: true });
    // A killed first-start installation must not accumulate partial downloads.
    for (const entry of await readdir(parent)) {
      if (entry.startsWith('.install-')) await rm(path.join(parent, entry), { recursive: true, force: true });
    }
    const staging = await mkdtemp(path.join(parent, '.install-'));
    try {
      run('npm', ['install', '--global', '--prefix', staging, '--no-audit', '--no-fund',
        `--allow-scripts=${spec.package}`, '--', `${spec.package}@latest`]);
      await rm(prefix, { recursive: true, force: true });
      await rename(staging, prefix);
    } finally {
      await rm(staging, { recursive: true, force: true });
    }
  }
  return command => [
    '#!/bin/sh',
    `NPM_CONFIG_PREFIX='${prefix}'`,
    'export NPM_CONFIG_PREFIX',
    `exec '${path.join(prefix, 'bin', command)}' "$@"`,
    '',
  ].join('\n');
}

/// Cursor's own installer, which puts the CLI under HOME where its updater
/// keeps it current.
async function installCursor(spec) {
  const installed = path.join(home, '.local', 'bin', spec.commands[0]);
  if (!await executable(installed)) {
    run('bash', ['-c', `set -o pipefail; curl --fail --silent --show-error --location '${spec.installer}' | bash`]);
  }
  return command => path.join(home, '.local', 'bin', command);
}

const bin = path.join(store, 'bin');
await mkdir(bin, { recursive: true });
// Only this directory is managed: user-installed ACP commands have their own PATH entries.
for (const { commands } of Object.values(agents)) {
  for (const command of commands) await rm(path.join(bin, command), { force: true });
}

const available = [];
for (const name of selected) {
  const spec = agents[name];
  try {
    console.log(`Preparing ${name}`);
    if (spec.package) {
      const wrapper = await installPackage(name, spec);
      for (const command of spec.commands) {
        const file = path.join(bin, command);
        await writeFile(file, wrapper(command));
        await chmod(file, 0o755);
      }
    } else {
      const target = await installCursor(spec);
      for (const command of spec.commands) await symlink(target(command), path.join(bin, command));
    }
    available.push(name);
  } catch (error) {
    // The machine starts without it, so it stays reachable; the next start
    // tries again.
    console.error(`Could not install ${name}: ${error.message}`);
  }
}
console.log(`Available agents: ${available.join(',') || '(none)'}`);
