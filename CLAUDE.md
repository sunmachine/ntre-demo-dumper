# Project guidelines

Rules for anyone, human or model, working in this repository. `ARCHITECTURE.md`
holds the design and the layer rules, `SCHEMA.md` the table semantics; this
file holds only the conventions that outlive any one piece of work.

## Commits

- No model attribution. No `Co-Authored-By` trailer naming a model, no
  "Generated with" line, no session link, no tool name in the message body. A
  commit is the repository's, not the tool's.
- The subject says what changed, the body says why. A routine judgement call
  is made and stated in the message rather than asked about.
- Nothing is pushed from a session. The developer pushes. A session ends with
  the list of branches, the count of commits on each, and the words "not
  pushed".
- One branch per issue, cut from `main`, so each can be reviewed and merged on
  its own.
- Before a commit, the CI checks pass locally: `cargo clippy --locked -- -D
  warnings` and `cargo test --release`.

## People

- No personal details in the tree: no real names, no email addresses, no
  machine names, no home directory paths. Write "the developer" for the person
  who owns the repository and "a contributor" for anyone else. Git author
  metadata and `LICENSE.md` are the two places a name belongs.
- Demo files and the databases built from them stay outside the tree. Refer
  to them by what they are, "a September 2026 SourceTV recording", not by
  path.
- Dates are absolute, `2026-09-12`, never "yesterday" or "last session".

## Writing style

The prose in docs, comments, commit bodies and reports follows one style, so
that a reader who was not in the session can follow the record start to
finish.

- Sentences, not fragments. A short sentence beats a label with a colon. One
  idea per sentence, with a verb.
- Say why. A comment or commit body that restates the code is noise; one that
  names the constraint the code answers is the record. Write for a newcomer to
  the codebase, and leave out the deliberation that led to the choice.
- No em dashes, no en dashes, no arrows, no parentheticals that carry the
  point. A comma, a full stop or a new sentence does the work.
- Numbers go in a table or on their own line, not inside prose, and only when
  they change what the reader does.
- Names of files, functions and flags appear in backticks and only when the
  reader has to go there. Describe the rest in words.
- Every claim about the code or a demo cites `file:line`, a commit hash, or
  the query that produced it. "Fixed in `82c04ee`" is a claim a reader can
  check; "fixed earlier" is not.
- A list holds parallel items, one or two sentences each. A line of argument
  stays in prose.
- Reports are written as an account: what happened, what was wrong, what
  fixed it, what was measured, what is still open. Open items name the user
  visible symptom first.

## Schema and format facts

- Every table and column is documented in `SCHEMA.md`; a unit test fails
  otherwise. When a column's meaning is a wire fact, say which message, event
  or property it comes from.
- Facts about the NT;RE wire format go in `ARCHITECTURE.md` under file format
  or net-message notes, with the demo or upstream source they were checked
  against. Upstream is the NeotokyoRebuild/neo repository; cite the file and,
  where it matters, the commit.
- Behaviour that differs between SourceTV and POV recordings, or between
  NT;RE builds, is tagged in `SCHEMA.md` rather than left for the reader to
  discover from an empty table.

## Working method

- One piece of work per session, two at most. A session starts by reading
  `ARCHITECTURE.md` and ends with a record of what changed.
- The orchestrating model directs, reviews every diff, runs the checks
  itself and verifies every claim a subagent makes against the code or the
  demo before repeating it. Mechanical work with a precise brief goes to a
  smaller model; pieces with design in them go to a larger one.
- Investigations write their scratch files, tools and databases outside the
  tree. A branch carries only the change under review.
- On the Linux workstation every toolchain step runs in the dev container:
  `distrobox enter ntre-dev -- cargo build --release` (container defined in
  `distrobox.ini`) or the Dev Container in `.devcontainer/`. The host runs
  only the built binary.
