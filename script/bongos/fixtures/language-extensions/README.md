Fixtures of scenario 45-language-extensions. Each extension folder becomes a local git repository
when the scenario runs (it runs `git init` and commits the files under the run folder), served to
the app as a `file://` URL; no `.git` folder is committed here.

- `quokka`: a language extension with no grammar and no Rust part, on the fixture list.
- `wombat`: the same shape, not on the list, so installing it asks the trust question.
- `numbat`: on the list, with a `Cargo.toml` (committed as `Cargo.toml.fixture`, so the workspace
  never sees it) that needs extension API 0.99.0, so the preflight refuses it before any build.
- `languages.toml`: the fixture list, in the format of the embedded list. The scenario replaces
  `@<NAME>_URL@` and `@<NAME>_COMMIT@` and passes the result through `BONGOS_EXTENSION_LIST`.
- `project`: files of the three made-up languages, which no built-in language claims.
