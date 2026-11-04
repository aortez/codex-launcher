# codex-launcher

A terminal menu for choosing a [Codex CLI](https://github.com/openai/codex)
profile and launching Codex with it. You can also list, inspect, and launch
profiles from the command line, and another program can read and drive a
running menu through `codex-launcher ctl`.

```
┌ Profiles ──┐┌ gateway ─────────────────────────────────────────────────────┐
│  default   ││Team gateway with a key from the environment.                 │
│> gateway   ││                                                              │
│  local     ││model     gpt-team                                            │
│            ││provider  gateway (Team gateway)                              │
│            ││url       https://gateway.example.com/v1                      │
│            ││effort    high  (base)                                        │
│            ││sandbox   workspace-write  (base)                             │
│            ││approval  on-request  (base)                                  │
│            ││launch    codex --profile gateway                             │
│            ││                                                              │
│            ││── gateway.config.toml ──                                     │
│            ││# Team gateway with a key from the environment.               │
│            ││model = "gpt-team"                                            │
└────────────┘└────────────────────────────────────────────────── 1-13 of 21 ┘
↑/↓ move · Enter launch · PgUp/PgDn scroll · q quit
```

## Install

Requires Rust 1.94 or newer. Tested on Linux; it uses Unix sockets and `exec`, so
it does not build on Windows.

```sh
cargo install --locked --git https://github.com/aortez/codex-launcher
```

From a clone, `cargo install --locked --path .` does the same. Run either
command again to update. Cargo puts the binary in `~/.cargo/bin`; if that is
not on your `PATH`, add it or link the binary from a directory that is:

```sh
ln -s ~/.cargo/bin/codex-launcher ~/.local/bin/codex-launcher
```

`cargo uninstall codex-launcher` removes it; remove any link you made too.

## Profiles

Codex loads `$CODEX_HOME/<name>.config.toml` as a layer on top of
`$CODEX_HOME/config.toml` when it runs with `--profile <name>`. `CODEX_HOME`
defaults to `~/.codex`. The launcher lists every such file, sorted by name,
after a `default` entry that runs Codex with `config.toml` alone.

The details pane shows the settings Codex will use. A value marked `(base)`
comes from `config.toml` because the profile does not set it, and
`(built-in)` marks Codex's own default. The description is the comment block at
the top of the profile file. Values of keys that look like credentials
(`api_key`, `token`, `password`, header, `query_params`, and `env` tables, and
so on), passwords inside URLs, and URL query values are shown as `<redacted>`. Environment variable names
such as `env_key` stay visible.

Profile names may use letters, digits, `-`, and `_`, as Codex requires. Codex
ignores `--profile` for a name with no file; the launcher reports it as an
error instead.

### Wrappers

Some profiles need setup before Codex starts, such as a tunnel or an API key
loaded from a file. If an executable named `codex-<name>` is on `PATH`,
launching profile `<name>` runs it, with any extra arguments, instead of
`codex --profile <name>`. The wrapper must select the profile itself. The
details pane shows the command and the wrapper's path.

## Usage

```sh
codex-launcher                         # Open the menu.
codex-launcher -- resume --last        # Open the menu; pass the arguments to Codex.
codex-launcher local                   # Launch a profile without the menu.
codex-launcher local exec "fix tests"  # Arguments after the profile go to Codex.
codex-launcher local --dry-run         # Print the command instead of running it.
codex-launcher list                    # One line per profile; --json for JSON.
codex-launcher show gateway            # The details pane as text; --json for JSON.
```

```
$ codex-launcher list
PROFILE  MODEL            PROVIDER  EFFORT  DESCRIPTION
default  gpt-base         openai    high    Runs Codex with config.toml only, without a profile.
gateway  gpt-team         gateway   high    Team gateway with a key from the environment.
local    qwen3-coder:30b  ollama    medium  Local Ollama model on this machine.
```

`list`, `show`, `ctl`, and `help` are subcommands, so profiles with those names
can only be launched from the menu.

In the menu, `↑`/`↓` (or `k`/`j`) move, `Home`/`End` (or `g`/`G`) jump to the
first or last profile, `PgUp`/`PgDn` scroll the details, `Enter` launches, and
`q`, `Esc`, or `Ctrl-C` quits. Launching replaces the launcher process with
Codex, so Codex gets the terminal and nothing is left running.

## Using it from scripts

Each menu listens on a Unix socket at
`$XDG_RUNTIME_DIR/codex-launcher/<pid>.sock`, in a directory only you can
open. `ctl` finds the menu on its own when one is running; pass `--pid` when
there are several.

```sh
codex-launcher ctl instances       # Running menus: pid, terminal, directory, selection.
codex-launcher ctl state           # The menu, the selection, and the command Enter runs.
codex-launcher ctl screen          # The screen as plain text.
codex-launcher ctl key down down   # Press keys, then print the new state.
codex-launcher ctl select local    # Move the selection to a profile by name.
codex-launcher ctl launch          # Launch the selection, like pressing Enter.
codex-launcher ctl quit
```

Keys go through the same keymap as the keyboard. Key names are `up`, `down`,
`left`, `right`, `home`, `end`, `pgup`, `pgdn`, `enter`, `esc`, `tab`,
`backspace`, `space`, `ctrl-<letter>`, or a single character such as `q`. A
request with an unknown key or profile name changes nothing and exits with
status 1. A reply comes after the menu has applied the request and redrawn, so
it always matches the screen.

```
$ codex-launcher ctl key down
{
  "pid": 41237,
  "mode": "tui",
  "tty": "/dev/pts/3",
  "cwd": "/home/me/project",
  "codex_home": "/home/me/.codex",
  "status": "selecting",
  "selected": "gateway",
  "selected_index": 1,
  "command": ["codex", "--profile", "gateway"],
  "command_line": "codex --profile gateway",
  "menu": [
    {"name": "default", "model": "gpt-base", "provider": "openai", "effort": "high"},
    {"name": "gateway", "model": "gpt-team", "provider": "gateway", "effort": "high"},
    {"name": "local", "model": "qwen3-coder:30b", "provider": "ollama", "effort": "medium"}
  ],
  "profile": {"name": "gateway", "model": {"value": "gpt-team", "source": "profile"}, ...},
  "details": {"scroll": 0, "max_scroll": 0},
  "warnings": []
}
```

### Headless mode

`codex-launcher --headless` runs the same menu without a terminal. It draws to
an offscreen screen (`--size 100x30` by default) that `ctl screen` reads, and
when launched it prints the command on stdout and exits instead of running it.
Tests and agents can drive it the same way as a menu in a terminal.

```sh
codex-launcher --headless -- resume --last &
codex-launcher ctl select local
codex-launcher ctl launch       # The headless menu prints: codex --profile local resume --last
```

## Development

```sh
cargo test
cargo clippy --all-targets
```

`tests/fixtures/codex-home` is a made-up Codex home used by the tests and the
examples above.

## License

MIT. See [LICENSE](LICENSE).
