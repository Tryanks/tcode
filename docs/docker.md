# Tcode on Docker, NAS and homelab hosts

Releases publish `ghcr.io/tryanks/tcode-headless` for Linux `amd64` and `arm64`.
The image contains the release's headless binary with its browser app embedded.
`latest` includes all six native agent CLIs; `slim` contains the host, Node.js
and the system tools needed to install selected agents on first start. Version
tags are `<version>` and `<version>-slim`, without the leading `v`. Prereleases
have version tags only and do not replace `latest` or `slim`.

## Start a machine

Download [docker-compose.yml](../docker-compose.yml) into a directory on the
NAS. Create a private `.env` beside it:

```dotenv
TCODE_PASSWORD=replace-with-a-long-unique-password
PUID=1000
PGID=1000
TCODE_PROJECTS=/srv/projects
# TCODE_VERSION=slim
# TCODE_AGENTS=codex,pi
```

Use a non-root project owner's `id -u` and `id -g` as `PUID` and `PGID`. Prepare the
host project directory with write access for those IDs; startup fixes ownership
of its own data volume, never `/projects`. Then run:

```sh
chmod 600 .env
docker compose up -d
docker compose logs -f tcode
```

Startup prints the machine id and invitation link/QR. Scan or paste the
invitation in a phone or desktop Tcode client. Open `http://<nas-ip>:47420/`
for the browser app and sign in with `TCODE_PASSWORD`. The invitation expires;
use the browser's **Settings → Remote → New invitation** for another device,
or `docker compose exec --user tcode tcode tcode-headless pair` to print the
current invitation. See [remote access](remote.md) for the pairing lifecycle.

The template publishes TCP 47420 for the password-protected browser listener
and UDP 47420 for native clients. Linux `network_mode: host`, replacing the
`ports` section, also carries LAN multicast/mDNS and is preferable for local
discovery. Under bridge networking, Traverse works normally; if a device cannot
find the direct LAN path, enter the NAS's IP address on the device. Allow inbound
UDP 47420 in the host firewall. HTTP is for trusted networks; use an HTTPS
reverse proxy for browser access over an untrusted network.

## Data and projects

| Container path | Purpose |
| --- | --- |
| `/data/tcode` | Tcode settings, threads, `traverse.json`, browser password and tokens |
| `/data/home` | The runtime user's `HOME`: all agent credential and configuration directories |
| `/data/agents` | Slim CLI installations and npm cache; custom global npm packages under `custom` |
| `/data/bin` | User-supplied executables, including ACP agent commands, on `PATH` |
| `/projects` | Mounted project directories; use these paths when adding projects in Tcode |

Keep and back up the **whole `/data` volume**. Losing `traverse.json` loses the
machine key and invalidates every native pairing. Losing the home directory
loses CLI logins. `docker compose down` retains the named volume;
`docker compose down -v` deletes it. Stop Tcode before taking a consistent
backup. Never run two hosts against the same volume.

The entrypoint initializes the volume, then runs both CLI installation and
Tcode as `PUID:PGID` (1000:1000 by default). Agent processes and terminal panes
inherit those IDs. Changing the IDs on an existing volume migrates its
ownership. Do not set `user:` in Compose: it bypasses that initialization.
Use `docker compose exec --user tcode tcode bash` for a maintenance shell.

## Choose agents

`TCODE_AGENTS` accepts comma-separated `claude,codex,pi,opencode,cursor,grok`.
Unset means all six; an explicitly empty value means no native providers, even
on the full image. Unknown names stop startup. A selection is reapplied to the
native provider switches at startup; omitted native profiles are disabled, and
their configuration and credentials are retained. ACP configuration is retained.

Only selected CLI commands are added to the managed `PATH` directory. On the
full image they point to bundled installations; on slim, missing installations
are downloaded into `/data/agents` and reused offline on later starts. The
[agent manifest](../crates/headless/container/agents.json) pins the versions for
both images. Cache entries include the architecture and version, so a migrated
volume cannot reuse an executable for the wrong CPU. Changing the selection
removes command links without deleting cached packages or credentials.

For ACP, install the agent's command into `/data/bin`, mount an executable
there, or run `npm install --global <package>` as `tcode` to persist it under
`/data/agents/custom`. Node, npm, npx and the system `PATH` remain available even
with an empty native selection. Install any additional language runtime in a
derived image. Add the command through Tcode's existing ACP configuration.

## Login from a Tcode terminal pane

Open a terminal pane in a project after connecting to the machine. Its shell
runs inside the container with the same home and selected commands as the
agents. Open displayed URLs on your own computer or phone. Browser callbacks
to `localhost` refer to that browser's computer, not the container; publishing
47420 does not forward an agent's OAuth callback port.

| Agent | In-container method | API-key alternative | Persistent location under `/data/home` |
| --- | --- | --- | --- |
| Claude Code | `claude auth login`: open the printed URL externally and paste the authorization code when offered. | `ANTHROPIC_API_KEY` | `.claude`, `.claude.json` |
| Codex | `codex login --device-auth`: open the verification URL and enter the code. Enable device-code login in account/workspace security settings first. Default `codex login` uses a localhost callback and needs forwarding or a local login copied into the container. | Set `OPENAI_API_KEY`, then `printenv OPENAI_API_KEY \| codex login --with-api-key` | `.codex/auth.json` |
| pi | Run `pi`, then `/login`. Use a device code or paste the final redirect URL/authorization code when prompted. A provider without a paste fallback needs callback forwarding or an API key. | The selected provider's key, e.g. `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | `.pi/agent/auth.json` |
| OpenCode | `opencode auth login`: paste a provider API key, or use GitHub Copilot's displayed device code. OAuth methods requiring a localhost callback need forwarding or a provider key. | The selected provider's key, e.g. `OPENAI_API_KEY` | `.local/share/opencode/auth.json` |
| Cursor | `NO_OPEN_BROWSER=1 cursor-agent login`: open the printed Cursor link externally while the CLI waits for browser authentication. | `CURSOR_API_KEY` for unattended operation | `.config/cursor`, `.cursor` |
| Grok | `grok login --device-auth`: follow the printed device-code flow. Default browser OAuth may need a localhost callback. | `XAI_API_KEY` | `.grok/auth.json` |

Sources: [Claude authentication](https://code.claude.com/docs/en/authentication),
[Codex headless authentication](https://developers.openai.com/codex/auth),
[pi providers](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md),
[OpenCode providers](https://opencode.ai/docs/providers/),
[Cursor authentication](https://cursor.com/docs/cli/reference/authentication).
Provider login flows can change; the image pins CLI versions, not account policies.

Enable the relevant environment rows in Compose for API-key authentication,
then recreate the service. An `export` in one terminal affects that shell and
its children; it does not update the already-running Tcode host. Alternatively
configure the key in Tcode's provider settings.

The automated smoke check verifies CLI execution, password login, native LAN
pairing/reconnection, UID/GID ownership and persistence of the shared agent
home and browser login token. It does **not** complete third-party account logins. Those flows
still need an account-backed manual check on a Linux host; do not treat file
persistence checks as evidence of successful OAuth or API authentication.

## Optional Docker socket

Uncomment the socket mount and `DOCKER_GID` environment row in Compose to let agents create
**sibling containers**. This grants **host-root-equivalent access**. Enable it
only for agents and users you trust. It is not Docker-in-Docker.

The base image does not include a Docker CLI. For example, build a derived
image using the official static CLI (pin the image tags for your deployment):

```dockerfile
FROM docker:cli AS docker-cli
FROM ghcr.io/tryanks/tcode-headless:latest
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
```

Set `DOCKER_GID` to `stat -c %g /var/run/docker.sock` on the host. The entrypoint
adds that supplementary group, including group 0 when explicitly selected,
and also preserves other non-root groups supplied with Docker's `group_add`.
Bind mounts requested by sibling containers refer to **host paths**, so use
the host's `/srv/projects/...` path when constructing their mounts.

## Upgrade or build locally

Back up `/data`, change `TCODE_VERSION` if pinned, then run
`docker compose pull && docker compose up -d`. Bundled CLIs are image-managed;
update them by upgrading the image. Slim downloads the new manifest versions
once, retaining existing login data. Old cache versions can be removed while
the service is stopped to reclaim space.

The release workflow builds each architecture natively, consumes the same
`--features web` binaries as the archives, tests the images, and publishes
multi-architecture manifests to GHCR. It also publishes
`ghcr.io/tryanks/tcode-traverse`; see the
[Traverse server documentation](../crates/traverse-server/README.md#docker).

For maintainers enabling publishing for the first time: make both GHCR packages
public in their package settings after their first upload, and retain this
repository's Actions access. GHCR creates new packages as private; repository
linking grants workflow access but does not make anonymous pulls available.
See [GitHub's registry setup](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#pushing-container-images).

To reproduce a headless image build, stage the matching Linux release binary
as `dist/amd64/tcode-headless` or `dist/arm64/tcode-headless`, then run from the
repository root (Docker selects `TARGETARCH`):

```sh
docker build --target full -f crates/headless/Dockerfile -t tcode-headless:local .
# Or: --target slim
```

The [container CI](../.github/workflows/containers.yml) builds from source and
runs the same [smoke check](../crates/headless/container/smoke.mjs) on native
amd64 and arm64 runners. Release publishing repeats it against the actual
release binaries before pushing either architecture.
