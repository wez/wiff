# Reviewing with wiff

wiff captures a diff, opens it in a terminal UI where you browse and annotate
it, and stores the review as a session you and an agent can return to. This page
covers the three kinds of review it supports and the lifetime of a session. The
review UI itself is described in [The review interface](interface.md).

## Local change review

The default review captures a change from your repository and needs no network
access. The review, with all its comments, replies, and verdicts, is stored on
your own disk, which makes wiff a good fit for private review.

```bash
wiff new                 # the working tree against the current commit
wiff new --cached        # the staged index
wiff new --change HEAD   # a branch, change, or revision against its parent
wiff new --from-base     # the whole branch back to its fork point
git diff | wiff new      # a unified diff piped in
```

Each form fixes the base differently, which matters when you later `wiff
refresh` to pull in new work:

- Plain `wiff new` pins the base at the current commit. A commit or amend you
  make afterward stays inside the range, and `wiff refresh` picks it up.
- `--change` follows a branch to its newest commit on refresh; a bare revision
  is held as given.
- `--from-base` takes the base from your configured
  [`base_revision_rules`](configuration.md#base-revision), which by default is
  the fork point from your upstream branch. Pass `--base <ruleset>` to override
  it for one review, or `--base empty` to review the whole history back to the
  first commit.

## Forge review

A forge review mirrors a GitHub pull request into a local session. You read and
annotate it in the TUI just like any other review, then publish your comments,
replies, resolutions, and verdict back to the pull request.

```bash
wiff forge pull 6185                                          # by number
wiff forge pull https://github.com/wezterm/wezterm/pull/6185  # or by full URL
```

Run `wiff forge pull 6185` from inside the repository and wiff works out which
forge to talk to from your git remote, then fetches pull request 6185. A remote
configured through a `url.<base>.insteadOf` alias works too: wiff expands it the
way git does, so `octo:demo.git` finds the same forge as the URL it stands for.

You can also give a full pull-request URL. That names the forge outright, so it
works from anywhere, even outside a repository with no working copy checked out:
wiff fetches the before and after contents straight from the forge and diffs
them for you.

Once you have drafted your comments, publish them from the TUI by pressing `P`.
This is how most reviews go out: wiff first fetches the latest state of the pull
request and rebases your review onto it, then sends your comments back.

You can also publish from the command line with `wiff forge push`. When the
repository has more than one review open, say which to publish with `wiff forge
push <NUMBER|URL>` or `--session <ID>`.

### Supported hosts

GitHub is the only host with a working adapter today. Codeberg is recognised as
a host too, but its `forgejo` adapter is not implemented yet, so pulling one
reports that the adapter is unavailable. Any other host, including a self-hosted
GitHub or Forgejo instance, has to be added by hand under `[forge]` in
`config.toml`; see [Configuration](configuration.md).

Support for a new host is deliberately self-contained. Each host talks through a
small provider adapter, and the rest of wiff, the review model, session storage,
and the push and pull flow, is already in place. Adding Codeberg, or any other
Forgejo host, comes down to writing that one adapter.

### Authentication tokens

wiff reads the forge token from a per-host environment variable, or from the
command line:

- `GITHUB_TOKEN`, or `GITHUB_TOKEN_FILE` naming a file that holds the token.
- `--forge-token <TOKEN>` or `--forge-token-file <PATH>`, which take precedence
  over the environment. These belong on the `wiff forge` command, before the
  `pull` or `push` subcommand: `wiff forge --forge-token <TOKEN> pull 6185`.

## Explore review

An explore review is for looking at and commenting on code that already exists,
where there is no change to diff against. Instead of a diff, it reads a set of
files at their current state and lets you attach comments to the real lines.

A common use is handing context to a person or an agent when setting up a new
task: gather the relevant files, annotate the parts that matter, and share the
review so whoever picks the work up starts with your notes anchored to the code.

```bash
wiff new --explore                    # open an empty explore review
wiff explore add src/lib.rs src/main.rs   # add files to the review
```

Add files from the command line with `wiff explore add`, or from the in-review
file picker. Each file is captured at its working-copy state; re-adding a file
already under review captures nothing new. A refresh re-reads the same set from
disk.

## Session lifetime

A session outlives a single sitting so you and your agent can iterate on it.

- **Create** a review to begin: `wiff new` (or `wiff forge pull`).
- **Iterate** until done: reopen with `wiff resume`, and pull in new work with
  `wiff refresh` (or `ctrl-r` in the TUI), which recaptures the diff and moves
  your comments forward onto it. A session remembers how to find its base and
  tip rather than a fixed pair of commits, so refresh follows an amend, rebase,
  or added commit on its own. If the base itself has moved, refresh captures the
  new range anyway and warns you that the starting point shifted. A forge review
  refreshes by fetching its pull request again.
- **Remove** it when finished: `wiff session rm`, or choose Remove session on
  exit.

List sessions any time with `wiff session list`. When a repository holds several
sessions, acting commands without an explicit `--session` resolve the one whose
review matches the branch you have checked out, falling back to the most recent
session when none matches.
