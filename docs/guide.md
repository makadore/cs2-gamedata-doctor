# gamedata-doctor user guide

This guide is for people who maintain a CS2 server plugin or a framework: CounterStrikeSharp,
CS2Fixes, cs2kz, a Metamod plugin of your own. You do not need to know how the tool works inside.
For that, and for how its accuracy was measured, see [internals.md](internals.md).

The tool is experimental (0.x). Treat every report as a diagnosis and every file it writes
as a draft: read it, test it on a server, then ship it.

Contents:

1. [The problem in one minute](#1-the-problem-in-one-minute)
2. [Install](#2-install)
3. [The short version](#3-the-short-version)
4. [Step 0: have the old build](#4-step-0-have-the-old-build)
5. [Fix a gamedata file: `apply`](#5-fix-a-gamedata-file-apply)
6. [Do I have to rebuild: `sdk`](#6-do-i-have-to-rebuild-sdk)
7. [Look without writing: `diff`, `sigs`, `dump`](#7-look-without-writing-diff-sigs-dump)
8. [Gamedata formats and class maps](#8-gamedata-formats-and-class-maps)
9. [Windows](#9-windows)
10. [Scripts and CI](#10-scripts-and-ci)
11. [How far to trust it](#11-how-far-to-trust-it)
12. [When something looks wrong](#12-when-something-looks-wrong)

## 1. The problem in one minute

A game update can break a plugin in three ways.

| What breaks | Where it lives | How it shows |
|---|---|---|
| A **vtable index** (offset) | gamedata: `"CBaseEntity_Teleport": { "offsets": { "linux": 164 } }` | Silently. The plugin calls the neighbouring function, the server crashes somewhere else, or does the wrong thing. |
| A **byte signature** | gamedata: `"signatures": { "linux": "55 48 89 E5 ..." }` | Loudly: "failed to find signature" at startup. |
| An **SDK header** | hl2sdk / s2sdk, compiled into your `.so` / `.dll` | Silently, like an index. No gamedata edit helps; the plugin must be rebuilt on fixed headers. |

gamedata-doctor compares the game's libraries before and after the update and tells you which of your
indices moved and where to, which signatures died and what the new ones are, and which SDK
classes no longer match the game. It needs no symbols and no disassembler on your side.

It always needs **two builds**: the one your files were correct for, and the new one.

## 2. Install

Download one file from the releases page:

- Linux: `gamedata-doctor-linux-x86_64` (static, runs on any distribution). `chmod +x` it.
- Windows: `gamedata-doctor-windows-x86_64.exe`.

Or build it with a current stable Rust: `cargo install --path .`

The same program reads Linux (`.so`) and Windows (`.dll`) game libraries, whichever system it
runs on. You can check Windows gamedata from a Linux machine and the other way round.

## 3. The short version

```
# once, BEFORE the game updates: save the libraries (about 300 MB, not the 70 GB game)
gamedata-doctor keep /path/to/cs2/game --store ~/cs2-builds
# prints: /home/you/cs2-builds/1.41.9.7-2000943-linux

# after the update
OLD=~/cs2-builds/1.41.9.7-2000943-linux
NEW=/path/to/cs2/game

# gamedata: new indices and new signatures, written into a copy
gamedata-doctor apply $OLD $NEW --gamedata gamedata.json --out gamedata.fixed.json

# SDK: do I have to rebuild, and which lines of my code are hit
gamedata-doctor sdk $OLD $NEW --sdk path/to/hl2sdk --source path/to/my/plugin/src
```

Read what each command prints. Lines marked `applied` were changed for you. Lines marked `human`
are yours to check. Nothing else in the file is touched.

## 4. Step 0: have the old build

An update overwrites the libraries. Once that happened, the old build is gone from your disk,
and without it there is nothing to compare with.

### Before the update: `keep`

```
gamedata-doctor keep /path/to/cs2/game --store ~/cs2-builds
```

- Copies only the `.so` / `.dll` files and `steam.inf`. The `addons` folders are skipped.
- The copy goes to `<store>/<PatchVersion>-<ServerVersion>-<os>`, for example
  `1.41.9.7-2000943-linux`.
- A version that is already there is compared with the game's files and left alone; if the
  folder holds other files under the same version name, `keep` stops with an error. Run it
  from cron, or in the script that
  updates your server, right before the update step.
- The path of the folder is printed on stdout, everything else on stderr:
  `OLD=$(gamedata-doctor keep game --store builds)`.

To save less, name the libraries your gamedata uses:

```
gamedata-doctor keep game --store builds --lib server --lib engine --lib networksystem   # about 55 MB
```

Library names are the ones a gamedata file uses: `server`, `engine`, `tier0`,
`networksystem`, `steamnetworkingsockets`.

The kept folder works in every command where a game directory does.

### After the update, with nothing kept

Download the previous build's depot with a depot downloader. The CS2 dedicated server is app
`730`; the Linux binaries are depot `2347773`, the Windows binaries depot `2347771`. The
manifest id of an older build is listed on SteamDB. Older manifests need a Steam account that
owns the game; the current one can be fetched anonymously.

### Files or directories

Every command takes either two files or two directories.

```
gamedata-doctor apply old/libserver.so new/libserver.so --gamedata g.json --out fixed.json   # one library
gamedata-doctor apply old/game         new/game         --gamedata g.json --out fixed.json   # every library
```

With directories the tool finds each library by name on both sides. Use directories unless you
have a reason not to: a gamedata file often names more than one library.

Where the libraries are in a CS2 install:

| | Linux | Windows |
|---|---|---|
| server | `game/csgo/bin/linuxsteamrt64/libserver.so` | `game/csgo/bin/win64/server.dll` |
| engine and the rest | `game/bin/linuxsteamrt64/libengine2.so` | `game/bin/win64/engine2.dll` |

## 5. Fix a gamedata file: `apply`

```
gamedata-doctor apply $OLD $NEW --gamedata gamedata.json --out gamedata.fixed.json
```

`apply` reads your file, works out what the update did to each entry, and writes a copy in
which only the values it is sure about are changed. Formatting, comments and everything else
stay byte for byte.

What the output looks like (the lines are from real runs on several files, put together to
show every kind):

```
wrote fixed.json: 1 index(es) and 1 signature(s) changed at level strong; 10 index(es) unchanged, 27 signature(s) still valid; 1 need a human, 1 index(es) look unmoved without proof
  applied   Base_beta                             3 -> 4    Strong
  applied   ReplyConnection                       new signature
                                                    55 41 B8 FF FF FF FF B9 00 01 00 00 48 89 E5
                                                    evidence Strings, likeness 88 %; in the history this level was right 63 of 63 times
  human     Derived_zeta                          GUESS 6 -> 7: similarity guess inside a changed region, check by hand (7 -> 8 slots)
  same?     CBaseEntity_IsPlayerPawn              LIKELY 170 -> 170: same-shaped twins placed by their neighbours, call targets not compared (239 -> 239 slots)
```

### The three kinds of lines

| Line | Meaning | What you do |
|---|---|---|
| `applied` | The value was changed in the output file. | Nothing. Test your plugin as usual. |
| `human` | The entry moved or broke, and the tool is not sure enough to write it. Its best answer is in the text. | Check this one by hand. The answer shown is a good place to start. |
| `same?` | The tool's best answer is "it did not move", but on weak evidence. | Usually nothing. If the plugin misbehaves around this function, look here first. |
| `UNCHECKED` | The tool did not look at this entry at all: its library was not found on both sides, or its name could not be tied to a class. | Give the missing library or a class map (section 8), or check it by hand. |

Entries that are simply fine are counted in the first line and not listed. The first line
also says how many entries were NOT CHECKED: "0 need a human" means something only when
that number is 0 too.

### Signatures in the report

For a new signature the report gives the evidence it rests on, how alike the old and the new
function are, and how often that kind of evidence was right in the tool's history check:

```
evidence Strings, likeness 91 %; in the history this level was right 63 of 63 times
```

"Likeness" is a similarity score, not a probability. The history count is the number to go by.

When the function was not found, you get up to three candidates, each with a ready signature:

```
human     CCSNavArea_IsValidNavMesh   BROKEN: matches nowhere in the new build and the function was not found; by hand
            candidate 0x180dc9950, likeness 100 %: 48 83 3D ? ? ? ? 00 0F 95 C0 C3 CC CC CC CC 40 53
            candidate 0x180188920, likeness 99 %: 48 83 79 18 00 0F 95 C0 C3 ...
```

Open the address in your disassembler, confirm, paste the signature.

A regenerated signature is always checked to match exactly once in the new build.

### How much it may write: `--level`

| `--level` | Indices written | Signatures written |
|---|---|---|
| `strong` (default) | Only shifts proven by an exact function match. | Exact match, or the function's own string literals. |
| `historical` | Same as `strong`. | Also the kinds of evidence that were checked 50 times or more in the history and were never wrong there. |
| `likely` | Also `LIKELY` shifts. | Everything the tool found. |

A `GUESS` is never written at any level. Start with the default. `historical` rests on the
same past updates the tool was tuned on: it is a record of the past, not a proof for the next
update. Read the history table in [internals.md](internals.md) before using it. Use `likely` when you will test every
entry anyway and want the best first draft.

### Writing in place

`--out` does not replace a file that already exists unless you add `--force`; it makes no
backup. `--in-place` does:

```
gamedata-doctor apply $OLD $NEW --gamedata gamedata.json --in-place
```

The original is kept next to the file as `gamedata.json.<hash>.bak`. The write goes through a
temporary file and a rename, so an interrupted run never leaves a half-written file.

**Run `apply` once per update.** It maps a file that is correct for the old build onto the new
build. If you run it again on the already fixed file with the same two builds, it will shift
the values a second time. If in doubt, restore the `.bak` and run again.

## 6. Do I have to rebuild: `sdk`

Gamedata is not the only thing that goes stale. Your plugin is compiled against SDK headers
that declare the game's interfaces: `IVEngineServer2`, `INetworkMessages` and so on. When the
game inserts a virtual function into one of them, every call your plugin makes to a later
function of that interface goes to the wrong slot. Only fixed headers and a rebuild cure that.

```
gamedata-doctor sdk $OLD $NEW --sdk path/to/hl2sdk
```

Example (CS2 update of 22 October 2026, SDK from before the fix):

```
BROKEN    IVEngineServer2 via CEngineServer [122 -> 123 slots]: 57 of the 122 declared slot(s) moved
          (55 proven by fingerprint): SynchronouslySpawnGroup 65 -> 66, SetTimescale 67 -> 68, ...
          (public/engine2/ivengineserver2.h:71)
            fix: a placeholder for new slot 65 in front of IVEngineServer2::SynchronouslySpawnGroup
111 header class(es) found in 2 libraries: 110 unchanged, 1 BROKEN, 0 GONE, 0 suspect, 0 not checkable
```

### The verdicts

| Verdict | Meaning | What you do |
|---|---|---|
| `BROKEN` | Functions the header declares are now at other slots, or the header declares more than the game has. | Rebuild on fixed headers if your plugin uses the class. |
| `GONE` | The class is not in the new build at all. | The interface was redesigned. Code that used it has to change. |
| `suspect` | A slot moved, but only position says so. Often a new function at the very end, which is harmless. | Keep an eye on it. No action without other signs. |
| not checkable | The header could not be matched with the old build, or it already has the size of the new table (written for the new build: the binaries carry no names, so its order cannot be confirmed). Shown with `--all`. | Nothing; the tool has no opinion. |

No output above the summary line means the headers still describe the game.

### Is my plugin affected: `--source`

Not every broken class is your problem. Point the tool at your source:

```
gamedata-doctor sdk $OLD $NEW --sdk path/to/hl2sdk --source path/to/plugin/src
```

Under each finding it lists the places that look like a call or a hook of a function that
moved. This is a text search, so every line is a candidate to look at, not a proof:

```
BROKEN    INetworkMessages via CNetworkMessages [40 -> 37 slots]: ...
            in the source (text search, candidates): 0 with the class spelled out, 7 call(s) by a name only this class has, 0 by a name other classes have too, 4 mention(s) of the class
              core/UserMessage.cpp:23  GetNetMessageInfo
              core/UserMessage.h:107  FindNetworkMessagePartial
BROKEN    IFlattenedSerializers via CFlattenedSerializers [45 -> 46 slots]: ...
            in the source: no use found
source: 94 file(s) read; 1 of the 2 broken or gone class(es) are used there
```

- A listed line calls the wrong function on the new build until you rebuild on fixed headers.
- `no use found` means the search found nothing; you can most likely wait for the SDK to
  be fixed.
- "With the class spelled out" is `Class::Name` or a hook macro naming the class: the
  strongest kind. A call by a name only this class has is next. Calls by a name that other,
  intact classes also have (`GetName`, `Init`) are only counted; `--verbose` lists them, marked
  `(maybe)`. The tool reads text, not C++, and never knows which class an object is.

`--source` can be given several times.

### A patch for the headers: `--patch`

When a function was inserted or removed, the header can be repaired without knowing what the
new function is:

```
gamedata-doctor sdk $OLD $NEW --sdk path/to/hl2sdk --patch sdk.diff
cd path/to/hl2sdk && patch -p1 < sdk.diff      # or: git apply sdk.diff
```

The patch puts `virtual void gamedata_doctor_unknown_<slot>() {}` where a slot appeared and removes
the declaration of a slot that vanished. For the 22 October update it is one line, in the same
place where the SDK maintainers later declared the real function:

```
 	virtual void	MakeSpawnGroupActive( SpawnGroupHandle_t spawnGroup ) = 0;
+	virtual void gamedata_doctor_unknown_65() {}
 	virtual void	SynchronouslySpawnGroup( SpawnGroupHandle_t spawnGroup ) = 0;
```

The patch is a proposal to review, not a finished fix. After writing it the tool reads the
patched headers again and says how many of the broken classes now have a header of the new
table's size. That is all it can confirm: build everything that uses the headers and run it
before trusting the patch.

What to know:

- Only changes whose place is proven go into the patch: the slots on both sides must match
  by an exact fingerprint. The rest are printed as `fix:` lines with the note "the place is
  not proven", for you to place by hand.
- The placeholder has an empty body. If the new function is a pure virtual that your own
  class must implement, the placeholder hides that: the compiler will not ask you for it.
- The placeholder restores the layout. It does not tell you the new function's name or
  arguments, so you cannot call it. Replace it when the SDK is updated.
- It cannot help with a redesign (`GONE`).

### What do the headers say: `--class`

```
gamedata-doctor sdk x y --sdk path/to/hl2sdk --class IVEngineServer2
```

Prints the table the headers declare for a class, slot by slot, for Linux and for Windows. The
two build arguments are ignored here. Useful to find the index of a function by name.

## 7. Look without writing: `diff`, `sigs`, `dump`

```
gamedata-doctor diff $OLD_LIB $NEW_LIB --gamedata gamedata.json
```

`diff` takes two library files. It prints which classes changed and how, then each gamedata
index with a verdict:

```
CEngineServer: 122 -> 123 slots; shifted: 65..121+1; inserted new: [65]

gamedata offsets (linux):
  CBaseEntity_Teleport          164  unchanged
  SomeClass_SomeMethod           65  SHIFT 65 -> 66
  Other_Method                   40  LIKELY 40 -> 41: ...
  Third_Method                   12  GUESS 12 -> 13: ... check by hand
```

`SHIFT` is proven, `LIKELY` is probable, `GUESS` must be checked, `UNMATCHED` means no
counterpart was found. Without `--gamedata` you get only the class changes: a quick way to see
what an update did to a library.

```
gamedata-doctor sigs $OLD $NEW --gamedata gamedata.json
```

`sigs` reports on signatures only, with the same details as `apply`, and writes nothing unless
you add `--out FILE`.

```
gamedata-doctor dump libserver.so                       # classes and table sizes
gamedata-doctor dump libserver.so --class CCSPlayerPawn # every slot of one class
```

## 8. Gamedata formats and class maps

Three formats are read and written back unchanged apart from the values:

- CounterStrikeSharp `gamedata.json`
- CS2Fixes `*.jsonc` (comments are kept)
- Valve KeyValues `*.games.txt` (cs2kz, SourceMod style)

To place an index the tool has to know its class. It takes it from the entry name:
`CBaseEntity_Teleport`, `ISource2GameEntities::CheckTransmit`. Names without a class
(`Teleport`, `Respawn`) need a class map, a text file with one `Entry Class` pair per line:

```
Teleport CBaseEntity
Respawn CCSPlayerPawn
```

```
gamedata-doctor apply $OLD $NEW --gamedata my.games.txt --class-map my.map --out fixed.txt
```

Maps for cs2kz, CS2Fixes and CounterStrikeSharp are built in and used automatically when the
path of the gamedata file contains `cs2kz`, `cs2fixes` or `counterstrikesharp` (the file name,
or a folder above it as in `addons/counterstrikesharp/gamedata/gamedata.json`); the tool says
so when it does. `--class-map` replaces the built-in map.

A name the tool cannot tie to any class is reported as `UNCHECKED`. So is a number larger
than its class's table: it may be a field offset, but it may as well be a stale index or one
tied to the wrong class, and the binary cannot tell which. Only you can: if an entry is not a
virtual-table index at all (`GameEntitySystem` in CounterStrikeSharp), say so in the class map
with a dash, and it is reported as out of scope instead:

```
GameEntitySystem -
```

## 9. Windows

Give the Windows libraries and the tool reads the `windows` values of the gamedata:

```
gamedata-doctor apply old-win/game new-win/game --gamedata gamedata.json --out fixed.json
```

Indices differ between the platforms, so each platform is checked against its own binaries.
To fix both in one file, run `apply` twice, feeding the output of the first run to the second:

```
gamedata-doctor apply old-linux new-linux --gamedata gamedata.json --out step1.json
gamedata-doctor apply old-win   new-win   --gamedata step1.json    --out gamedata.fixed.json
```

Mixing a Linux and a Windows build in one run is refused.

## 10. Scripts and CI

- `--json` on every command prints a machine-readable report. `apply --json` carries a
  `schema_version` field, `complete` (true only when every entry was checked and none is
  left open) and `entries`: one row per entry and platform with a `status` of `checked_same`,
  `written`, `needs_human`, `same_unproven`, `not_checked` or `out_of_scope`.
- `apply --strict` exits with status 2 unless the run is complete: nothing left for a human,
  nothing unproven (`same?`), nothing unchecked. `sigs` exits with 2 when a library it needed
  was not found. A strict run is all or nothing: when it is not complete it writes no file
  and makes no backup. For a draft of an incomplete result, run without `--strict` and read
  `complete` and `entries` from `--json`.
- Exit status 0 means the command ran; 1 means it could not (bad path, unreadable file).

A minimal update script:

```sh
#!/bin/sh
set -e
OLD=$(gamedata-doctor keep "$GAME" --store "$BUILDS")     # the build before the update
steamcmd +force_install_dir "$SERVER" +login anonymous +app_update 730 +quit
gamedata-doctor apply "$OLD" "$GAME" --gamedata gamedata.json --out gamedata.new.json --force --strict \
  || echo "gamedata is not fully checked: nothing written, see the report above"
gamedata-doctor sdk "$OLD" "$GAME" --sdk "$SDK" --source src
```

## 11. How far to trust it

The tool was checked against what the maintainers of CounterStrikeSharp, CS2Fixes and cs2kz
actually committed after 14 CS2 updates.

| | Result |
|---|---|
| Indices marked `SHIFT` or `unchanged` (strong), Linux | 132 right, 0 wrong, 1 disputed |
| Indices marked `LIKELY`, Linux | 170 right, 0 wrong, 1 disputed |
| Indices marked `GUESS`, Linux | 65 right, 1 wrong. Never written automatically. |
| Broken signatures, Linux | 124 broken: 110 found right, 0 wrong, 14 left to a person |
| Broken signatures, Windows | 120 broken: 110 found right, 1 wrong, 9 left to a person |

These are measurements on past updates, not guarantees. The scripts and the list of builds behind these numbers are not in this repository yet, so
they cannot be reproduced from it. A "proven" match means two functions
have the same code shape; in rare cases two different functions do. Always start your server
and test after an update. The full tables and how they were made are in
[internals.md](internals.md).

What the tool cannot see at all:

- A function that kept its slot but changed its arguments or meaning.
- Field offsets inside structures.
- Anything in a library you did not give it.

## 12. When something looks wrong

**"library X: not found in both directories, N signature(s) NOT checked"**
One of the two directories does not contain that library. If you used `keep --lib`, keep that
library too. Signatures of that library were not checked at all; `apply` lists each of them as
`UNCHECKED`.

**"the old signature matches nowhere in the old build, nothing to follow"**
The gamedata file and the old build do not belong together: the file is newer or older than
that build. Use the gamedata as it was when it worked on the old build.

**"not a vtable entry we can place"**
The entry name carries no class. Add a class map (section 8).

**"index beyond the old table ..."**
The number is larger than the class's table: a stale index, an entry tied to the wrong class,
or a field offset. The tool leaves it alone and counts it as unchecked. If it is a field
offset, mark it with a dash in a class map (section 8).

**"interface entry: ... not checkable"**
An interface index is answered through the classes that implement it. If one of them cannot be
checked, the tool gives no answer rather than a possibly wrong one. Check by hand, or use
`gamedata-doctor dump --class` on the implementing class in both builds.

**"CONFLICT"**
The old signature still matches once in the new build, but at a function that does not look
like the old one. The game may have removed your function and a look-alike now matches. Check
by hand before trusting the old signature.

**Everything is reported as moved, or nothing matches**
Check that old and new are the builds you think: `gamedata-doctor dump <lib>` prints the version it
found in `steam.inf`. Check that both are for the same platform.

**`apply` shifted my values twice**
You ran it again on its own output. Restore the original (the `.bak` next to the file when
`--in-place` was used) and run once.
