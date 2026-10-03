# Agent Development Lifecycle

Build, test, deploy, and operate A2A agents on Nasiko — from `nasiko new` to production.

## Overview

The lifecycle has four phases:

**Create** → **Test** → **Deploy** → **Operate**

You don't need a cluster to start developing. Create and test your agent locally — a cluster is only needed when you're ready to deploy.

## Prerequisites

- Docker running locally
- `nasiko` CLI installed

## Project Structure

An agent project has two required files and one auto-generated file:

    my-agent/
      AgentCard.json          ← you manage this (or `nasiko card` generates it)
      Dockerfile              ← you manage this (or `nasiko new` scaffolds it)
      src/                    ← your agent source code
      .nasiko/agent.json      ← auto-created by `nasiko deploy` (do not edit)

**AgentCard.json** is the single source of truth for your agent's identity. It contains:

- **name** — unique identifier on the cluster
- **version** — image tag used for deploys (e.g. "0.1.0")
- **description** — what the agent does
- **skills** — capabilities the agent exposes
- **capabilities** — streaming, push notifications, etc.
- **url** — agent endpoint
- **protocolVersion** — A2A protocol version (e.g. "0.2.9")
- **preferredTransport** — JSONRPC

Which commands use which files:

- `nasiko card` — generates **AgentCard.json** (from source + description)
- `nasiko validate` — checks both **AgentCard.json** and **Dockerfile** exist and are valid
- `nasiko build` — reads **Dockerfile** to build; reads **AgentCard.json** for the image tag (name:version)
- `nasiko run` — same as build, then starts the container
- `nasiko deploy` — reads **AgentCard.json** for name + version, builds, pushes, deploys; writes **.nasiko/agent.json** to track the agent ID on the cluster

The `.nasiko/agent.json` file links your local directory to a deployed agent — so subsequent `nasiko deploy` updates the same agent instead of creating a new one. Add it to `.gitignore`.

---

## Phase 1: Create

### Scaffold a new agent

    nasiko new                      # interactive — pick a template
    nasiko new openai my-agent      # from template directly
    nasiko new claude-sdk my-agent  # Claude SDK template

Templates are pulled from the artifact registry (Nasiko's public registry is connected by default). This creates a project directory with:

- **AgentCard.json** — A2A agent metadata (name, version, skills, endpoint)
- **Dockerfile** — container build instructions
- **Source code** — A2A protocol handler

### Generate or update the agent card

If you're connected to a cluster, the card is generated using an LLM (reads your source code + description). Otherwise it falls back to interactive prompts.

    nasiko card "A code review agent that finds bugs in PRs"
    nasiko card                     # omit description to auto-detect from source
    nasiko card --dir ./my-agent    # specify directory

### Validate your agent project

    nasiko validate                 # check structure, AgentCard.json, Dockerfile

---

## Phase 2: Test

### Build the container image

    nasiko build                    # build from current directory
    nasiko build --tag my-agent:v2  # custom tag
    nasiko build ./path/to/agent    # specify directory

### Run locally

    nasiko run                      # build + start on port 8000
    nasiko run --port 8080          # custom port

Your agent is now running at http://localhost:8000. It speaks the A2A protocol — no cluster needed.

### Chat with your agent

Interactive mode:

    nasiko chat http://localhost:8000

One-shot:

    nasiko chat http://localhost:8000 "What can you do?"

Full-screen TUI:

    nasiko chat http://localhost:8000 --tui

Resume a previous session:

    nasiko sessions
    nasiko chat http://localhost:8000 --resume <session-id>

---

## Phase 3: Deploy

When you're ready to deploy, connect to a cluster (remote or local).

### Connect to a cluster

Remote cluster:

    nasiko connect https://nasiko.example.com --name prod
    nasiko auth login

Or start a local cluster (auto-registers as "local"):

    nasiko up

`nasiko up` starts Postgres, Redis, and the Nasiko control plane locally. It registers itself as a cluster named "local" — visible in `nasiko clusters` alongside any remote ones.

    nasiko clusters       # see all configured clusters
    nasiko use prod       # switch active cluster
    nasiko down           # stop local cluster

### Deploy

    nasiko deploy .                          # build, push, deploy from current dir
    nasiko deploy . --name my-agent          # explicit name
    nasiko deploy . --port 8080              # non-default container port
    nasiko deploy . --env-file .env          # inject secrets from file
    nasiko deploy . -e API_KEY=sk-xxx       # inline env vars

This builds the Docker image, pushes it to the cluster's OCI registry, and starts the container. Agent names are unique per cluster — deploying the same name again updates the existing agent.

### Push without deploying

Useful for CI or when you want to separate image upload from deployment:

    nasiko push .
    nasiko push . --name my-agent

### Deploy from the registry

Deploy an agent that is already published to an artifact registry, without
building anything locally — the control plane pulls it itself, so no Docker
daemon is needed on your machine:

    nasiko import nasiko/my-agent:1.0.0                       # resolved against the connected registry
    nasiko import registry.example.com/nasiko/my-agent:1.0.0  # or fully qualified

The short `owner/name[:tag]` form resolves against whatever `nasiko registry
connect` points at, the same way `nasiko new` resolves a template. The tag
defaults to `latest`.

Both packaging formats are accepted: a container image is pulled and deployed
directly, while a source artifact is downloaded, built server-side, and then
deployed (the command prints a `build_id` in that case — follow it with
`nasiko logs <agent>`).

The registry host must be listed in the control plane's
`REGISTRY_IMPORT_ALLOWED_HOSTS`, otherwise the import is refused. Registries
that require authentication are not supported yet — the server fetches
manifests anonymously.

### Manage secrets

Secrets are encrypted env vars injected into agent containers at runtime. There are two scopes:

**Vault secrets** — applied to all your agents:

    nasiko secrets set OPENAI_API_KEY sk-xxx
    nasiko secrets ls
    nasiko secrets rm OPENAI_API_KEY

**Agent-specific secrets** — override vault, applied to one agent only:

    nasiko secrets set DB_URL postgres://... --agent my-agent
    nasiko secrets ls --agent my-agent
    nasiko secrets rm DB_URL --agent my-agent

**Precedence** (highest wins): inline deploy `-e` > agent secrets > vault secrets.

---

## Phase 4: Operate

### List running agents

    nasiko ps                       # table view
    nasiko ps --json                # machine-readable

### View logs

    nasiko logs my-agent            # last 50 lines
    nasiko logs my-agent -n 200    # more history

### Lifecycle management

    nasiko stop my-agent            # stop (scale to 0, keeps state)
    nasiko start my-agent           # resume a stopped agent
    nasiko restart my-agent         # redeploy with latest secrets/env
    nasiko rm my-agent              # terminate and deregister
    nasiko rm my-agent -f           # force removal without confirmation

**restart** destroys and recreates the container — it picks up any secrets you've changed since the last deploy. **stop/start** just pause and resume without changing configuration.

### Cluster health

    nasiko status                   # control plane health + metrics

### Observability & usage attribution

The control plane injects the standard `OTEL_*` environment variables into every
deployment (collector endpoint, `OTEL_SERVICE_NAME`, GenAI capture mode) and routes your
agent's LLM calls through its LLM gateway (`OPENAI_BASE_URL` etc.).

**Forwarding `traceparent` is mandatory, not best-effort.** Every LLM call (and every
MCP `tools/call`) must carry the W3C `traceparent` your agent received on its inbound
A2A request — the platform resolves the calling user and the flow's budgets from it,
and **rejects unattributable calls with `403`** rather than serving them with degraded
billing. There is no fallback attribution.

| Your agent | LLM / MCP calls served | Distributed traces | Per-user/session attribution |
|---|---|---|---|
| Forwards `traceparent` | ✅ | ✅ | ✅ precise (the trace id names the flow) |
| Drops `traceparent` | ❌ `403` on every call | ❌ | — |

How to forward it, per language — all driven by the injected `OTEL_*` env vars:

| Language | How |
|---|---|
| Python | `opentelemetry-instrument python main.py`, or initialize the SDK yourself (see `oss/agents/common/telemetry.py`) |
| Node.js | Set `NODE_OPTIONS="--require @opentelemetry/auto-instrumentations-node/register"` in your Dockerfile, with the package in your image |
| Java | OTel javaagent: `JAVA_TOOL_OPTIONS="-javaagent:/path/to/opentelemetry-javaagent.jar"` |
| Go | loongsuite `otel go build` in the Dockerfile (compile-time `net/http` instrumentation), or copy the manual-forwarding fallback in `oss/agents/weather` |
| Rust | No auto-instrumentation exists — initialize the OTel SDK in code and inject the header explicitly (see the Rust agents' `telemetry.rs`) |

Minimum: extract `traceparent` from your **inbound HTTP server** and inject it on your
**outbound LLM/MCP clients**. Token counts themselves come from the LLM provider's
response — no GenAI-specific instrumentors are required.

### Tools via the MCP gateway

Every deployment also receives the platform's tool layer, configured entirely by env:

- `MCP_GATEWAY_URL` — the gateway's single JSON-RPC endpoint (when the platform has one configured)
- `MCP_GATEWAY_TOKEN` — your agent's own credential, minted fresh on every deploy

Configure your MCP client **once at startup** — there is no per-request credential:

```python
mcp = McpClient(
    url=os.environ["MCP_GATEWAY_URL"],
    headers={"Authorization": f"Bearer {os.environ['MCP_GATEWAY_TOKEN']}"},
)
```

`tools/list` works immediately (agent-only identity — use it for startup discovery).
`tools/call` is authorized per flow: the gateway resolves the calling user from your
forwarded `traceparent` and requires that the flow was actually dispatched to your
agent, so the same OTel propagation above is all you need. The Go reference
implementation is `oss/agents/weather` (`mcp.go`).

---

## Phase 5: Share (Registry)

### Connect to the artifact registry

A public registry is connected by default. You can also use a private one:

    nasiko registry connect https://registry.example.com
    nasiko registry disconnect
    nasiko registry status

### Browse and search

    nasiko registry search "code review"     # search by keyword
    nasiko registry search -t agent          # filter to agents
    nasiko registry list                     # list everything
    nasiko registry list --json              # machine-readable

### Publish

Publishing to a shared artifact registry is available in the Nasiko enterprise edition.

---

## Typical Workflow

    # Scaffold
    nasiko new openai code-reviewer
    cd code-reviewer

    # Test locally (no cluster needed)
    nasiko validate
    nasiko run
    nasiko chat http://localhost:8000 "Review this PR: ..."

    # Deploy
    nasiko connect https://nasiko.example.com --name prod
    nasiko auth login
    nasiko deploy .

    # Monitor
    nasiko ps
    nasiko logs code-reviewer

---

## Iterating on a deployed agent

Code changes — redeploy (rebuilds image, pushes, restarts container):

    nasiko deploy .

Changed secrets only — restart to pick them up (no rebuild):

    nasiko secrets set NEW_KEY value --agent my-agent
    nasiko restart my-agent

Published a new version to the registry — re-run the import with the new tag:

    nasiko import nasiko/my-agent:1.0.1

This updates the agent in place rather than creating a second one: it matches
on owner + name, so the agent keeps its id, and its grants, ACLs, and stored
secrets survive. The control plane re-pulls the image and recreates the
container.

Prefer bumping the tag. Re-publishing the *same* tag also works — the pull
compares digests and the container is recreated either way — but the recorded
version no longer identifies which build is running.

Note that `nasiko restart` does **not** fetch a new image; it recreates the
container from what the cluster already has. Only `deploy` and `import` bring
in new code.

---

## Local cluster workflow

Test the full deploy flow locally before pushing to production:

    nasiko up                       # start local cluster
    nasiko deploy .                 # deploys to local
    nasiko ps                       # verify it's running
    nasiko chat http://localhost:8080/api/agents/my-agent "test"
    nasiko down                     # tear down when done

---

## CI/CD Integration

    # Build and push (CI job)
    nasiko connect $CLUSTER_URL
    nasiko auth login               # uses NASIKO_TOKEN env var
    nasiko push . --name my-agent

    # Deploy (CD job or manual gate)
    nasiko deploy my-agent:latest

---

## Command Reference

**Create**

- `nasiko new [template] [name]` — Scaffold agent project
- `nasiko card [description]` — Generate AgentCard.json
- `nasiko validate` — Validate project structure

**Test**

- `nasiko build` — Build Docker image
- `nasiko run` — Run agent locally
- `nasiko chat <url>` — Chat via A2A protocol

**Deploy**

- `nasiko connect <url>` — Register cluster
- `nasiko up` / `nasiko down` — Start/stop local cluster
- `nasiko deploy .` — Build + push + deploy
- `nasiko push .` — Push image only
- `nasiko import <ref>` — Deploy from a registry reference (server pulls it)
- `nasiko secrets set <key> <value>` — Store secret (vault or `--agent`)

**Operate**

- `nasiko ps` — List running agents
- `nasiko logs <agent>` — Stream logs
- `nasiko stop <agent>` — Stop (scale to 0)
- `nasiko start <agent>` — Resume stopped agent
- `nasiko restart <agent>` — Redeploy with fresh env
- `nasiko rm <agent>` — Terminate and deregister

**Share**

- `nasiko registry search` — Browse registry
- `nasiko registry list` — List all artifacts

Publishing to a shared artifact registry is available in the Nasiko enterprise edition.
