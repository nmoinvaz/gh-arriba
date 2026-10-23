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

### Workflows and releases

| Command | Description |
|---------|-------------|
| `run logs <RUN_OR_JOB>` | Download job logs to files for searching |
| `release` | Dispatch the repo's release workflow with a version bump |

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
| `config [sync] [files]` | Sync files with the gist (default) |
| `config status [files]` | Report what sync would do without making any changes |
| `config add [--platform] <files>` | Add files to the tracked set and push them |
| `config remove [--platform] <files>` | Remove files from the tracked set and delete from gist |
| `config restore <files>` | Overwrite local files with the gist version, ignoring mtime |
| `config cleanup` | Remove gist files not listed in the manifest |
| `config open` | Open the project's gist in a web browser |
| `config delete` | Delete the entire config gist |

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

`config` syncs project config files, such as `.vscode/settings.json` or `.claude/CLAUDE.md`,
to and from a GitHub gist. It tracks which files belong to a project in a YAML manifest stored
inside the gist itself. After the first sync, running `gh arriba config` with no arguments reads
the manifest and does the right thing automatically.

```
gh arriba config add .claude/CLAUDE.md .vscode/settings.json
gh arriba config
```

Use `--name` to override the gist name prefix (default: basename of project root), `--root` to
set the root directory for relative paths (default: git root or cwd), and `--private` to create
the gist as secret instead of public.

A file that only applies to the current platform is added with `--platform`. The same path may
carry a separate variant per platform, and sync only ever touches the variant for the platform it
runs on.

Each project gets a single gist identified by the description `"<project> config-sync"`. Files
are stored with underscores replacing path separators (`zlib-ng_.vscode_settings.json`), and
platform-specific files tag the platform before the extension
(`zlib-ng_.vscode_settings[macos].json`). Syncing compares the newest local file modification
time against the manifest's timestamp, and whichever side is newer wins.

Before pushing files, `config` scans them for potential secrets (API keys, tokens, passwords) and
aborts if any are found.

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
