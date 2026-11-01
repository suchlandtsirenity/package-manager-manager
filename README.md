# pmm — the package manager manager

One tool for every package manager on a Linux machine. apt, brew, flatpak,
snap, cargo, npm, pip, pipx, uv, nix and the rest each have their own commands,
their own idea of "installed", and no idea the others exist. pmm puts them
behind one CLI (and a desktop app): list and search everything at once, see
what's installed twice and which copy actually runs, update it all in one go,
and add or remove the managers themselves without losing track of what they held.

```console
$ pmm dupes
Same package from more than one manager

  bash              apt 5.2.21-2ubuntu4 · brew 5.3.20
                    runs brew's: bash, bashbug · apt's: clear_console, rbash
  pywal             pip 3.3.0 · pipx 3.8.13
                    runs pipx's: wal
  zstd              apt 1.5.5+dfsg2-2build1.1 · brew 1.5.7_1
                    runs brew's: pzstd, unzstd, zstd, zstdcat, +3 more

Same command in more than one place on PATH (the first one runs)

  brew hides apt                                             55 commands   bash, bashbug, bunzip2, bzcat, +51 more
  rustup hides no manager (/usr/local/bin)                   11 commands   cargo, cargo-clippy, cargo-fmt, clippy-driver, +7 more
  pip hides apt                                              5 commands    f2py, keyring, markdown-it, markdown_py, pygmentize
```

<!-- TODO: screenshot of pmm-gui's Installed view -->

## What it does

```console
$ pmm managers
Package managers: 14 of 14 installed

  apt      2.8.3     system         Debian/Ubuntu system packages
  brew     7.0.11    standalone     Homebrew on Linux
  cargo    1.99.0    via rustup     Rust packages (cargo install)
  go       1.27.3    via brew       Go tools (go install)
  npm      10.9.0    via nvm        Node.js packages (global installs)
  ...
```

| Command | |
|---|---|
| `pmm managers` | Every manager pmm knows, whether it's installed, and what it was installed through |
| `pmm status` | Package totals, duplicate count, when each manager last changed |
| `pmm list [-m <manager>]` | Counts per manager, or one manager's full list |
| `pmm search <query>` | Installed matches first (from cache, instant), then each repository's results as they arrive |
| `pmm dupes` | Packages installed by more than one manager, and commands shadowed on `PATH`, credited to the manager that put them there |
| `pmm outdated` | Installed packages with a newer version, per manager |
| `pmm update [--dry-run]` | Update every manager in turn; the whole plan, `sudo` steps included, is printed first |
| `pmm info / install / uninstall / upgrade <pkg>` | Per-package actions; asks which manager when more than one has it |
| `pmm add <manager> [--via <method>]` | Install a manager, and whatever it's installed through |
| `pmm remove <manager>` | Remove a manager, after showing everything that goes with it and saving a restore list |
| `pmm restore [<list>]` | Reinstall a removed manager's packages, with the same manager or another |

## Managers are data, not code

Each package manager is a TOML **recipe**: how to detect it, list its packages,
search its repository, update it, install and remove packages, and undo its
own installation. The 14 built-in recipes live in
[`crates/pmm-core/recipes/`](crates/pmm-core/recipes/); a file in
`~/.config/pmm/managers/<name>.toml` adds a new manager or overrides a
built-in. Supporting another manager never needs a code change.

```toml
# recipes/apt.toml (excerpt)
[list]
command = ["dpkg-query", "-W", "-f", "${db:Status-Abbrev}\t${Package}\t${Version}\n"]
# "ii" = installed, "hi" = installed and held; "rc" (removed, config left) is skipped.
pattern = '^[ih]i\s*\t(?P<name>[^\t]+)\t(?P<version>.*)$'
watch = ["/var/lib/dpkg/status"]

[update]
# apt-get rather than apt: apt warns that its output isn't stable for scripts.
commands = [["apt-get", "update"], ["apt-get", "-y", "--with-new-pkgs", "upgrade"]]
sudo = true
```

The few managers a regex can't describe (npm globals under every nvm Node
version, cargo's `.crates2.json`) point at a named parser written in Rust.

## Safety

pmm runs other programs' installers and sometimes `sudo`, so it is careful
about it:

- **Nothing runs as root silently.** Every command is shown before it runs, and
  the sudo password is asked once, up front.
- **Package names are validated** (letters, digits, `@/._+:-`, no `..`) before
  they're put into a command.
- **`pmm remove` shows what you'd lose first**: the manager's packages, and other
  managers installed through it (removing brew takes go with it). It saves a
  restore list before deleting anything, and removes nothing if any of those
  lists can't be read. apt is protected and can't be removed.
- **Deleting files stays inside your home folder.**

## Install

Linux only for now, Debian/Ubuntu first (developed on Pop!_OS 24.04). Needs
Rust 1.85+.

```sh
cargo install --git https://github.com/suchlandtsirenity/pmm pmm
```

Or from a clone:

```sh
cargo build --release          # the CLI: target/release/pmm
cargo build --release -p pmm-gui   # the desktop app (Rust 1.93+, libcosmic)
```

## How it's built

A cargo workspace of three crates:

| Crate | |
|---|---|
| `pmm-core` | All the behaviour: recipes, detection, inventory, cache, every action |
| `pmm` | The CLI, a thin `clap` layer over pmm-core |
| `pmm-gui` | A [libcosmic](https://github.com/pop-os/libcosmic) desktop app over pmm-core |

Actions report progress as a stream of events, so the CLI prints them and the
GUI shows them live; swapping the GUI toolkit would only touch `pmm-gui`.
Package lists are cached and reused until a file the manager writes to (like
`/var/lib/dpkg/status`) changes, so search is instant.

The full design, including every command's exact behaviour, is in
[SPEC.md](SPEC.md).

```sh
cargo test     # pmm-core and the CLI (the GUI is skipped by default)
```

Recipe patterns are tested against output captured from the real commands.

## Status

The CLI is feature-complete for its first version. The GUI covers browsing,
search, package pages, updates and an activity log; adding and removing
managers from the GUI is still to come (the CLI does it today). Not yet on
crates.io.

## License

[MIT](LICENSE)
