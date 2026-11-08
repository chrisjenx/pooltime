# Migrating from bro

`pooltime import bro` copies a bro-cli pool into pooltime. It only **reads** bro's files
(`~/.claude-max-pool`, `~/.bro`) and Keychain items; it never changes or deletes them, and it never
writes to any Claude Code profile (so a profile such as `~/.claude-work` is left exactly as it is).
It writes only into pooltime's data dir.

## What is imported

| From bro | To pooltime |
|---|---|
| `accounts/<name>/.credentials.json`, else the Keychain item `Claude Code-credentials-<hash>` (may prompt) | Claude account `<name>` with its login expiry |
| `accounts/<name>/openai-auth.json` | Codex account `<name>` |
| `accounts/<name>/routing.json` | priority and weight |
| `usage.json` | rate-limit snapshots, active benches, and dead-token markers (only when they match the current token) |
| `sessions.json` | sticky sessions (stored hashed) |
| `models.json` | `models.toml` (skipped if pooltime already has one) |
| `tuning.json` | `config.toml` `[routing]` (keys you already set are left alone) |
| `~/.bro/pool-settings*.json` | per-profile snapshots, so `pooltime disconnect` restores your pre-bro settings |

An existing pooltime account with the same name is never overwritten. Snapshots that match no
profile on this machine are reported as orphans and not applied. Request counters are not imported.

## Cutover

Both tools refresh the same tokens, and a refresh rotates the token: once pooltime refreshes an
imported account, bro's copy is dead. So stop bro before pooltime serves the accounts.

1. **Preview** (writes nothing):

   ```sh
   pooltime import bro --dry-run
   ```

2. **Stop bro**:

   ```sh
   bro pool down
   ```

3. **Import.** pooltime must not be running (the import refuses, since a running daemon would
   overwrite what it seeds); stop it with Ctrl-C or `pooltime service uninstall` first.

   ```sh
   pooltime import bro
   ```

   It prints what it imported, the bro warning and the cleanup commands for later (it never runs
   them).

4. **Start pooltime and connect** the profiles you want (they still point at bro until you do):

   ```sh
   pooltime service install
   pooltime connect default claude-personal
   pooltime doctor
   ```

   Name profiles explicitly; `connect --all` would also take over any other `~/.claude*` profile.

Or let the wizard do steps 3–4 (it imports only while pooltime has no accounts yet):
`pooltime setup --import-bro --service --connect default,claude-personal --yes`.

If bro (or an unresponsive listener) is on bro's port 3456 when pooltime starts with imported
accounts, `pooltime serve` refuses to start. Stop bro, or, if you are sure it is stopped, start with
`pooltime serve --i-stopped-bro`.

## Running side by side

pooltime listens on 3460 and bro on 3456, so both can run during a trial. Give them **disjoint
accounts**: sign new or re-logged accounts into pooltime (`pooltime accounts login <name>`) rather
than importing ones bro is still using.

## Cleaning up bro

After a few days on pooltime, run the cleanup commands the import printed (`bro pool down`,
`rm -rf` of the imported bro account dirs, `security delete-generic-password` for the Keychain
items). Nothing is removed until you run them. The whole `~/.claude-max-pool` is only offered for
deletion when everything in it was imported; anything bro still holds alone (an account that was
skipped, failed, or whose id pooltime already had; models or tuning that were not imported) is
named on a `# kept in bro` line instead.

## Non-default locations

```sh
pooltime import bro --bro-pool /path/to/.claude-max-pool --bro-home /path/to/.bro --dry-run
```

Related: [Accounts](accounts.md), [Profiles](profiles.md), [parity checklist](parity.md).
