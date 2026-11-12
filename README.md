# cs2-gamedata-doctor

Tells a CS2 plugin developer what a game update broke, and drafts the fixes it can.

> **Experimental (0.x).** The reports are diagnostics, and what the tool writes is a draft
> for you to review and test, not an update to ship unattended. See
> [How far to trust it](#how-far-to-trust-it).

It compares the game's libraries before and after an update. No symbols, no IDA: everything
comes from the RTTI the compiler leaves in a stripped binary and from the code of the functions
themselves. Linux and Windows builds are both read.

| After an update you want to know | Command |
|---|---|
| Which vtable indices in my gamedata moved, and where to | `gamedata-doctor apply` |
| Which byte signatures died, and what the new ones are | `gamedata-doctor apply` |
| Do my SDK headers still match the game, do I have to rebuild | `gamedata-doctor sdk` |
| Which lines of my plugin call a function that moved | `gamedata-doctor sdk --source` |
| How do I keep the old build without keeping 70 GB | `gamedata-doctor keep` |

Works with CounterStrikeSharp `gamedata.json`, CS2Fixes `*.jsonc` and KeyValues `*.games.txt`
(cs2kz, Metamod plugins), and with hl2sdk / s2sdk headers.

## Quick start

```
# before the update: save the libraries (about 300 MB)
gamedata-doctor keep /path/to/cs2/game --store ~/cs2-builds
# -> ~/cs2-builds/1.41.9.7-2000943-linux

# after the update
OLD=~/cs2-builds/1.41.9.7-2000943-linux
NEW=/path/to/cs2/game

gamedata-doctor apply $OLD $NEW --gamedata gamedata.json --out gamedata.fixed.json
gamedata-doctor sdk   $OLD $NEW --sdk path/to/hl2sdk --source path/to/plugin/src
```

`apply` changes only the values it is sure about and lists the rest for you. From the update of
11 November 2026, MultiAddonManager's signatures:

```
wrote fixed.json: 0 index(es) and 1 signature(s) changed at level strong; 0 index(es) unchanged, 2 signature(s) still valid; 0 need a human, 0 index(es) look unmoved without proof, 0 NOT CHECKED
  applied   ReplyConnection                                  new signature
                                                 55 41 B8 FF FF FF FF B9 00 01 00 00 48 89 E5
                                                 evidence Strings, likeness 88 %; in the history this level was right 63 of 63 times
```

`sdk` names the interfaces that no longer match the headers and the places in your code that
use them. Same update, CounterStrikeSharp's source:

```
BROKEN    INetworkMessages via CNetworkMessages [40 -> 37 slots]: ...
            in the source (text search, candidates): ... 7 call(s) by a name only this class has ...
              core/UserMessage.cpp:23  GetNetMessageInfo
```

## Install

Download `gamedata-doctor-linux-x86_64` (static, no dependencies) or `gamedata-doctor-windows-x86_64.exe` from the
releases page, or build it with a current stable Rust: `cargo install --path .`

## Documentation

- **[User guide](docs/guide.md)**: every command with examples, how to read the output, what
  to do with each kind of line, scripts and CI, troubleshooting.
- **[Internals and accuracy](docs/internals.md)**: how functions are matched, the confidence
  levels, the full results of the history check, limits.
- **[Contributing](CONTRIBUTING.md)**: how to report a wrong answer and what a PR needs.

## How far to trust it

Checked against what the maintainers of CounterStrikeSharp, CS2Fixes and cs2kz committed after
14 CS2 updates (Linux): of the indices the tool marks as proven or likely, 302 were right and
none wrong (2 disputed); of 124 broken signatures it found 110, with no wrong answer. A guess is
never written into a file. These are measurements on past updates, the same updates the tool
was tuned on, not guarantees: start your server and test after every update. The scripts and the
list of builds behind the numbers are not in this repository yet. `sdk --patch` is
a proposal: nothing built on a patched header has been compiled and run as part of the tool's
checks.

It cannot see a function that kept its slot but changed its arguments, and it does not handle
field offsets inside structures.

## Commands

```
gamedata-doctor keep <game dir> --store DIR [--lib NAME]... [--name NAME]
gamedata-doctor apply <old> <new> --gamedata FILE (--out FILE [--force] | --in-place) [--level strong|historical|likely] [--class-map FILE] [--strict] [--json]
gamedata-doctor sdk <old> <new> --sdk DIR [--source DIR]... [--verbose] [--patch FILE] [--class NAME] [--all] [--json]
gamedata-doctor diff <old lib> <new lib> [--gamedata FILE]... [--class-map FILE] [--all] [--json]
gamedata-doctor sigs <old> <new> --gamedata FILE [--out FILE] [--level strong|historical|likely] [--json]
gamedata-doctor dump <lib> [--class NAME]
```

`<old>` and `<new>` are two library files or two game directories. `gamedata-doctor <command> --help`
describes every option.

## Building and tests

`cargo build --release`, `cargo test`. The tests run on small synthetic libraries under
`tests/fixtures`, built from C++ by `tests/fixtures/build.sh` (Linux) and `build.cmd` (MSVC).

## License

MIT.
