# asana

Work with an Asana workspace from the command line, the way `gh` works with
GitHub: `asana task list`, `asana task view <gid>`, `asana task create --title`.

## Install

```sh
cargo install --path . --locked
# or build locally and use ./bin/asana
./scripts/build.sh
```

## Authentication

Create a personal access token at <https://app.asana.com/0/my-apps> and
export it:

```sh
export ASANA_TOKEN=2/1234567890/1234567890:abcdef...
```

`ASANA_ACCESS_TOKEN` is accepted too, and so is a config file at
`~/.config/asana/config.toml` (`$XDG_CONFIG_HOME/asana/config.toml`):

```toml
token = "2/1234567890/1234567890:abcdef..."
workspace = "Acme Inc"   # optional: a name or gid; needed if you belong to several
```

The workspace can also be chosen per call with `--workspace <name|gid>` or
the `ASANA_WORKSPACE` environment variable. `asana auth status` shows what is
in effect.

## Examples

```sh
# open tasks assigned to you, or the open tasks of a project
asana task list
asana task list --project "Roadmap" --state all --limit 50

# one task, with its comments
asana task view 1211234567890123 --comments

# create, then complete
asana task create --title "Fix the login page" --project Roadmap --assignee @me --due tomorrow
asana task complete 1211234567890123
```

Every command has `--help`; lists and views take `--json` for the raw API
objects and `--web` to open Asana in the browser. When stdout is not a
terminal, lists are tab-separated and views are `key:\tvalue` lines.

Exit codes follow `gh`: 0 success, 1 error, 2 cancelled, 4 authentication
required or rejected.

For development, `ASANA_BASE_URL` points the client at another API root
(for example a local mock server).
