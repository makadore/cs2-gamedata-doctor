# gamedata-doctor: internals and accuracy

How the tool works and how it was checked. For how to use it, see the [user guide](guide.md).

The history checks below were run with scripts and a set of game builds that are not in this
repository yet: the numbers cannot be reproduced from it as it stands.

Finds how the virtual tables of a Source 2 server binary changed between two builds, and maps the
vtable indices in your gamedata from the old build to the new one. No symbols, no IDA: everything
comes from the RTTI the compiler leaves in a stripped binary and from the code of the functions
themselves.

Built for CS2 plugin frameworks (CounterStrikeSharp, CS2Fixes, cs2kz, Metamod plugins) whose
gamedata holds entries like `CCSPlayer_ItemServices_GiveNamedItem = 24`. A game update that inserts
one virtual method shifts every index behind it; the plugin then calls a neighbouring function with
the wrong arguments and the server crashes somewhere else. Signature breaks are loud, index breaks
are silent. Nothing in the ecosystem checked indices automatically; this does.

## What it does

```
gamedata-doctor diff old/libserver.so new/libserver.so --gamedata gamedata.json
```

- reads both binaries: sections, dynamic relocations, `.eh_frame`;
- finds every class's vtables through the RTTI typeinfo objects (names, bases, offsets);
- fingerprints every slot's function: decoded instructions with branch targets, RIP-relative
  operands and image addresses masked, so a function that did not change hashes equal in both builds;
- aligns the slots of each class: unique exact fingerprints first, then unique opcode shapes, then a
  similarity alignment inside whatever is left;
- reports per class which slots were inserted, removed or shifted, and per gamedata entry the new
  index with a confidence level;
- resolves interface entries (`ISource2GameEntities::CheckTransmit`) through the classes implementing
  the interface, following non-virtual bases only, and answers only when the implementations agree.

Example, the CS2 update of 22 October 2026 (`libengine2.so`):

```
CEngineServer: 122 -> 123 slots; shifted: 65..121+1; inserted new: [65]
```

The hl2sdk commit for that update added exactly one virtual to `IVEngineServer2`; counting the
headers (IAppSystem 11 + ISource2Engine 7 + declaration 47) gives slot 65. The tool found it from the
two binaries in 0.05 s.

## Confidence levels

Every placed entry carries one of four levels. Only the first two are shown as recommendations.

| Level | Evidence | Shown as |
|---|---|---|
| `Strong` | exact fingerprint unique in both tables | `unchanged` / `SHIFT a -> b` |
| `LikelyByShape` | opcode shape unique in both tables, constants changed | `LIKELY a -> b` |
| `LikelyByPosition` | identical-looking bodies placed by their neighbours; call targets are not compared | `LIKELY a -> b` |
| `Guess` | similarity alignment inside a rewritten region | `GUESS a -> b`, check by hand |

`Strong` is a structural match, not a semantic proof: two functions of the same shape that only
call different targets hash equal. The verdict text says so.

## Validation against history

The tool was run against what maintainers actually committed after 14 CS2 updates (October 2024,
then July 2025 to September 2026) in three projects: CounterStrikeSharp, CS2Fixes and cs2kz. 526
gamedata entries, 243 of them changed by the maintainers.

| Level | changed entries | unchanged entries |
|---|---|---|
| Strong | 37 correct, 1 disputed | 95 correct, 0 wrong |
| LikelyByShape | 106 correct, 1 disputed | 28 correct, 0 wrong |
| LikelyByPosition | 21 correct | 15 correct |
| Guess | 47 correct, 1 wrong | 18 correct |
| not placed | 29 (field offsets, engine interfaces, names without a class map) | 127 |

The two disputed entries are one cs2kz value whose maintainer history contradicts itself; the tool's
chain of values agrees with the maintainer's later corrections. The baseline "keep the old index"
scores 0 of 243 on the changed entries.

Twelve of the updates were the diagnostic set: class maps, build attachments and the similarity
alignment were tuned there. The two October 2024 updates were downloaded only after that, with their
build attachments written down in advance. Read as first run, before any change: CS2Fixes 10 of 11
placed changed entries right (the miss a `Guess`) plus 3 of 3, cs2kz 5 of 5 changed entries right but
6 predictions on entries the maintainer had left alone. Those six turned out to be the maintainer's
lag: cs2kz changed exactly those entries to the tool's values three days later. The oracle rule that
followed (an update's truth is the state after the maintainer's last fix within 14 days) was added
after seeing that set, so the recomputed 24 of 25 is no longer a fully untouched test of the whole
method; the `Guess` alignment itself was. The next real update is the untouched control. This remains
a retrospective estimate, not a probability. The scripts that build the oracle and run it live in the
author's workspace and are not part of this crate yet.

## Signatures (experimental)

```
gamedata-doctor sigs old/libserver.so new/libserver.so --gamedata gamedata.json [--out fixed.json] [--level strong|likely]
gamedata-doctor sigs old/game new/game --gamedata gamedata.json      # every library the file names
```

A byte signature breaks loudly, and far more often than an index. `sigs` follows each one: the
old pattern's single match in the old build names a function; the same function is looked for in
the new build; if the old bytes still match once, and at that function, nothing changes;
otherwise a new pattern is cut from the new function's first bytes, with every 4-byte
displacement, branch target and address wildcarded, just long enough to be unique (up to 400
bytes: a function with a twin needs a long one).

Given two files, `sigs` checks the signatures of that one library. Given two directories (two
game installs or depot downloads) it finds each library the gamedata names (`server`, `engine`,
`steamnetworkingsockets`, ...) in both and checks them all; `addons` folders are skipped, and a
library missing on either side is reported with the number of signatures left unchecked. An
entry marked `allow_multi_match` is only checked for still matching somewhere.

The report is meant to be read by a person. Every regenerated signature comes with the level
of evidence, how alike the old and the new function are (a likeness, not a probability) and how
often that level was right on the history; a signature whose function was not found comes with
the three most similar unmatched functions and a signature for each:

```
CS_Script_SetModel         BROKEN: new signature for 0xae2640 (Strings)
                             55 BE 01 00 00 00 48 89 E5 41 57 ...
                             evidence Strings, likeness 91 %; in the history this level was right 63 of 63 times
CCSNavArea_IsValidNavMesh  BROKEN: matches nowhere in the new build and the function was not found; by hand
                             candidate 0x180dc9950, likeness 100 %: 48 83 3D ? ? ? ? 00 0F 95 C0 C3 CC CC CC CC 40 53
                             candidate 0x180188920, likeness 99 %: 48 83 79 18 00 0F 95 C0 C3 ...
```

The function is found by, in order: an exact fingerprint unique in both builds (`Exact`); string
literals only that function refers to (`Strings`); a unique opcode shape (`Shape`); its place
among the calls of already matched functions (`CallGraph`); its place in `.text` between matched
neighbours (`Neighbours`); being the one clearly most similar function among those its matched
callers, callees and neighbours lead to (`Context`). The last three also need the two bodies to
look alike (size, strings, matched callers and callees). A function still unmatched after all
that gets one more try (`Mutual`): its most similar unmatched function in the whole new image,
by what survives a rewrite (the mix of instructions, the object fields touched, strings, matched
callers and callees), accepted only when that function's own most similar old function is this
one. `--out` writes `Exact` and `Strings`
only (63 of 63 right in the history below, on both platforms together 66 of 66 with `Exact`);
`--level likely` writes the rest too, where the one wrong answer of the table sits (`Shape`). When the old bytes still match once but at a function that does not
look like the old one, the entry is a `CONFLICT` for a person, never resolved by the tool.

Against the same history (25 pairs, 14 updates, three projects), judged by where the maintainers'
own new signature points in the new build:

| | Linux | Windows |
|---|---|---|
| signatures judged | 949 | 967 |
| broken by the update | 124 | 120 |
| of those: right function found | 110 (89 %) | 110 (92 %) |
| of those: wrong function | 0 | 1 |
| of those: left to a person | 14 | 9 |
| not broken: confirmed | 816 | 838 |
| not broken: flagged as a conflict | 9 | 9 |
| not broken: wrong | 0 | 0 |

The matcher's thresholds were tuned on the Linux history; the Windows column was produced after
that, with two fixes it prompted (chained `.pdata` regions, a size floor for the weak levels), so
neither column is an untouched test.

The first update after that was CS2 1.41.9.7 (11.11.2026), run before the maintainers of
CounterStrikeSharp and CS2Fixes had reacted, Linux, old build 1.41.9.5. No gamedata index moved
and the tool said so. Of 126 signatures in four files (CounterStrikeSharp, CS2Fixes, cs2kz,
MultiAddonManager's three taken from its source) it flagged three: MultiAddonManager's
`ReplyConnection` (the cause of that day's outage; the regenerated pattern lands on the function
the authors' fix lands on), CS2Fixes' `CS_Script_SetModel` (gone, regenerated) and
CounterStrikeSharp's `ClientPrint` (now matching two functions; the regenerated pattern follows
what the old one pointed at, which was not the function the entry is named after - the tool
follows the signature, it does not know the intent). That run also prompted three changes: more
than one library per run, the longer pattern limit and `allow_multi_match`. A regenerated signature is always
verified to match exactly once in the new build, at the function found; whether that function is
the right one is what the levels and the table are about.

## Keeping the build before an update

Every command compares two builds, and an update overwrites the one it replaces. The tool reads
nothing but the libraries, so the game itself does not have to be kept twice: of a 73 GB
dedicated server install the Linux libraries are 325 MB, and the three a gamedata file usually
names (`server`, `engine`, `networksystem`) about 55 MB.

```
gamedata-doctor keep ~/cs2/game --store ~/cs2-builds                 # -> ~/cs2-builds/1.41.9.7-2000943-linux
gamedata-doctor keep ~/cs2/game --store ~/cs2-builds --lib server --lib engine
# ... the game updates ...
gamedata-doctor apply ~/cs2-builds/1.41.9.7-2000943-linux ~/cs2/game --gamedata gamedata.json --out fixed.json
```

`keep` copies the `.so` / `.dll` files (not the `addons` folders) and `steam.inf`, keeping their
paths, into a folder named after `PatchVersion` and `ServerVersion`. The folder works wherever a
game directory does. A version that is kept already is left as it was, so the command can run
before every update or on a timer; it prints the folder on stdout.

## SDK headers

```
gamedata-doctor sdk old/game new/game --sdk path/to/s2sdk        # every library found in both
gamedata-doctor sdk old/libengine2.so new/libengine2.so --sdk path/to/s2sdk
gamedata-doctor sdk x y --sdk path/to/s2sdk --class IVEngineServer2   # what the headers declare, slot by slot
gamedata-doctor sdk old/game new/game --sdk path/to/s2sdk --patch sdk.diff   # and a patch for the headers
```

A gamedata file is not the only thing an update breaks. Metamod, CounterStrikeSharp and every
native plugin are compiled against SDK headers, and a header that no longer describes the game
makes them call the wrong slot: no gamedata fix cures that, only a fixed header and a rebuild.
`sdk` reads the headers (the virtual functions of every class, in declaration order, with the
platform's `#if` branches and the ABI's rules for destructors and overrides), finds each class
in both builds and reports:

- `GONE`: the class is not in the new build;
- `BROKEN`: a slot the header declares moved, vanished or got a neighbour inserted before it,
  proven by a function fingerprint, or the header declares more slots than the new table has;
- `suspect`: slots moved by position only (the new slot may sit at the end);
- nothing, when the header already describes the new build.

Both outages this tool was written after are found this way, from the two builds and the SDK
of the day:

| update | SDK | result |
|---|---|---|
| 1.41.9.2 (22.10.2026) | before `Update IVEngineServer2 (#116)` | `BROKEN IVEngineServer2 via CEngineServer [122 -> 123 slots]: ... SynchronouslySpawnGroup 65 -> 66, ...` |
| 1.41.9.2 | after it | nothing |
| 1.41.9.7 (11.11.2026) | the pin of CounterStrikeSharp 1.0.378 | `GONE INetworkMessageInternal`, `GONE IProtobufBinding`, `BROKEN INetworkMessages [40 -> 37 slots]`, `BROKEN CNetMessage [8 -> 7]`, `BROKEN IFlattenedSerializers [45 -> 46]` |
| 1.41.9.7 | with s2sdk PR 124 | `BROKEN IFlattenedSerializers` (a slot inserted near index 8, 23 moved slots proven), `suspect INetChannel` |

It is not a C++ parser: macros that declare virtuals and unusual syntax are beyond it, and a
changed parameter list in an unchanged slot is invisible to any tool that looks at tables. A
class whose header declares more slots than the old table had is counted as not checkable
(`--all` lists them).

### Is my plugin affected

`--source DIR` (repeatable) is the source of a plugin built on that SDK. Under every finding the
tool lists where that source calls or hooks a function whose slot moved, and where it names a
class that is gone:

```
BROKEN    INetworkMessages via CNetworkMessages [40 -> 37 slots]: ...
            in the source (text search, candidates): 0 with the class spelled out, 7 call(s) by a name only this class has, 0 by a name other classes have too, 4 mention(s) of the class
              core/UserMessage.cpp:23  GetNetMessageInfo
              core/UserMessage.h:107  FindNetworkMessagePartial
BROKEN    IFlattenedSerializers via CFlattenedSerializers [45 -> 46 slots]: ...
            in the source: no use found
```

This reads text, not C++: a hook is `Class::Name` or `Class, Name` in a macro, a call is
`->Name(` or `.Name(`. The type of the object is not known, so every line is a candidate. A
name that intact classes declare too (`GetName`, `Init`) is only counted; `--verbose` lists
those as well.

### A patch for the headers

Where slots were inserted into or removed from the part of a table a header declares, the header
can be made to describe the new table again without knowing what the new functions are:
`--patch FILE` writes a unified diff (`patch -p1` in the SDK root) that puts
`virtual void gamedata_doctor_unknown_<slot>() {}` where a slot appeared and takes out the declaration of
a slot that vanished, as a proposal to review. It then reads the patched headers and says how
many of the broken classes now have a header of the new table's size; that is all two builds
can confirm, since they carry no names. Only changes whose
place is proven go into the patch: the slots on both sides of it must have been paired by their
own exact fingerprints, not by shape and not by position among look-alikes. The rest is printed as `fix:` lines for a
person. For the update of 2026-10-22 the patch is one line, in front of
`SynchronouslySpawnGroup` in `ivengineserver2.h`: the line where the SDK's maintainers later
declared `IsSpawnGroupLoadedForClient`. A placeholder makes the code built on the header call the
right slots again; it does not tell what the new function does, and it cannot help when an
interface was redesigned (2026-11-11: classes gone, nothing to patch).

## Usage

```
gamedata-doctor dump <bin> [--class NAME]
gamedata-doctor diff <old> <new> [--gamedata FILE]... [--class-map FILE] [--all] [--json]
gamedata-doctor keep <game dir> --store DIR [--lib NAME]... [--name NAME]
gamedata-doctor apply <old> <new> --gamedata FILE (--out FILE | --in-place) [--level strong|historical|likely] [--strict] [--json]
gamedata-doctor validate <old> <new> --before FILE --after FILE [--class-map FILE] [--json]
gamedata-doctor sigs <old> <new> --gamedata FILE [--out FILE] [--level strong|historical|likely] [--json]
gamedata-doctor sdk <old> <new> --sdk DIR [--source DIR]... [--all] [--class NAME] [--patch FILE] [--json]
gamedata-doctor validate-sigs <old> <new> --before FILE --after FILE [--json]
```

`apply` carries a whole gamedata file to the new build: the indices and the signatures. `<old>` and
`<new>` are two binaries, or two game directories, and then every library answers for the classes
and signatures it has. A regenerated signature is written at `--level strong` when it rests on an
exact fingerprint or the function's own strings, at `--level historical` also on the levels that were
judged at least 50 times in the history and never wrong (`CallGraph`, `Neighbours`), at
`--level likely` always. An index whose best pairing says "same slot" on evidence weaker than the
level is listed as `same?`: nothing to write, nothing known to be wrong. With `--json` the
report carries `schema_version`, `complete` and one status per entry and platform.

`apply` writes the new indices into a copy of the gamedata file, changing nothing but the numbers:
at `--level strong` only proven shifts, at `--level likely` also the `LIKELY` ones; a `GUESS` is never
written, it is listed for a human together with everything the tool could not place. `--strict` is all
or nothing: unless every entry was checked and none is left to a person, unproven or unchecked, no
file is written and the exit status is 2. With `--in-place` the original is kept
next to the file as `<file>.<hash>.bak`, one backup per distinct content, never overwritten, and the
write is refused if the file changed after it was read. Every write goes through a temporary file and
a rename, so an interrupted run leaves the old file or the new one, never a torn one; permission bits
are kept, the owner too when run as root, ACLs and extended attributes are not. `diff` never writes
anything.

`apply` maps a file that describes the old build onto the new one, once. Run it twice over the same
file and the indices, which now describe the new build, land on other functions of the old table and
shift again; the tool cannot tell, only the backups can. `apply` keeps no record of which build a
file was last written for.

Gamedata formats: CounterStrikeSharp `gamedata.json`, CS2Fixes `*.jsonc`, Valve KeyValues
`*.games.txt`. Entries are placed on a class by their name (`Class_Method`, `Class::Method`); for
files whose names carry no class (`Teleport`, `ControllerChangeTeam` in cs2kz) a class map is
needed, one `Entry Class` pair per line. The maps in `classmaps/` are built in and picked by the
file's path (`cs2kz`, `cs2fixes`, `counterstrikesharp`); a line `Entry -` declares an entry not to
be a virtual-table index; `--class-map FILE` replaces them, and is the way for any
other file.

`validate` is the history check: predict from the gamedata before an update, compare with the file
the maintainers committed after it, tally per confidence level.

Build: `cargo build --release` (Rust stable). Tests: `cargo test` (synthetic fixtures under
`tests/fixtures`, rebuilt from C++ by `tests/fixtures/build.sh`).

## Windows

`server.dll` and `engine2.dll` are read the same way: PE sections, the base relocation table for
the pointers, `.pdata` for exact function ranges (every function with unwind information has one;
tiny leaf functions without it fall back to the heuristic), and MSVC RTTI
(complete object locator, type descriptor, class hierarchy descriptor) for the classes, their
tables and their bases. A pure virtual slot points at the `_purecall` import thunk, which is
treated like `__cxa_pure_virtual`. The `windows` key of the gamedata is used when both binaries
are PE; mixing an ELF and a PE build is refused. Indices differ between the two compilers (MSVC
emits one destructor slot, GCC two; an override of a secondary base's method gets no primary
slot under MSVC), so each OS is diffed against its own binaries.

The history check of the previous section was repeated on the Windows depot: the same 14 updates,
25 pairs, 526 entries and 243 changes, against the maintainers' `windows` values.

| Level | changed entries | unchanged entries |
|---|---|---|
| Strong | 13 correct, 1 disputed | 86 correct, 0 wrong |
| LikelyByShape | 126 correct, 1 disputed | 33 correct, 0 wrong |
| LikelyByPosition | 21 correct | 18 correct |
| Guess | 50 correct, 1 disputed | 19 correct |
| not placed | 30 | 127 |

The three disputed rows are the same cs2kz entry as on Linux (`PlayerRunCommand`), whose maintainer
history contradicts itself on both platforms; the tool's chain of values is consistent on both. The
one `Guess` miss of the Linux run does not occur on Windows. Also checked: the 1.41.9.6 `server.dll`
(4 072 classes, 197 242 slots, 0.3 s) diffed against itself is 100 % identical, every Windows index
of the three projects' current gamedata lands inside its table, and the same synthetic fixtures
compiled by MSVC (`tests/fixtures/*.dll`, `build.cmd`) give the expected shifts, guesses and
interface verdicts.

## Limits

- Interface entries are answered only through non-virtual base paths; a virtual base is reported as
  not placed.
- `.eh_frame` gives exact function bounds where the binary has them; CS2's Linux binaries cover
  only part of their functions, the rest use a heuristic (next known function start, `ret` or a
  tail `jmp` out of the body followed by padding). PE binaries record almost every function in
  `.pdata`; leaf functions without unwind information use the same heuristic.
- Field offsets (`GameEntitySystem`, `CheckTransmitPlayerSlot`) are not vtable slots and are reported
  as such.
- A signature that starts inside a function whose body changed is located but not regenerated.
- `Guess` must never be applied automatically.

## License

MIT.
