# Retention

pooltime keeps logs, the request journal and usage history on disk and prunes them by itself. All
limits are `[retention]` keys in `config.toml` (see [Configuration](configuration.md#retention)).

| What | Default | How it is pruned |
|---|---|---|
| Log files (`logs/pooltime.log.<date>`, one per day) | 7 days, 50 MB | at daemon start: files older than `logs_days` go, then oldest first until under `logs_max_mb`; today's file is never deleted |
| Request journal (requests + attempts) | 30 days, 200 MB, 100,000 requests | hourly: rows older than `journal_days`, then oldest first while the database is over `journal_max_mb` or the journal over `journal_max_requests` (0 = no row cap) |
| Account timeline (Events view) and alert records | 30 days | hourly, with the journal's `journal_days` |
| Usage history (weekly burn) | hourly 30 days → daily 1 year → weekly 2 years | hourly: hourly rows older than `history_hourly_days` roll up into daily rows, daily rows older than `history_daily_days` into weekly, weekly rows older than `history_weekly_days` are deleted |

After pruning, the database is compacted (`VACUUM`) once more than 10% of it is free pages.

## Seeing disk use

```sh
pooltime doctor     # storage and retention lines
```

The dashboard's **Requests** view shows storage used (data dir, database, logs). `doctor` warns
only when the database is above 125% of `journal_max_mb` (pruning is not keeping up).

## Example: keep less

```toml
[retention]
logs_days = 3
journal_days = 7
journal_max_mb = 50
```

Then `pooltime service restart`.

Related: [Debugging](debugging.md).
