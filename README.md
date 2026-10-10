# Saishin

> There is also a Rust port with the same configuration file, options and
> behaviour under [`rust/`](rust/) (`cargo build --release`). It keeps its
> own keyring entry — see `rust/README.md` before switching binaries.

A small cross-platform .NET command line tool that keeps files up to date from
official GitHub releases — or from any other URL — using an INI configuration file.

It runs on **Windows, macOS and Linux** from the same source, downloads release
assets, and can unpack ZIP archives according to per-source rules.

It also handles sources that need a GitHub account: you authenticate **once**, the
token goes into your operating system's secure credential store, and later runs —
including scheduled ones — reuse it without ever prompting again.

---

## Contents

- [Requirements](#requirements)
- [Build and run](#build-and-run)
- [Quick start](#quick-start)
- [Authentication](#authentication)
- [Scheduled runs](#scheduled-runs)
- [Configuration reference](#configuration-reference)
- [Archive handling](#archive-handling)
- [Glob patterns](#glob-patterns)
- [Command line options](#command-line-options)
- [Exit codes](#exit-codes)
- [Project layout](#project-layout)
- [Third-party components](#third-party-components)

---

## Requirements

- [.NET SDK 8.0](https://dotnet.microsoft.com/download) or newer.

Nothing else. There is no runtime dependency on Mono, on Windows-only APIs, or on
platform-specific tooling.

---

## Build and run

```bash
git clone <this-repo>
cd saishin

dotnet build -c Release
dotnet run -c Release -- --config config.ini
```

To get a self-contained binary for a given platform:

```bash
# Linux x64
dotnet publish -c Release -r linux-x64 --self-contained true -o out/linux-x64

# Windows x64
dotnet publish -c Release -r win-x64   --self-contained true -o out/win-x64

# macOS arm64
dotnet publish -c Release -r osx-arm64 --self-contained true -o out/osx-arm64
```

Replace the identifiers with `linux-arm64`, `win-arm64` or `osx-x64` as needed.

---

## Quick start

1. Copy the example configuration and edit it:

   ```bash
   cp config.ini.example config.ini
   ```

2. Check what would happen without downloading anything:

   ```bash
   dotnet run -- --config config.ini --dry-run
   ```

3. Run it:

   ```bash
   dotnet run -- --config config.ini
   ```

Start with `--help` for the full option list.

---

## Authentication

Public GitHub releases need no credentials. Sources that are account-gated — a
private repository, or builds published only to signed-in accounts — set
`private = true`, and then a token is required.

The token is a GitHub **personal access token** with the `repo` scope. Create one at
<https://github.com/settings/tokens>.

```bash
dotnet run -- --login
```

You are prompted once, the token is verified against the GitHub API, and it is then
written to the platform's credential store:

| Platform | Stored in |
| --- | --- |
| Windows | Credential Manager (via the Windows keychain API) |
| macOS | Keychain |
| Linux | Secret Service / GNOME Keyring, KWallet |

Remove it again with:

```bash
dotnet run -- --logout
```

### Where the token is looked up

In order:

1. Environment variables `SAISHIN_TOKEN`
   Useful for CI, where no keyring is available.
2. The OS secure store, as written by `--login`.
3. Otherwise, if any selected source is `private = true`, an interactive prompt.

The token is never written to the INI file, never logged, and never included in a
`.part` file. If you prefer not to use the keyring at all, export `GITHUB_TOKEN`
from your scheduler and `--login` becomes unnecessary.

---

## Scheduled runs

Scheduled runs must never block on a prompt, so pass `--non-interactive`. The tool
will then either reuse the stored token or fail with exit code `3` and a message
telling you to run `--login` once.

```bash
saishin --config /etc/saishin/config.ini --non-interactive
```

### Linux (cron)

```cron
17 */6 * * * /usr/local/bin/saishin --config /etc/saishin/config.ini --non-interactive >> /var/log/saishin.log 2>&1
```

### Linux (systemd timer)

```ini
# /etc/systemd/system/saishin.service
[Unit]
Description=saishin

[Service]
Type=oneshot
ExecStart=/usr/local/bin/saishin --config /etc/saishin/config.ini --non-interactive
```

```ini
# /etc/systemd/system/saishin.timer
[Unit]
Description=Run saishin every six hours

[Timer]
OnCalendar=*-*-* 00/6:17:00
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl enable --now saishin.timer
```

Headless Linux servers usually have no keyring daemon, and the tool then reports
that no secure store is available and falls back to environment variables. Use
`SAISHIN_TOKEN` in a `EnvironmentFile` on those machines instead of `--login`.

### macOS (launchd)

```xml
<!-- ~/Library/LaunchAgents/com.saishin.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.saishin</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/saishin</string>
    <string>--config</string>
    <string>/etc/saishin/config.ini</string>
    <string>--non-interactive</string>
  </array>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>6</integer></dict>
  <key>StandardOutPath</key>
  <string>/tmp/saishin.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/saishin.err</string>
</dict>
</plist>
```

### Windows (Task Scheduler)

```powershell
Register-ScheduledTask -TaskName saishin -Action (
  New-ScheduledTaskAction -Execute 'C:\tools\saishin.exe' `
    -Argument '--config C:\saishin\config.ini --non-interactive'
) -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddHours(6) `
    -RepetitionInterval (New-TimeSpan -Hours 6)) -RunLevel Limited
```

Run the interactive `--login` from a normal terminal first, as your user account.
Windows Credential Manager is per-user, so the scheduled task must run as the same
user.

---

## Configuration reference

Every section other than `[general]` is one source. Relative paths resolve against
the directory containing the INI file.

### `[general]`

| Key | Default | Meaning |
| --- | --- | --- |
| `out_dir` | `downloads` | Default download directory for sources without their own `out` |
| `github_api_url` | `https://api.github.com` | API root; override for GitHub Enterprise |
| `keyring_service` | `saishin` | Label used for the token in the secure store |
| `notify` | `false` | One desktop notification per run, after all sources, listing downloaded files. Uses `powershell` (Windows), `osascript` (macOS), `notify-send` (Linux) |
| `sound` | empty | Play this sound file after a run that downloaded something |
| `sound_player` | empty | Command used to play the sound; the file is appended as the last argument. Default: `afplay` (macOS), `ffplay` (Windows, Linux) |

### Common keys (every source)

| Key | Default | Meaning |
| --- | --- | --- |
| `type` | *(required)* | `github` or `url` |
| `out` | `out_dir` | Download directory for this source |
| `filters` | *(all)* | Comma-separated globs matched against the file name; a glob prefixed with `!` excludes matches |
| `changelog` | `false` | Write the release description to `<artifact>.txt` next to each downloaded file (github sources) |
| `version_suffix` | `false` | Rename downloads to `<stem>-<tag><ext>` so same-name artifacts keep one file per version (release tag, or run number for actions) |

### `type = github`

| Key | Default | Meaning |
| --- | --- | --- |
| `owner` | *(required)* | Repository owner or organisation |
| `repo` | *(required)* | Repository name |
| `tag` | `latest` | `latest`, `prerelease`, `all`, or an exact tag name |
| `private` | `false` | Requires a stored GitHub token |

`tag = latest` picks the most recent non-draft, non-prerelease release, matching
GitHub's own definition. `tag = prerelease` picks the newest prerelease, and an
exact tag name matches only that release. `tag = all` walks every non-draft
release, paginating in batches of 100 up to a cap of 1000 releases.

Downloaded GitHub files have their modification time set to the release publish
date (or the workflow run creation date for action artifacts), so `keep`
retention orders by that date rather than by download time.

### `type = url`

| Key | Default | Meaning |
| --- | --- | --- |
| `urls` | *(required)* | Comma-separated list of absolute URLs |

The file name is taken from the last segment of the URL path. These downloads are
compared with the server's reported content length, so a changed file is fetched
without needing `--force`.

### Lists

`filters`, `urls` and `extract` take comma-separated values on a single line:

```ini
filters = *.vpt, *.vpin, Tables/*
```

The INI parser collapses repeated keys to the last value, which is why lists are
written as one comma-separated line rather than repeated.

---

## Archive handling

`.zip` and `.vpt` files are treated as archives. Extraction happens only when a
source sets `extract`, so a source never unpacks anything by accident.

| Key | Default | Meaning |
| --- | --- | --- |
| `extract` | *(none)* | Comma-separated globs of entries to extract. Extraction is skipped entirely when absent |
| `extract_to` | `<out>/<section>` | Destination directory for the unpacked files |
| `strip_prefix` | `0` | Number of leading path segments to drop. Use `1` for archives that wrap everything in one folder |
| `overwrite` | `false` | Replace files that already exist. Otherwise existing files are left alone |
| `keep_zip` | `true` | Set to `false` to delete the archive once extraction finished |

Extraction is always safe against path traversal: entries that would escape the
destination directory, or that carry a `..` segment, are refused and reported as
warnings.

Example — drop the wrapper folder and keep only the tables:

```ini
[territory_tables]
type = github
owner = vpinball
repo = vpin-mame-tables
filters = *.vpt
out = ./downloads/tables
extract = *.vpt
extract_to = ./tables
strip_prefix = 1
overwrite = true
keep_zip = false
```

`--force` makes `--overwrite` unnecessary; it overrides both the download and the
extraction step.

---

## Glob patterns

Filters and `extract` accept these wildcards:

| Pattern | Matches |
| --- | --- |
| `*` | Any run of characters except `/` |
| `?` | Exactly one character except `/` |
| `**` | Any run of characters including `/` |

A pattern with no `/` is matched against the whole path and against the file name
alone, so `*.vpt` catches tables nested at any depth. Patterns containing `/` are
always matched against the full path.

Matching is case-insensitive on Windows and macOS and case-sensitive on Linux,
following normal filesystem expectations.

---

## Command line options

```
saishin [config.ini] [options]
```

| Option | Effect |
| --- | --- |
| `-c`, `--config <path>` | Configuration file. Defaults to `./config.ini`, then next to the executable |
| `-s`, `--source <name>` | Run only this source. Repeatable |
| `--dry-run` | Report what would be downloaded and extracted, download nothing |
| `-f`, `--force` | Re-download and overwrite even when a file is already current |
| `--non-interactive` | Never prompt. Fail with exit code `3` if credentials are missing |
| `--login` | Prompt for a token, verify it, store it, then exit |
| `--logout` | Remove the stored token, then exit |
| `-q`, `--quiet` | Suppress progress and informational output |
| `-h`, `--help` | Show usage |

The configuration path may also be given positionally, as in
`saishin config.ini`.

---

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Configuration error — bad or missing INI file |
| `2` | Download or network error |
| `3` | Authentication error — missing, invalid or insufficient token |
| `130` | Interrupted (Ctrl+C) |

Use these in scheduler error handling to tell a broken config apart from a network
outage or an expired credential.

Downloads are written to a `.part` file and moved into place only after the
transfer completes, so an interrupted run never leaves a truncated file that a
later run would mistake for current.

---

## Project layout

```
saishin/
├── saishin.csproj      project definition and package references
├── config.ini.example      annotated example configuration
├── README.md
└── src/
    ├── Program.cs          argument parsing, orchestration, exit codes
    ├── Config.cs           INI model and validation
    ├── Glob.cs             glob to regular expression
    ├── Secrets.cs          token prompt, storage and lookup order
    ├── GitHub.cs           release discovery and asset selection
    ├── Downloader.cs       HTTP transfer, skip detection, progress
    ├── ZipHandler.cs       archive extraction rules
    └── Log.cs              output helpers
```

---

## Third-party components

| Package | Licence | Why |
| --- | --- | --- |
| [`ini-parser-netstandard`](https://www.nuget.org/packages/ini-parser-netstandard) (2.5.3) | MIT | INI parsing. Pure managed code, no platform calls |
| [`keyring-dotnet`](https://www.nuget.org/packages/keyring-dotnet) (1.2.0) | MIT | Cross-platform access to Credential Manager, Keychain and GNOME Keyring |

ZIP handling uses `System.IO.Compression`, which ships with .NET, so no third-party
archive library is required.

Both packages are pulled from NuGet on first restore, so the initial build needs
network access. Subsequent builds use the local NuGet cache.
