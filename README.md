# gh-arriba

A GitHub CLI extension for the chores `gh` doesn't cover: review threads, pending reviews,
CI retries, signed rebases, releases, Dependabot, and project config sync.

Every command takes `--json`, and every prompt has a flag that answers it, so scripts and coding
agents can drive the same workflows a person runs by hand.

## Install

```
gh extension install nmoinvaz/gh-arriba
```

Upgrade with `gh extension upgrade arriba`.

## Use with Claude

Add this line to `~/.claude/CLAUDE.md` so Claude Code reaches for `gh arriba` where `gh` falls
short:

```
* Use `gh arriba` for what `gh` lacks, such as PR review threads, pending reviews, CI reruns, job logs, releases, and Dependabot. Run `gh arriba --help` for the commands.
```

## Usage

```
gh arriba [options] <command>
```

### Pull requests

| Command | Description |
|---------|-------------|
| `pr threads [PR]` | List unresolved review threads from humans and bots |
| `pr reply <ID> [BODY]` | Reply to a review comment or thread, body from stdin with `-` |
| `pr resolve <ID>...` | Resolve or unresolve (`--undo`) review threads |
| `pr comment show <URL\|ID>` | Print one review comment, issue comment, or review by its link or ID |
| `pr comment edit <URL\|ID> [BODY]` | Replace the body of a comment or review you posted, body from stdin with `-` |
| `pr review lines [PR]` | Print the diff line ranges that accept inline comments |
| `pr review add <PATH:LINE[-END]> [BODY]` | Add an inline comment to your pending review |
| `pr review show [PR]` | Print the comments in your pending review |
| `pr review submit [PR]` | Submit your pending review as comment, approve, or request changes |
| `pr review discard [PR]` | Delete your pending review |
| `pr apply [PR]` | Apply reviewers' suggestion blocks to the working tree |
| `pr dump [PR]` | Write the whole discussion as grep-friendly text |
| `pr retry [PR]` | Rerun failed and cancelled checks on a PR, or all your PRs with `--mine` |
| `pr rebase [PR]` | Rebase onto the base branch, signed locally or server-side with `--remote` |
| `pr reviewers [PR]` | Suggest reviewers from top contributors and request one with `--add` |

`PR` is a number, URL, `owner/repo#N`, or branch. It defaults to the PR for the current branch.
`ID` in `pr reply` and `pr resolve` is a comment ID, a `#discussion_r` comment link, or a
`PRRT_` thread ID.

### Workflows and releases

| Command | Description |
|---------|-------------|
| `run logs <RUN_OR_JOB>` | Download job logs to files for searching |
| `release` | Dispatch the repo's release workflow with a version bump, previewing with `--dry-run` |

### Dependabot

| Command | Description |
|---------|-------------|
| `dependabot list` | Group open PRs into ready, needs rebase, and blocked, with lock-step sets |
| `dependabot merge [PR]...` | Approve and merge PRs, singly or as a lock-step group |
| `dependabot rebase [PR]...` | Ask Dependabot to rebase PRs |
| `dependabot alerts` | List open security alerts with their fix PR |
| `dependabot dismiss <ALERT>...` | Dismiss alerts with a reason |

### Config sync

| Command | Description |
|---------|-------------|
| `config [sync] [files]` | Push or pull each tracked file, whichever side is newer (default) |
| `config status [files]` | Show what sync would push or pull, without changing anything |
| `config add [--platform] <files>` | Track files and push them, creating the gist on first use |
| `config remove [--platform] <files>` | Stop tracking files and delete them from the gist |
| `config restore [files]` | Overwrite local files with the gist versions |
| `config cleanup` | Delete gist files the manifest no longer lists |
| `config open` | Open the project's gist in a web browser |
| `config delete` | Delete the project's gist, after confirming |

See [Config sync](#config-sync-1) for how it works.

### Options

| Option | Description |
|--------|-------------|
| `-R`, `--repo <OWNER/REPO>` | Target repository (default: current repo) |
| `--json` | Print machine-readable JSON |
| `-y`, `--yes` | Skip confirmation prompts |
| `-h`, `--help` | Print help for any command |
| `-V`, `--version` | Print version information |

## Examples

Work through review feedback on the current branch's PR:

```
gh arriba pr threads
gh arriba pr apply --thread PRRT_kwDOAbc123
echo "Fixed." | gh arriba pr reply 1234567890 --resolve
```

Build up a review with inline comments, then submit it:

```
gh arriba pr review lines 42
gh arriba pr review add src/inflate.c:120-124 "This drops the window check." --pr 42
gh arriba pr review submit 42 --event request-changes --body "See inline comments."
```

Rerun every failed check across all your open PRs:

```
gh arriba pr retry --mine
```

Download only the failed job logs from a run and search them:

```
gh arriba run logs https://github.com/owner/repo/actions/runs/123456 --failed -o logs
grep -rn "error:" logs
```

Cut a minor release through the repo's Release workflow:

```
gh arriba release --bump minor
```

Merge every ready Dependabot PR, with lock-step groups merged together:

```
gh arriba dependabot merge --all-ready
```

## Config sync

`config` keeps per-project files that you don't commit, such as `.vscode/settings.json` or
`.claude/CLAUDE.md`, in sync across machines. Each project gets one GitHub gist that holds the
files plus a manifest listing them.

### First machine

Track the files from anywhere inside the project:

```
gh arriba config add --private .vscode/settings.json .claude/CLAUDE.md
```

This creates the gist and pushes the files. Gists are public unless you pass `--private`, and the
flag only matters here, when the gist is created.

### Every other machine

Run `config` with no arguments inside the same project:

```
gh arriba config
```

It reads the manifest from the gist, pulls every tracked file, and pushes any you changed locally.
Run it again whenever you edit a tracked file. To track another file later, run `config add` again
from any machine.

### Which side wins

Sync works per file and never merges. It compares each file's local modification time with the time
the manifest recorded for it.

| Local file | Sync |
|------------|------|
| Newer than the manifest | Pushes it to the gist |
| Older than the manifest | Pulls the gist version |
| Missing | Pulls the gist version |
| Within a second of the manifest | Leaves it alone |

Edit a file on one machine at a time, since the newer copy overwrites the other. Run `config status`
first to see what would happen, or `config restore` to take the gist version regardless.

### Platform-specific files

Add a file with `--platform` when it only applies to the current OS, such as a toolchain path:

```
gh arriba config add --platform .vscode/cmake-variants.yaml
```

Run the same command on each OS and each keeps its own copy of that path in the gist. Sync only
touches the copy for the OS it runs on, `macos`, `linux`, or `windows`.

### Options

| Option | Description |
|--------|-------------|
| `--name <NAME>` | Gist name prefix (default: basename of the project root) |
| `--root <DIR>` | Directory that file paths are relative to (default: git root, or the current directory) |
| `--private` | Create the gist as secret instead of public |

### Safety

- Before pushing, `config` scans files for potential secrets such as API keys, tokens, and
  passwords, and aborts if it finds any.
- `config delete` asks for confirmation, or takes `--yes`.

### Gist layout

The gist's description is `"<project> config-sync"`, which is how `config` finds it again. Each file
is stored under a flat name with underscores for path separators, as in
`zlib-ng_.vscode_settings.json`, and platform-specific files tag the OS before the extension, as in
`zlib-ng_.vscode_settings[macos].json`. The manifest is `.<project>-config-sync.yaml`.

`config` replaces [gcs](https://github.com/nmoinvaz/gcs) and reads the gists it created.

## Authentication

Uses the token from `gh auth token`, falling back to `GH_TOKEN` or `GITHUB_TOKEN`. Set
`GH_HOST` to target GitHub Enterprise Server.

## Development

```
cargo build --release
cp target/release/gh-arriba .
gh extension install .
```

The copied binary is ignored by git. Rebuild and copy it again to pick up changes.

## License

MIT
