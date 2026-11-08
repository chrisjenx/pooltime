# Cutover runbook: bro → pooltime

The exact sequence for moving a machine that runs bro-cli onto pooltime, in two phases: a **shadow
run** (pooltime serves a scratch profile next to bro, with accounts bro does not use), then the
**cutover** itself. For what `import bro` copies and why bro must be stopped first, see
[migrating-from-bro.md](migrating-from-bro.md).

Rules that hold throughout:

- bro keeps running on its port (3456) during the shadow run; pooltime uses its own (3460).
- pooltime never changes bro's `~/.claude-max-pool`, `~/.bro` or Keychain items, and never touches a
  profile you don't name (e.g. `~/.claude-work`).
- During the shadow run, pooltime gets only accounts that bro does **not** serve: two pools
  refreshing the same account would rotate each other's tokens.

## Phase 1: shadow run

Use pooltime v1.0.0 or later (`pooltime --version`).

1. **Sign in two accounts that bro does not use** (for example two accounts whose bro login has
   already expired). Each opens a browser; in paste mode, run it in your own terminal.

   ```sh
   pooltime accounts add <spare-1> --email <spare-1-email>
   pooltime accounts login <spare-1>
   pooltime accounts add <spare-2> --email <spare-2-email>
   pooltime accounts login <spare-2>
   pooltime accounts list
   ```

2. **Start pooltime in the foreground** on its default port (3460), next to bro:

   ```sh
   pooltime serve
   ```

   (If it refuses because bro is answering and imported accounts are enabled, you have imported
   bro accounts into this home already: use a fresh `POOLTIME_HOME` for the shadow run.)

3. **Connect one scratch profile** (a settings dir you don't use for real work), in another terminal:

   ```sh
   mkdir -p ~/.claude-pooltime-shadow
   pooltime connect ~/.claude-pooltime-shadow --statusline
   pooltime doctor
   ```

4. **Exercise it** with `CLAUDE_CONFIG_DIR=~/.claude-pooltime-shadow claude`: a streamed reply, a tool
   call, Esc to cancel mid-stream, each model family (Opus, Sonnet, Haiku, Fable if your accounts
   have it) and a Codex model if a Codex account is pooled. Then check what pooltime recorded:

   ```sh
   pooltime status
   pooltime logs -n 50
   pooltime open
   ```

   In the dashboard, **Requests** shows each request's attempt chain and **Events** the account
   timeline. `pooltime logs --request <id>` prints one chain. Anything unexpected becomes a bug with
   a regression test before the cutover.

5. **End the shadow run**: `pooltime disconnect ~/.claude-pooltime-shadow`, then Ctrl-C the
   foreground `serve`. The two spare accounts stay in pooltime.

## Phase 2: cutover

Do this only when you're ready to stop bro. Name your profiles explicitly; here `default`
(`~/.claude`) and `claude-personal` (`~/.claude-personal`). `~/.claude-work` is not named and is
left alone.

1. **Preview the import** (writes nothing):

   ```sh
   pooltime import bro --dry-run
   ```

2. **Stop bro**:

   ```sh
   bro pool down
   ```

3. **Import** (pooltime must not be running):

   ```sh
   pooltime import bro
   ```

   Keep its output: it lists what was imported and the cleanup commands for later. Don't run the
   cleanup until pooltime has served you for a while.

4. **Install the service, connect, verify**:

   ```sh
   pooltime service install
   pooltime connect default claude-personal --statusline --mcp
   pooltime doctor
   ```

   The cutover is complete when `pooltime doctor` shows every line ✓ (or `i`) and exits 0. Restart
   any Claude Code session that was open before `connect`.

## Rollback

```sh
pooltime disconnect default claude-personal
pooltime service uninstall
bro pool up
```

`disconnect` restores each profile's settings from before bro (from the imported snapshot). Accounts
that pooltime refreshed after the import have rotated tokens, so bro's copies of those are dead: sign
them in again in bro.
