# claude-backup: restore Claude Code on a new machine

This folder is a backup of Claude Code made by `claude-backup.exe`.
The program in this folder can restore it right away. Rust is not needed.

## What's in the folder

| Path | Restores to |
|---|---|
| `.claude\` | `%USERPROFILE%\.claude` (settings, CLAUDE.md, skills, plugins, sessions, memory) |
| `.claude.json` | `%USERPROFILE%\.claude.json` (MCP servers, per-project state) |
| `appdata-caveman\` | `%APPDATA%\caveman` |
| `claude-backup.exe` | the backup/restore program |
| `claude-backup.conf` | its config (what to back up, what to skip, where to store it) |
| `last-run.log` | log of the latest scheduled run |

**Not in the backup:** the login token (`.credentials.json`), caches, and logs.
You log in again after restoring.

## Restore steps

### 1. Install the basics

```powershell
irm https://claude.ai/install.ps1 | iex   # Claude Code
```

Also install Git for Windows if it isn't there yet.
**Don't open Claude Code yet.** If it's already open, close every window first.

### 2. Bring the backup folder over

Plug in the drive or copy the folder to the new machine. Check that `dir` in `claude-backup.conf` matches where the folder actually is.
If the drive letter changed (say D: became E:), pick one of these:

- run with `--dir`, e.g. `.\claude-backup.exe restore --dir E:\claude-backup`
- or edit the conf to `dir = .` (uses the folder the exe is in)

### 3. Preview, then restore

Open PowerShell in the backup folder:

```powershell
.\claude-backup.exe restore --dry-run   # lists files that would be copied, changes nothing
.\claude-backup.exe restore
```

When it finishes you'll see `copied N (...), errors 0`.
If errors isn't 0, read the `error:` lines above it (usually a file still open in Claude, or a missing env var).

Restore overwrites files on the machine that differ from the backup. Files that exist only on the machine are left alone.

### 4. Log in again

```powershell
claude
```

Type `/login`, then check:

- `/plugin`: plugins should all be there. If one is broken, reinstall it from there.
- `/mcp`: MCP servers that run local programs need those programs installed (see the next section).

## Programs outside the backup to reinstall

Hooks, the statusline, and MCP servers often call programs installed separately (node, python, npx, other CLIs).
The backup doesn't include those programs, so make a list of them **before moving machines**:

```powershell
# commands used by hooks and the statusline
$s = Get-Content "$env:USERPROFILE\.claude\settings.json" -Raw | ConvertFrom-Json
$s.hooks.PSObject.Properties.Value.hooks.command | Sort-Object -Unique
$s.statusLine.command

# commands behind each MCP server
$c = Get-Content "$env:USERPROFILE\.claude.json" -Raw | ConvertFrom-Json
$c.mcpServers.PSObject.Properties | ForEach-Object { "$($_.Name): $($_.Value.command)" }
```

Install them on the new machine. If you install them at a different path, fix the paths in `%USERPROFILE%\.claude\settings.json` and `%USERPROFILE%\.claude.json`.
If an MCP server lives in a folder outside the backup (e.g. its own venv), add that folder with `item =` in the conf or set it up again.

## If the username on the new machine is different

Items in the conf use `%USERPROFILE%`, so files land in the right home automatically. Two things still need fixing by hand:

1. **The folder names in `projects\`** are built from the original path (e.g. `C--Users-OldName`). Rename them to the new user so old sessions, `--resume` and memory come back:

   ```powershell
   Rename-Item "$env:USERPROFILE\.claude\projects\C--Users-OldName" "C--Users-$env:USERNAME"
   ```

   Projects on other drives (e.g. `D--my-project`) don't need renaming if the project lives at the same path.

2. **Absolute paths** `C:\Users\OldName` in settings.json and .claude.json: search and replace them with the new path.

## Turn the daily backup back on

Run in PowerShell (not admin) from the backup folder:

```powershell
$dir = (Get-Location).Path
$exe = "$dir\claude-backup.exe"; $log = "$dir\last-run.log"
$action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless cmd.exe /c `"`"$exe`" > `"$log`" 2>&1`"" -WorkingDirectory $dir
$trigger = New-ScheduledTaskTrigger -Daily -At 21:00
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 2)
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName ClaudeBackup -Action $action -Trigger $trigger -Settings $settings -Principal $principal
Start-ScheduledTask ClaudeBackup; Get-ScheduledTaskInfo ClaudeBackup
```

`LastTaskResult` = 0 means it worked. The log is in `last-run.log`.

## Build from source

```powershell
cargo build --release
.\target\release\claude-backup.exe   # first run: creates the conf next to the exe and backs up to D:\claude-backup
```

After that, use the exe in the backup folder as your main copy (run from target, it copies its own conf over the one in the backup folder).
Uses only the standard library, no extra dependencies. Tests: `cargo test`.

## Commands

```text
claude-backup [backup|restore] [--config FILE] [--dir PATH] [--dry-run]
```

- Config: `--config`, otherwise `claude-backup.conf` next to the exe (created with defaults if missing)
- Backup folder: `--dir`, then env `CLAUDE_BACKUP_DIR`, then `dir` in the conf
- Skip rules (`skip =`) apply only to backup. Restore brings back everything in the folder.
- To also back up the login token, delete the `skip = .credentials.json` line (the token is then stored on the drive as plain text).
