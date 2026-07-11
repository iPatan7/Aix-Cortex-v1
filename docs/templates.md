# Writing your own templates

A template is a promise: *this command does the thing, this check proves it,
this command undoes it, and this check proves the undo.* The built-ins are
Rust; yours are TOML files in `~/.cortex/templates/` (override the directory
with `CORTEX_TEMPLATE_DIR`). One template per file, loaded in filename order.

TOML rather than YAML because cortex already parses TOML for its policy file:
no extra dependency in the static binary, and both operator-edited formats
stay consistent.

## A complete example

```toml
# ~/.cortex/templates/app-flag.toml
id = "app.flag"                       # lowercase [a-z0-9.-_]; namespace it
summary = "Drop a marker flag for the app"
category = "app"                      # groups it in `cortex templates`
example = "cortex do app.flag path=/tmp/app.flag"

# Trigger keyword groups for the planner. A request matches only when EVERY
# group has at least one hit (typos within one edit count). Optional `verbs`
# raise the match score without being required. Omit `keywords` entirely to
# make the template reachable only via `cortex do`.
keywords = [["flag"], ["drop", "place", "set"]]
verbs = []

# True when the effect lives outside the filesystem (a daemon, a container):
# no overlay sandbox, the journaled inverse IS the undo. False (the default)
# runs the forward command inside an OverlayFS transaction first, and undo
# restores the captured files byte for byte.
host_side = true

# Shown in every plan: what undo does if the world moved in the meantime.
drift_note = "the flag is just a file; undo removes it"

# The four commands. {param} is replaced by the shell-QUOTED argument —
# a value can never inject shell syntax. {{ and }} are literal braces.
forward        = "touch {path}"
verify_forward = "test -e {path}"
inverse        = "rm -f {path}"
verify_inverse = "! test -e {path}"

[[params]]
name = "path"
about = "where the flag file goes"
kind = "abs-path"        # see kinds below
# default = "/tmp/flag"  # a param with no default is required
```

Check it loads, matches and binds:

```console
$ cortex templates show app.flag
$ cortex --plan "drop a flag at /tmp/app.flag"
```

## Parameter kinds

Values are validated before anything renders. Validation is a UX property —
every value is shell-quoted regardless — but a plan built from a value that
cannot be what the parameter means is refused early, with the expectation
named.

| kind | accepts |
|---|---|
| `ident` | names: letters, digits, `. _ - +` |
| `user` | a system login name |
| `image` | a container image reference (`nginx`, `redis:7`, `ghcr.io/a/b`) |
| `port` | a port number, 1–65535 |
| `port-mapping` | `host:container`, e.g. `8080:80` |
| `volume-mapping` | two absolute paths, `host:container`, e.g. `/srv/data:/data` |
| `env-var` | a `KEY=value` assignment (`NODE_ENV=production`) |
| `abs-path` | an absolute path with no `..` segments |
| `mode` | an octal file mode, e.g. `0644` |
| `line` | one line of free text |
| `text` | free text |

The planner extracts unambiguous shapes from free text (`port`,
`port-mapping`, `abs-path`, `user`, `image`), fills the rest from `key=value`
tokens and declared defaults, and — when something required is still missing
— prints the exact `cortex do` line with the holes marked.

## The rules

- **Both verifiers are mandatory.** A template whose inverse has no
  post-condition is refused at load time. An inverse that merely exits 0
  proves nothing (`echo done` exits 0); the post-condition is what makes
  undo trustworthy. `cortex verify --self` will exercise your template's
  full cycle wherever a fixture exists.
- **You cannot shadow a built-in.** A built-in's inverse was reviewed with
  the code; redefining `docker.run` in your home directory would replace a
  reviewed promise with an unreviewed one. Namespace your ids (`app.flag`,
  not `flag`).
- **Root trusts only root.** When cortex runs as root, template files must
  be root-owned and not group/world-writable — the same rule as
  `/etc/cortex/policy.toml`. A root binary running commands from a file any
  user can edit is sudo with extra steps.
- **Policy applies.** User templates evaluate as `template:<id>` like any
  other; an admin can `deny template:app.*` (or anything else) in the
  root-owned policy file.
- **Filesystem-backed templates** (`host_side = false`) may set
  `inverse = "true"` when the journal's file restore *is* the whole undo
  (there is no host-side action to compensate). Prefer a real inverse with a
  real post-condition whenever one exists — it is what lets the conformance
  suite prove your template round-trips.

## How matching works (so you can predict it)

Deterministic, same input → same plan:

1. `key=value` tokens are split out; the rest is lowercased words.
2. A template is a candidate when every `keywords` group has a hit. A word
   hits a keyword exactly, by light stemming (`running` → `run`), or within
   one typo for words of five letters or more — never two.
3. Candidates are scored by matched words (+ `verbs` hits); ties resolve in
   registry order — built-ins first, then your files in filename order.
4. The best candidate that fully binds wins — unless a *strictly*
   better-scoring template was only missing parameters, in which case that
   template teaches its command instead ("serve nginx over https" teaches
   nginx.tls's cert/key rather than silently planning plain HTTP). If
   nothing matched at all, the nearest templates are suggested.
5. An explicit `key=value` counts as a keyword hit for its parameter name,
   so `env=A=b` reaches a template whose trigger group contains "env".

## Composition

A request with conjunctions — `and`, `then`, `also`, `;`, `&&`, or a comma —
is split, and if **every** segment independently yields a complete plan, the
result is a composite: each step rendered and numbered before anything runs,
each step journaled separately, `cortex undo --all` unwinding them
newest-first. If any segment is ambiguous or missing a parameter, the whole
sentence is read as one request — composition can add plans, never change
what a single-intent sentence means.

```console
$ cortex try "install htop and open port 8080"
  · understood 2 steps
▸ step 1/2: package.install package=htop
  ...
▸ step 2/2: firewall.allow port=8080 proto=tcp
  ...
```

## Finding templates

```console
$ cortex templates                 # the whole catalog, by category
$ cortex templates show <id>       # one full contract, undo included
$ cortex templates search backup   # approximate search with examples
```
