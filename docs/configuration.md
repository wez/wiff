# Configuration

wiff reads an optional `config.toml` at startup. Every setting has a default, so
the file is only needed to change wiff's behaviour. Unknown keys are rejected,
which catches typos early.

## Location

The config file lives in the platform config directory for wiff:

- Linux: `~/.config/wiff/config.toml`
- macOS: `~/Library/Application Support/wiff/config.toml`
- Windows: `%APPDATA%\wiff\config.toml`

If `WIFF_CONFIG_DIR` is set in the environment, wiff reads `config.toml` from
that path instead.

## Options

### Display

```toml
# The unchanged lines kept on each side of a change; longer runs fold away.
display_context = 3
# The shortest run of unchanged lines that folds away; shorter runs stay open.
min_fold = 5
# Columns per tab stop for diff display and comment editing.
tab_width = 4
# Wrap diff content to the viewport width instead of clipping at the edge.
wrap_lines = true
# Show line numbers in the gutter on startup.
show_line_numbers = true
# The syntax theme; the rest of the interface takes its colors from it.
theme = "wez"
```

`display_context` is a display choice, independent of how much context the diff
was captured with. `wrap_lines`, `show_line_numbers`, and `theme` are the
startup values; `toggle_wrap`, `toggle_line_numbers`, and the `T` theme picker
change them within a session.

### Layout

```toml
# The starting diff layout: auto, unified, side_by_side, only_after, or rendered.
diff_mode = "auto"
# The terminal width at or above which `auto` chooses the side-by-side layout.
side_by_side_min_width = 130
# Detach the open comment editor when an arrow moves past its top or bottom,
# floating it at a screen edge to free the cursor for navigating the diff.
nudge_to_detach = true
```

`auto` uses side-by-side once the terminal is at least `side_by_side_min_width`
columns wide, and unified below that. The `diff_mode_*` actions switch the
layout within a session.

### On exit

```toml
# What to do with the session when the review UI exits: prompt, keep, or remove.
on_exit = "prompt"
```

### Author identity

wiff attributes annotations to a name that depends on whether a human or an
agent is acting. The defaults are `$USER` for a human and `assistant` for an
agent; override either:

```toml
[author.names]
human = "Wez Furlong"
agent = "reviewer-bot"
```

### Base revision

`wiff new --from-base` reviews a whole branch. To do that it needs a starting
point: the commit your branch grew from. That commit is the base, and
everything from there up to the branch tip is what you review.

wiff finds the base with a ruleset, a comma-separated list of rules tried left
to right until one resolves. The default is:

```toml
# Used by `wiff new --from-base` when no --base is given on the command line.
base_revision_rules = "merge-base(upstream), merge-base(trunk)"
```

This reads as: use the fork point from this branch's upstream; if the branch has
no upstream, use the fork point from trunk.

The fork point is what `merge-base` computes. Say your branch left `main` at
commit `B`, and `main` has gained commits since:

```
A---B---C---D      main
     \
      E---F---G    your branch (tip at G)
```

`merge-base(trunk)` resolves to `B`, so the review covers just `E`, `F`, and
`G`, your own work, without the commits `main` picked up in the meantime.

#### Operators

Each rule is one operator applied to a reference:

- `merge-base(<ref>)` is the fork point: the common ancestor of `<ref>` and the
  branch tip. This is the usual choice for a branch review.
- `ref(<ref>)` uses `<ref>` itself as the base.
- `parent(<ref>)` uses the first parent of `<ref>`.
- `empty` uses the empty tree, reviewing the whole history back to the first
  commit. `wiff new --from-base --base empty` is the shorthand for this.

#### References

A reference names the commit an operator works from:

- `trunk` is the repository's default branch.
- `upstream` is the branch this one tracks.
- `@` is the branch tip under review.
- `name(<ref>)` is any ref spelled out literally, such as `name(origin/main)`.
  The `name(...)` wrapper keeps a branch called `trunk` from colliding with the
  `trunk` symbol above.

#### Limiting a rule to one source-control system

wiff is designed to work with git, jj, hg, and sl, though only git is supported
today. Prefix a rule with a system's name and a colon to apply it only under
that system:

```toml
base_revision_rules = "git:ref(name(origin/main)), merge-base(trunk)"
```

The first rule is tried only in a git repository; anywhere else it is skipped
and the next rule runs.

For anything the built-in operators cannot express, hand an expression straight
to the system's own resolver by naming the system as the operator, as in
`jj(parents(@))`. The expression is opaque to wiff, so a native base that in
fact follows the tip is not recognised as tip-tracking, and a refresh reports it
moving each time the tip advances. To track the tip and keep refresh quiet, use
`parent(@)` or `merge-base(@)` instead.

### Forge hosts

wiff has built-in rows for two hosts, `github.com` and `codeberg.org`,
and only the GitHub adapter works today; a Codeberg pull reports that its
`forgejo` adapter is unavailable. To review pull requests on any other host,
including a self-hosted GitHub or Forgejo instance, add it yourself under
`[forge]`, keyed by host. A field left unset on a built-in host inherits its
default.

```toml
[forge."github.com"]
provider = "github"        # the adapter family that speaks to the host
token_env = "GITHUB_TOKEN" # the variable holding the token
token_file_env = "GITHUB_TOKEN_FILE" # or a variable naming a file holding it

[forge."git.example.com"]
provider = "github"        # a github-compatible instance
api_base = "https://git.example.com/api/v3"
token_env = "EXAMPLE_TOKEN"
```

The `provider` names which adapter family speaks to the host. `github` works
today; `forgejo` is recognised but not yet implemented. A command-line
`--forge-token` or `--forge-token-file` overrides the configured variables for a
single run.

### Enclosing-definition patterns

When a long run of unchanged lines folds away, the fold marker names the
definition those hidden lines sit inside, so you can tell where in the file you
are without expanding it. It is the same context `git diff` prints after the
`@@` on a hunk header.

For example, a change deep inside a long method collapses the lines above it to
a marker that still names the enclosing block:

```
▸ [24 unchanged lines]  impl Session {
      fn save(&self) -> Result<()> {
-         write(&self.path)?;
+         write_atomic(&self.path)?;
```

wiff ships patterns for rust, python, and markdown. Under `[section]` you can
teach it a new language, or replace the rules for one it already knows, keyed by
the language token the highlighter uses. Each value is a list of patterns in
git's `userdiff` format:

- a plain regular expression matches a line that opens a definition;
- a pattern starting with `!` marks a line to exclude, even when another pattern
  would match it.

Listing a language replaces its built-in rules rather than adding to them. The
example below teaches wiff about Go, which it ships no rules for:

```toml
[section]
# Lines that open a Go function, method, type, or interface.
go = ['^func\b', '^type\b']
```

If you instead list rust, python, or markdown, you take over that language's
rules completely, so repeat the cases you still want; otherwise definitions wiff
used to recognise stop being labelled.

### Generated files

A recognised generated file shows a `[generated]` badge and folds to its header
by default. wiff already knows many generated files; under `[generated]` you can
add your own rules:

- `names` adds path globs. A file whose path matches is treated as generated.
- `markers` adds strings. A file is treated as generated when one appears near
  the top of it.

To switch off one of wiff's built-in rules, list it again with a leading `!`:
the entry `!<rule>` removes the built-in rule whose text is `<rule>`. This helps
when a built-in rule wrongly flags a file in your project.

```toml
[generated]
# Path globs whose match marks a file generated, checked against the basename,
# or the whole path when the glob contains a `/`.
names = ["*.pb.go", "vendor/**"]
# Strings whose appearance near the top of a file marks it generated.
markers = ["@generated by my-tool"]
```

Name globs are the more dependable choice. A marker is only detected when the
top of the file is part of the diff, so if a file is changed only lower down,
its marker never appears in the diff and the rule does not fire. A name glob
matches on the path and so is not affected by which lines changed.

### Keybindings

Override the keys bound to an action under `[keymap]`, keyed by action name.
Each value is a list of chords, and listing an action replaces its default
chords. A chord is one of:

- a single press, such as `q`;
- a modified press, such as `ctrl-f` or `shift-tab`;
- a sequence pressed in turn, separated by whitespace, such as `g g`.

```toml
[keymap]
line_down = ["j", "down"]
quit = ["q", "ctrl-c"]
```

To start from a clean slate and bind only what you list:

```toml
disable_default_keymap = true
```
