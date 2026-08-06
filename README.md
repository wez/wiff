# wiff

> sniff out the wiff in your diff, from the comfort of your terminal

wiff is a terminal-first diff and code-review tool. It captures a diff, lets you
browse and annotate it with syntax highlighting, and stores the review as a
local session that both a human (in the TUI) and an agent (via the CLI and a
skill) can read and write at the same time.

[Read the full docs over at https://wezfurlong.org/wiff/](https://wezfurlong.org/wiff/)

![A tour of wiff: opening a change, leaving a comment, switching themes, and resuming a session by id](docs/assets/wiff-demo.gif)

## Install

Prebuilt binaries for Linux (static) and macOS, x86_64 and arm64, are attached
to the rolling
[`continuous`](https://github.com/wez/wiff/releases/tag/continuous) release,
rebuilt from `main` on every commit.

Or install straight from the git repository with a recent Rust toolchain,
without cloning it first:

```bash
cargo install --git https://github.com/wez/wiff.git wiff
```

Or, from a checkout of this repository:

```bash
cargo install --path crates/wiff
```

Either way puts the `wiff` binary on your PATH.

## Review types

wiff has a rich local code-review model that needs no network access: it
captures a diff from your working tree, index, or history, and stores the
review -- comments, replies, verdicts, and all -- as a session on your own
disk. That makes it a good fit for private review of code that never leaves
your machine. On top of that same model, a forge integration mirrors a GitHub
pull request into a local session and publishes your review back, so you can
review a PR from the comfort of the terminal.

- **Local change review** captures a diff and reviews the change it describes:
  the working tree, the staged index, a branch or revision, or a whole branch
  back to its fork point. This is the default.
- **Forge review** mirrors a GitHub pull request into a local session so you can
  read and annotate it in the TUI, then publishes your comments, replies, and
  verdict back to the PR. Pull by number against the repo's forge remote, or by
  full URL from anywhere, even without a local checkout:

  ```bash
  wiff forge pull 6185
  wiff forge pull https://github.com/wezterm/wezterm/pull/6185
  ```
- **Explore review** annotates existing code rather than a change, which is
  useful when laying out a new task. It opens an
  empty review and reads a chosen set of files at their current state.
  Start one with `wiff new --explore`, then add files with
  `wiff explore add <PATH>...` or the file picker in the TUI.

## Reviewing with an agent

wiff ships an agent skill that teaches your agent to read and annotate a review
through the `wiff` CLI. The TUI stays yours; the agent works only through the
command line on the same session.

Install the skill and get its path with:

```bash
wiff skill-path
```

This expands the bundled skill into your data directory (rewriting each `wiff`
call to the absolute path of your binary) and prints the path to its `SKILL.md`.
Point your agent at that file, or copy the `skills/wiff-review/` directory into
wherever your agent looks for skills. Once installed, ask your agent to review
the diff or address the existing comments; it discovers the active session for
the checkout on its own.

## Usage

Start a review from a diff source and open the TUI:

```bash
wiff new                 # the working tree against the current commit
wiff new --cached        # the staged index
wiff new --change HEAD   # a branch, change, or revision against its parent
wiff new --from-base     # the whole branch back to its fork point
git diff | wiff new      # a unified diff piped in
```

A plain `wiff new` pins the base at the current commit, so a commit or amend
you make afterward stays inside the range and `wiff refresh` shows it. `--change`
follows a branch or change to its newest commit on refresh, while a bare
revision is held. `--from-base` takes the base from the configured
`base_revision_rules` (the merge-base with your upstream by default); `--base
<ruleset>` overrides it with an explicit ruleset, and `--base empty` reviews the
whole history to the root.

### Navigation

Bindings follow `less`, with review actions layered on top:

- `j` / `k` or arrows to scroll a line, `space` / `b` to page.
- `g` / `G` to jump to the top (the review summary) or bottom.
- `,` / `.` previous/next file, `[` / `]` previous/next hunk, `{` / `}`
  previous/next comment.
- `t` file picker, `C` comment picker, `T` theme picker.
- `/` and `?` search forward/backward, `n` / `N` to repeat.
- `enter` toggles a fold, `tab` toggles a comment, `H` hides all comments,
  `w` toggles line wrapping, `L` toggles the line-number gutter.

### Views

Switch the diff layout live (or set `diff_mode` in config):

- `1` unified: one column with added and removed lines interleaved.
- `2` side-by-side: before on the left, after on the right.
- `3` after-side only: the resulting file in a single column.
- `4` rendered: the after side through a type-specific renderer (markdown
  today), falling back to the after-side source when there is no renderer.
- `0` auto: side-by-side when the terminal is wide enough, unified otherwise.

### Comments

- `v` starts a linewise selection; extend it with the navigation keys.
- `c` adds a comment on the selection, or on the cursor line with no selection.
- On a comment, `r` replies, `e` edits, `x` resolves, `d` withdraws, `a` sets a
  verdict.
- In the editor, `ctrl-d` moves the body into your drafts and `esc` cancels.

Edits are buffered as drafts and marked as such. `ctrl-s` saves them to the
session; you are also offered to save on exit.

## Session lifetime

A session outlives a single sitting so you and your agent can iterate on it.

- **Create** a review to begin: `wiff new`.
- **Iterate** until done: reopen with `wiff resume`, capture new changes into
  the session with `wiff refresh` (or `ctrl-r` in the TUI), which rebases your
  comments forward onto the new diff. Because a session stores how to find its
  base and tip rather than a frozen pair of commits, refresh follows an amend,
  rebase, or added commit on its own; when the base has moved out from under the
  review it captures anyway and warns that the starting point shifted.
- **Remove** it when finished: `wiff session rm`, or choose Remove session on
  exit.

List sessions any time with `wiff session list`. When a repository holds several
sessions, acting commands without an explicit `--session` resolve the one whose
review matches the branch you have checked out, falling back to the most recent
session when none matches.
