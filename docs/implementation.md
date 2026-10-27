# Implementation addendum — 2026-10-27

Design note: [docs/design.md](design.md).


The design note above remains the product source of truth. This addendum records the implementation that exists in this repository, the decisions taken while building it, and the proof boundary. Where the design describes a future security boundary, it is called out explicitly rather than presented as finished.

## Product boundary

Tag Team is a standalone control plane for running Claude Code subtasks on enrolled machines:

- the **hub** owns the durable journal, placement, node leases, runner supervision, MCP surface, `done_when` verification, review-branch landing, and retention;
- each **runner** keeps the Claude Agent SDK loop on the main machine and reaches borrowed machines only through Tag Team MCP tools;
- each **node** executes the resulting commands inside an OS sandbox, with a local command journal, owner policy, egress proxy, and heartbeat;
- the **plugin** supplies the `/tag-team` skill, session binding, status context, and Stop hook;
- the **macOS menu bar app** and `tagteam-service` crate provide owner-facing node status and per-user service controls.

The implemented spine is real rather than a mock-up: a one-time mTLS enrolment is approved on the hub, work is dispatched through the MCP/local API, a runner calls remote tools, the node executes them in confinement, and the hub lands one unmerged review branch after rechecking `done_when`. The owner's HEAD, index, working tree, and current branch are not modified by result landing.

## Architecture

| Process | Where | Transport |
|---|---|---|
| `tagteam hub` | main machine | daemon |
| `tagteam mcp` | main machine, one stdio shim per Claude session | length-delimited JSON over a private Unix socket |
| Agent SDK runner | main machine, one process per subtask | JSON Lines over stdio |
| `tagteam-node` | borrowed machine | gRPC over mTLS; nodes always dial out |
| owner surfaces | hub or node machine | CLI, native menu bar, per-user LaunchAgent/systemd user unit |

The original custom-channel design is not implemented. Answer and steering delivery use the runner JSON Lines protocol with durable delivery rows, runner acknowledgement, and replay on reconnect. The main session can also observe work through MCP status, `inbox(wait)`, `tagteam watch`, and the Stop hook.

## Security and execution invariants

1. A journal state change and its event commit in the same SQLite WAL transaction.
2. Every runner-requested Bash, Read, Write, Edit, Glob, or Grep operation becomes an idempotent node command. Bash and filesystem helpers execute through the macOS Seatbelt or Linux bubblewrap sandbox. Workspace preparation, collection, and cleanup remain node-controlled lifecycle operations with path, digest, and size checks.
3. The runner allow-list is enforced by `allowedTools`, `disallowedTools`, a `PreToolUse` hook, and `permission_policy: "always_ask"` on every remote tool. The SDK auto-approves bare `allowedTools` entries before `canUseTool`, so `PreToolUse` is the effective per-call gate; `canUseTool` remains a fallback for tools outside that list. `settingSources` is `[]`, so user/project/local Claude settings and hooks cannot override Tag Team policy.
4. The node receives the owner's `NodePolicy` in the authenticated Hello. It independently rejects repositories outside `allowed_repo_ids`, resource profiles above local `max_cores`/`max_memory`, oversized egress lists, and malformed exact DNS hosts. The hub also schedules against node capacity, owner hours, battery state, repository head, and toolchain requirements.
5. Every node action is idempotent by `command_id`. A completed result survives reconnect; an interrupted pre-result command becomes a durable non-retryable failure instead of remaining `InProgress` forever.
6. Result branches are never merged automatically. The hub verifies bundle digest and connectivity, checks ancestry, enforces the aggregate patch limit for every command path including Bash, re-runs `done_when`, and only then creates the review branch.
7. Answers and steering instructions are durable, acknowledged, and replayable. Unanswered questions expire to explicit best-judgment guidance. The Stop hook blocks while work is waiting, treats remote question text as delimited untrusted data, and honours `stop_hook_active`.
8. Egress is proxy-only inside the sandbox. The proxy requires exact allow-listed DNS hostnames and rejects direct IPs, unlisted names, unsafe or mixed DNS resolution, disallowed ports, and malformed requests. Hub default hosts and node `extra_hosts` are normalized and de-duplicated.
9. Terminal, cancelled, and failed-preparation work emits a node cleanup command. The current node removes its per-subtask workspace; a persistent repository object cache is not implemented yet.

## Configuration and operation

Node configuration defaults to `~/.config/tagteam/node.toml`. `join` persists a private mode-0600 identity/config and removes the consumed invite from the saved configuration; it does **not** install a background service. A representative configuration is:

```toml
name = "studio-mac"
data_dir = "~/.local/share/tagteam-node"
only_when_idle = true
idle_after = "10m"
on_owner_return = "pause"
max_cores = 4
max_memory = "12G"
max_subtasks = 1
hours = ["22:00-07:00"]
never_on_battery = true
extra_hosts = ["git.example"]
# Root commit IDs this borrowed machine may execute. Omit only on a trusted
# own machine; [] denies every repository.
allowed_repo_ids = ["0123456789abcdef0123456789abcdef01234567"]
```

Durations and byte sizes accept friendly values (`30s`, `45m`, `30d`, `12G`) as well as legacy integer fields where supported. Invalid or conflicting representations fail configuration loading.

Hub configuration defaults to `~/.config/tagteam/hub.toml`:

```toml
listen = "127.0.0.1:7443"
public_address = "https://hub.example.ts.net"
data_dir = "~/.local/share/tagteam"
max_concurrent_per_account = 2
heartbeat_timeout_seconds = 90
runner_program = "node"
runner_args = ["/absolute/path/to/tag-team/runner/dist/src/main.js"]
default_egress = [
  "crates.io",
  "static.crates.io",
  "index.crates.io",
  "registry.npmjs.org",
  "pypi.org",
  "files.pythonhosted.org",
  "proxy.golang.org",
]

[limits]
wall_clock = "45m"
command_timeout = "10m"
question_deadline = "10m"

[retention]
events = "30d"
blobs = "7d"
```

Retention runs once while the hub opens and then hourly. It removes old terminal event history and unreferenced content-addressed blobs; active journal rows and referenced blobs are retained. A failed sweep is logged without stopping the daemon.

Node lifecycle commands are:

```bash
tagteam-node join <invite> --name studio-mac
tagteam-node probe
tagteam-node run
tagteam-node status
tagteam-node pause
tagteam-node resume
tagteam-node leave
tagteam-node service install
tagteam-node service start
tagteam-node service stop
tagteam-node service restart
tagteam-node service status --json
tagteam-node service uninstall
```

`pause` and `leave` use local control files and stop an installed service when present; `resume` starts it again if installed. `leave` retains the local identity but disables reconnects. Hub services use the same install/start/stop/restart/status/uninstall interface through `tagteam service`. On macOS these are per-user LaunchAgents; on Linux they are per-user systemd units.

The menu bar app lives at `apps/tagteam-menubar`. It reads `tagteam-node status` every three seconds and exposes pause, resume, leave, service controls, config opening, and node-ID copy. `Scripts/build-app.sh` builds an ad-hoc signed `TagTeam.app`; installation is a manual copy to `/Applications`.

## Heartbeat and placement evidence

The node heartbeat reports CPU and memory capacity, the free space of the volume containing its workspace path, battery state with a `battery_known` flag, local minute-of-day, idle duration, running workspaces, cached repository heads, and versions for:

`cargo`, `rustc`, `node`, `pnpm`, `git`, `python`, `python3`, `go`, and `xcodebuild`.

Missing idle evidence fails safe to `Present` for idle-only scheduling. Unknown battery evidence fails closed when `never_on_battery` is set. Configured owner hours use the node's local clock, including daylight-saving changes. Placement fails fast with a specific missing-toolchain reason; managed toolchain installation is not implemented.

## Interactive and recovery behavior

`AskMain` persists the question and leaves the runner tool promise pending. `tagteam answer` or the MCP `answer` tool commits the answer and a delivery row before attempting to send it; the runner acknowledges delivery, and an unacknowledged delivery is replayed when a runner becomes ready. `tagteam steer` uses the same durable pattern. The one-second expiry sweep closes overdue questions and resumes the runner with explicit timeout guidance.

Heartbeat loss and owner-return policy are reconciled by the hub. Work can be paused and resumed on the same node, or reassigned to another online node with the original runner session. Reassignment cancels the old runner and remote command, aborts the process-local driver, and prevents a stopped attempt's late result from replacing the new attempt's durable completion. The node command journal preserves completed and cancelled results across reconnect.

The loopback two-node recovery test proves:

- node A starts a slow sandboxed command and then disappears;
- B is selected after lease expiry and resumes the original runner session;
- exactly one review branch contains B's result;
- the owner checkout remains unchanged;
- A's delayed local side effect stays in A's workspace;
- reconnecting A and replaying its late result does not replace B's done state or duplicate the branch.

## Repository map

```text
proto/tagteam/v1/tagteam.proto   node <-> hub gRPC contract
crates/tagteam-core/             durable types, state machine, scheduling, protocols
crates/tagteam-proto/            generated gRPC bindings
crates/tagteam-pki/              invites, CA, mTLS identities
crates/tagteam-git/              snapshots, bundles, ancestry, review branches
crates/tagteam-sandbox/          Seatbelt / bubblewrap confinement
crates/tagteam-proxy/            exact-hostname allow-list HTTP(S) proxy
crates/tagteam-hub/              daemon, journal, scheduler, MCP shim, CLI
crates/tagteam-node/             enrollment, session, sandboxed commands, heartbeat
crates/tagteam-service/          per-user LaunchAgent/systemd management
runner/                          Claude Agent SDK loop and remote tool bridge
plugin/                          Claude Code skill, MCP wiring, hooks
apps/tagteam-menubar/            native macOS menu bar owner surface
scripts/                         loopback and opt-in live acceptance entrypoints
tests/integration/               workspace contract test
tests/sandbox/                   OS sandbox escape and cancellation tests
tests/fixtures/                  repository and protocol fixtures
```

## Development and proof

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm -C runner install --frozen-lockfile
pnpm -C runner typecheck
pnpm -C runner test
swift test --package-path apps/tagteam-menubar
./scripts/acceptance-loopback.sh
```

`scripts/acceptance-loopback.sh` runs the execution spine, OS exec and egress confinement, two-node recovery, runner typecheck, and runner tests. These use deterministic runners and spend no model tokens.

The live runner acceptance is opt-in because it spends provider tokens and requires a signed-in Claude Agent SDK host:

```bash
TAGTEAM_LIVE_SDK=1 ./scripts/acceptance-live-sdk.sh
```

Set `CLAUDE_CONFIG_DIR` to select a signed-in Claude Code profile when needed. The test confirms the model calls the exact `Finish` payload and simulates the hub’s post-Finish termination by closing the runner input after acknowledging that call.

### Verified in this checkpoint

On 2026-10-27, a local macOS arm64 run produced:

- `cargo fmt --all --check`: clean;
- `cargo clippy --workspace --all-targets -- -D warnings`: clean;
- `cargo test --workspace`: all tests passed, including the real loopback execution spine, macOS exec and egress confinement, sandbox escape tests, and two-node recovery;
- `./scripts/acceptance-loopback.sh`: passed in full;
- `pnpm -C runner typecheck`: passed;
- `pnpm -C runner test`: 9 passed, 1 live-SDK test skipped by the opt-in guard; the live test was then run separately and passed;
- `swift test --package-path apps/tagteam-menubar`: 8 passed;
- menu-bar app bundle build, ad-hoc codesign verification, and Info.plist lint: passed;
- live Agent SDK acceptance: passed at 2026-10-27 19:38 SAST; the model called `Finish` exactly once with outcome `no_change`, summary `live SDK acceptance passed`, and empty assumptions/checks; the runner exited `success` with `cost_usd: null` because the hub terminates the runner immediately after `Finish`.

### Not verified

These are separate proof boundaries, not extensions of the local result:

- full live Agent SDK conversation behavior beyond the bounded `Finish` call, including questions, steering, and model continuation across a real session;
- two real machines over a tailnet; the recovery proof is a loopback hub with two node identities;
- Linux runtime and sandbox acceptance; Linux logic is compiled and tested, but this checkpoint ran on macOS;
- a deployed hub with tailnet-only ingress, service hardening, and production DNS/TLS;
- interactive menu-bar launch, permission dialogs, and owner click paths;
- friend-machine account isolation: the per-repo allowlist exists, but the proposed separate `_tagteam` OS user and home-directory isolation do not;
- platform key storage: the node key is a mode-0600 `identity.json`, not the macOS Keychain;
- channel push: not implemented;
- managed toolchain installation: not implemented;
- persistent per-repository object-cache reuse: not implemented;
- project `CLAUDE.md` guidance in runner sessions: not loaded, because `settingSources` is intentionally empty and no sanitized project-guidance channel exists yet;
- an owner history view in the menu bar beyond node status and actions.
