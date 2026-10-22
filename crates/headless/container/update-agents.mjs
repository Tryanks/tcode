// Moves agents.json to each CLI's current release: npm's `latest` tag, and the
// Cursor build its official install script downloads, with archive digests.
import { createHash } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';

const manifest = new URL('./agents.json', import.meta.url);
const agents = JSON.parse(await readFile(manifest));

async function fetchOk(url) {
  const response = await fetch(url, { signal: AbortSignal.timeout(600_000) });
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  return response;
}

for (const [name, spec] of Object.entries(agents)) {
  const previous = spec.version;
  if (spec.package) {
    spec.version = (await (await fetchOk(`https://registry.npmjs.org/${spec.package}/latest`)).json()).version;
  } else {
    const script = await (await fetchOk(spec.installer)).text();
    const version = script.match(/downloads\.cursor\.com\/lab\/([^/"'\s]+)\//)?.[1];
    if (!version) throw new Error(`${spec.installer} names no ${name} version`);
    spec.version = version;
    for (const arch of Object.keys(spec.sha256)) {
      const archive = await (await fetchOk(spec.archive.replace('{version}', version).replace('{arch}', arch))).arrayBuffer();
      spec.sha256[arch] = createHash('sha256').update(Buffer.from(archive)).digest('hex');
    }
  }
  console.log(`${name}: ${previous} -> ${spec.version}`);
}
await writeFile(manifest, `${JSON.stringify(agents, null, 2)}\n`);
