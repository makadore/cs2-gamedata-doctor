# Contributing

Thanks for looking. This is a small tool with one promise: when it says it is sure, it is right.
Most of the rules below exist to keep that promise.

## Before you start

- A bug fix or a new gamedata format: open a PR directly.
- A change to how functions or slots are matched, a new command, a new output field: open an
  issue first and say what you want to change and why. It saves you from writing something that
  cannot be merged.
- A wrong answer from the tool is the most useful report there is. See "Reporting a wrong
  answer" below; you do not need to fix it yourself.

## What a PR must pass

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI runs the same three. A PR with a red check is not reviewed.

## Rules

1. **One PR, one change.** A fix and a refactoring are two PRs. Small PRs get merged, large
   ones wait.
2. **A fix comes with a test that fails without it.** The tests run on the synthetic libraries
   in `tests/fixtures`. If the case needs a new class or function, add it to
   `tests/fixtures/src/old.cpp` / `new.cpp`, rebuild with `build.sh` (and `build.cmd` for the
   `.dll`), and commit the rebuilt files.
3. **Never commit game files.** No `libserver.so`, no `server.dll`, no pieces of them, no SDK
   headers. They are not ours to distribute. Name the build instead (`PatchVersion` and
   `ServerVersion` from `steam.inf`, or the depot manifest id) so others can fetch it.
4. **A change to matching needs numbers.** If you touch `diff.rs`, `fp.rs`, `sig.rs`, the
   alignment in `report.rs` or `sdkcheck.rs`, say in the PR what you ran it on and what
   changed: which entries are now right that were wrong or unplaced, and that nothing that was
   right became wrong. "Should be better" is not a measurement. The maintainer reruns the
   history check (14 updates, three projects) before merging such a PR; a change that adds one
   wrong answer at a level the tool writes automatically is not merged, however many entries it
   gains.
5. **Sure means sure.** Anything written into a user's file without asking (`apply` at the
   default level, `sdk --patch`) must rest on evidence, not on position or likeness alone.
   New heuristics start as a `human` line or a candidate, and move up only with history behind
   them.
6. **Output is an interface.** People parse the text and the `--json`. Do not rename a verdict,
   reorder columns or change a JSON field in a PR that is about something else. A deliberate
   change to them is its own PR and says so in the title.
7. **No panics on bad input.** The tool reads files it did not make. A truncated or hostile
   binary must come back as an error (see `tests/errors.rs`). No `unwrap` on anything read from
   a file.
8. **Dependencies.** A new crate needs a reason in the PR text. Prefer the standard library.
9. **Comments say why, not what.** Short, in English, only where the code cannot say it. No
   commented-out code, no decorative banners.
10. **You answer for every line you send**, whoever or whatever typed it. A PR whose author
    cannot explain it is closed.

## PR text

Keep it short: what was wrong or missing, what the change does, how you checked it. No
headings for the sake of headings. If it fixes an issue, write `Fixes #N`.

## Review and merge

- Every PR is reviewed by a maintainer. Review comments are answered or acted on, each of them.
- PRs are squash-merged; the PR title becomes the commit subject, so make it a sentence that
  says what changed (`sigs: follow a signature that starts inside a function`).
- A PR with no reply to a review for 30 days is closed. Reopen it when you are back.

## Reporting a wrong answer

Open an issue with the "Wrong answer" form. What is needed:

- the two builds (versions or manifest ids) and the platform;
- the gamedata entry, or the class and function;
- what the tool said (the line, or the `--json` row) and at which level;
- what is right, and how you know (the maintainers' commit, a disassembler, a crash).

Do not attach the binaries.

## Class maps

`classmaps/*.map` tie gamedata names that carry no class to their class. To add or fix one,
send the map and say where the class comes from (the line of the plugin's source that uses the
offset). To have a map picked automatically by file name, add it to `builtin_class_map` in
`src/gamedata.rs`.

## License

By sending a PR you agree that your contribution is released under the MIT license of this
repository.
