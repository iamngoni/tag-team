# Tag Team

[![CI](https://github.com/iamngoni/tag-team/actions/workflows/ci.yml/badge.svg)](https://github.com/iamngoni/tag-team/actions/workflows/ci.yml)

Tag Team lets Claude Code split independent work into subtasks that run on enrolled macOS or Linux machines. The hub coordinates work while the Claude Agent SDK runner stays on your main machine; borrowed nodes execute commands in isolated workspaces and return review branches.

## What it does

- Dispatches independent Claude Code subtasks to available machines on your network.
- Keeps the agent loop and Claude Code login on the hub machine.
- Enrolls nodes with one-time invites and mTLS; owners can approve, pause, and inspect nodes.
- Runs node commands in OS sandboxes and returns changes on review branches.
- Provides a Claude Code `/tag-team` plugin skill plus CLI status, inbox, logs, and collection commands.

## Current status and proof boundary

Local macOS arm64 checks and the bounded live Agent SDK acceptance passed on 2026-10-27. A full interactive conversation, operation across two real machines, Linux runtime acceptance, and a deployed service remain unverified. See the [implementation addendum](docs/implementation.md) for the exact evidence and gaps.

## Requirements

- macOS 14+ on Apple Silicon, or Linux x86_64 with glibc 2.35 or newer.
- Node.js 22+ and Claude Code signed in on the hub machine.
- Linux nodes require `bubblewrap` (`bwrap`) and a working per-user systemd manager (`systemd-run --user`) for sandboxed execution and cgroup limits.
- The hub and nodes must be able to reach each other over a private network; Tailscale or WireGuard is recommended.
- Source builds additionally require Rust 1.93.1 and pnpm 10.33+; the pinned protobuf compiler is vendored by the build.
- Swift/Xcode is needed only to build the optional macOS menu bar app.

## Install a GitHub release

Download the matching archive and `.sha256` sidecar from [GitHub Releases](https://github.com/iamngoni/tag-team/releases). Release assets are named `tagteam-vX.Y.Z-aarch64-apple-darwin.tar.gz` and `tagteam-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`.

```bash
archive=tagteam-vX.Y.Z-aarch64-apple-darwin.tar.gz # choose your platform's asset
curl -LO "https://github.com/iamngoni/tag-team/releases/download/vX.Y.Z/$archive"
curl -LO "https://github.com/iamngoni/tag-team/releases/download/vX.Y.Z/$archive.sha256"
shasum -a 256 -c "$archive.sha256" # Linux: sha256sum -c "$archive.sha256"
tar -xzf "$archive"
cd "${archive%.tar.gz}"
./install-release.sh
```

The installer verifies the archive contents, places `tagteam` and `tagteam-node` in `~/.local/bin`, the runner in `~/.local/share/tagteam/runner`, and the plugin in `~/.local/share/tagteam/plugin`. On macOS it also installs the optional ad-hoc-signed app at `~/Applications/TagTeam.app`. It does not overwrite hub or node configuration or start services. Add `~/.local/bin` to your `PATH`. Set `TAGTEAM_SKIP_APP=1` to omit the menu bar app, or `TAGTEAM_INSTALL_PREFIX=/path` to install somewhere other than `~/.local`.

## Install from source

From the repository root:

```bash
cargo install --path crates/tagteam-hub
cargo install --path crates/tagteam-node
# Or build both binaries into target/debug:
cargo build --bins

cd runner
pnpm install --frozen-lockfile
pnpm build
cd ..
```

The plugin directory is `plugin` in the source checkout. Keep the built runner at `runner/dist/src/main.js` or set the configured path to its absolute location. The source install does not install the optional menu bar app.

## Configure the hub and node

Create `~/.config/tagteam/hub.toml`, using absolute paths for both Node and the runner entry point:

```toml
listen = "0.0.0.0:7443" # or the hub's tailnet IP
public_address = "https://YOUR-HUB-TAILNET-NAME:7443"
data_dir = "~/.local/share/tagteam"
runner_program = "/absolute/path/to/node"
runner_args = ["/absolute/path/to/runner/dist/src/main.js"]
```

Release installs put the runner at `~/.local/share/tagteam/runner/dist/src/main.js`; use its fully expanded absolute path. Source installs use the checkout's `runner/dist/src/main.js`. The runner path in `runner_args` must be absolute. A background hub service should also use an absolute `runner_program` Node path, since its environment may have a different `PATH`. Keep the node-plane listener on the tailnet interface; do not expose port 7443 to the public internet. For a single-machine smoke test, `listen = "127.0.0.1:7443"` is sufficient.

On each node, create `~/.config/tagteam/node.toml`:

```toml
name = "borrowed-machine"
data_dir = "~/.local/share/tagteam-node"
only_when_idle = true
max_subtasks = 1
allowed_repo_ids = []
```

`allowed_repo_ids` is mandatory for borrowed machines: use approved 40-character Git root commit IDs. An empty list (`[]`) denies all repositories. Configure the hub address and network access for your environment before enrolling remote machines. For a first local smoke test, set `only_when_idle = false` so an actively used machine is schedulable; keep it `true` for a machine you are lending to others.

Find a repository's root commit ID with `git rev-list --max-parents=0 HEAD` inside that repository.

## Start, enroll, and inspect nodes

Start the hub in a terminal to test the configuration:

```bash
tagteam hub
```

For a long-running hub, install and start the per-user service instead:

```bash
tagteam service install
tagteam service start
tagteam service status
```

Create an invite on the hub machine, share it with the node owner, then join and approve the node:

```bash
tagteam invite borrowed-machine
# On the node, use the displayed invite:
tagteam-node join '<invite>' --name borrowed-machine
# Back on the hub:
tagteam approve borrowed-machine
tagteam nodes
```

Run `tagteam-node run` on the node, or install and start its per-user service:

```bash
tagteam-node service install
tagteam-node service start
```

Check readiness with `tagteam nodes` and `tagteam-node probe`.

## Load the Claude Code plugin

Load the plugin for a Claude Code session:

```bash
claude --plugin-dir "$HOME/.local/share/tagteam/plugin"
```

For a source checkout, use `claude --plugin-dir ./plugin` from the repository root. There is no marketplace manifest yet; `claude plugin install tagteam` will not work from this tree.

## Try `/tag-team`

In a committed scratch Git repository, start Claude Code with the plugin and ask `/tag-team` to split a task into independent subtasks. Review progress and returned changes with:

```bash
tagteam status
tagteam inbox --wait 5s
tagteam logs <subtask-id>
tagteam collect <subtask-id> --worktree
```

Collection with `--worktree` creates a review branch/worktree for you to inspect. Review its diff and run the relevant checks yourself. Review branches are not auto-merged.

## Development checks

These checks do not need Claude tokens:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd runner && pnpm typecheck && pnpm test
cd .. && ./scripts/acceptance-loopback.sh
```

## Limitations

- Review branches are not auto-merged; inspect and merge them yourself.
- Initial releases do not support Windows, Intel macOS, or Linux arm64.
- Tag Team does not install or manage node toolchains.
- The Claude Code plugin is loaded per session; it is not installed from a marketplace.
- The macOS menu bar app is ad-hoc signed and not notarized; macOS may require approval in System Settings before first launch.
- Licensing remains `UNLICENSED`.

## Documentation

- [Design note](docs/design.md)
- [Implementation addendum](docs/implementation.md)
- [Release process](docs/releasing.md)
- [Menu bar app](apps/tagteam-menubar/README.md)
