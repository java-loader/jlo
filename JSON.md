# JSON Output

`jlo list --json` and `jlo current --json` print one JSON document on one line. Keys are only ever added — never
removed, renamed or retyped — so ignore keys you do not know, and expect new values in `status` and `source.kind`.
What J'Lo does not know is `null` rather than a missing key. The only exception is `source.path`, present only
for the two config kinds. On failure the exit status is 1 and stdout is empty — except when writing
stdout itself fails, so check the exit status.

```shell
jlo list --json | jq -r '.names[] | select(.update) | .name'   # names with an update
jlo current --json | jq -r .version
```

`jlo list --json` (also with `--offline`):

```json
{"names":[{"name":"25","major":25,"distribution":"temurin",
  "installed":[{"version":"25.0.1+8","path":"…/Contents/Home","active":true,"status":"installed"},
               {"version":"25.0.0+36","path":"…/Contents/Home","active":false,"status":"superseded"}],
  "offered":"25.0.2+10","update":true}]}
```

- `names` — every name installed or offered, newest major first.
- `installed` — newest build first; the first is the one the name stands for. `path` is its `JAVA_HOME`.
  `active`: your `JAVA_HOME` points into it. `status`: `installed`, or `superseded` by a newer build of the name.
  `active` and `status` are independent: a shell still on an old build is both.
- `offered` / `update` — Adoptium's newest build of the name, and whether it is newer than every installed build
  (`false` for a name that is only offered, not installed).
  Both `null` when J'Lo did not ask: with `--offline`, or for a pre-release stream whose version has been released.
  `offered: null` with `update: false`: asked, and Adoptium has no build for your machine. If any lookup fails,
  the command fails rather than print a partial list.

`jlo current --json`:

```json
{"name":"25","major":25,"distribution":"temurin","version":"25.0.1+8","path":"…/Contents/Home",
 "source":{"kind":"project_config","path":"/home/you/project/.jlorc"},"pinned_elsewhere":null}
```

- `path` — `JAVA_HOME` exactly as set, so compare `version`, not paths, against `jlo list`.
- `name`, `major`, `distribution`, `version` — `null` when `JAVA_HOME` is not a JDK J'Lo manages.
- `source` — why this JDK: `kind` is `project_config` or `default_config` (each with the file's absolute `path`),
  `newest_installed`, or `foreign` (`JAVA_HOME` set outside J'Lo); `null` when nothing accounts for it.
- `pinned_elsewhere` — `null`, or `{name, major, distribution, source}` of a config that asks for a different JDK.
