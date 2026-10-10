namespace Saishin;

sealed record PendingFile(string Url, string FileName, long? Size, string ReleaseTag, string Changelog, DateTimeOffset? PublishedAt, bool JsonAccept);

sealed class Options
{
    public string? ConfigPath { get; private set; }
    public List<string> Sources { get; } = new();
    public bool DryRun { get; private set; }
    public bool Force { get; private set; }
    public bool NonInteractive { get; private set; }
    public bool Login { get; private set; }
    public bool Logout { get; private set; }
    public bool ShowHelp { get; private set; }
    public bool Quiet { get; private set; }

    public static Options Parse(string[] args)
    {
        var options = new Options();

        for (var i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "-h":
                case "--help":
                    options.ShowHelp = true;
                    break;
                case "--dry-run":
                    options.DryRun = true;
                    break;
                case "-f":
                case "--force":
                    options.Force = true;
                    break;
                case "--non-interactive":
                    options.NonInteractive = true;
                    break;
                case "--login":
                    options.Login = true;
                    break;
                case "--logout":
                    options.Logout = true;
                    break;
                case "-q":
                case "--quiet":
                    options.Quiet = true;
                    break;
                case "-c":
                case "--config":
                    options.ConfigPath = Next(args, ref i);
                    break;
                case "-s":
                case "--source":
                    options.Sources.Add(Next(args, ref i));
                    break;
                default:
                    if (args[i].StartsWith('-'))
                    {
                        throw new ArgumentException($"Unknown option '{args[i]}'.");
                    }

                    if (options.ConfigPath is null)
                    {
                        options.ConfigPath = args[i];
                    }
                    else
                    {
                        throw new ArgumentException($"Unexpected argument '{args[i]}'.");
                    }

                    break;
            }
        }

        return options;
    }

    private static string Next(string[] args, ref int index)
    {
        if (index + 1 >= args.Length)
        {
            throw new ArgumentException($"Option '{args[index]}' requires a value.");
        }

        return args[++index];
    }
}

static class Program
{
    private const string UserAgent = "saishin/1.0";

    private static async Task<int> Main(string[] args)
    {
        Options options;
        try
        {
            options = Options.Parse(args);
        }
        catch (ArgumentException ex)
        {
            Log.Error(ex.Message);
            Console.Error.WriteLine();
            PrintUsage();
            return 1;
        }

        if (options.ShowHelp)
        {
            PrintUsage();
            return 0;
        }

        Log.Quiet = options.Quiet;

        using var cancellation = new CancellationTokenSource();
        Console.CancelKeyPress += (_, eventArgs) =>
        {
            eventArgs.Cancel = true;
            cancellation.Cancel();
        };

        try
        {
            var configPath = options.ConfigPath ?? FindConfig();
            var config = Config.Load(configPath);

            if (options.Logout)
            {
                Secrets.Remove(config);
                return 0;
            }

            using var http = new Downloader(UserAgent);

            if (options.Login)
            {
                return await LoginAsync(config, http, cancellation.Token);
            }

            var selected = SelectSources(config, options);
            var token = await ResolveTokenAsync(config, selected, http, options, cancellation.Token);

            var updated = new List<string>();
            foreach (var source in selected)
            {
                var names = await RunSourceAsync(source, config, http, token, options, cancellation.Token);
                updated.AddRange(names.Select(name => $"{source.Name}: {name}"));
            }

            if (config.Notify && updated.Count > 0)
            {
                Notifier.Send("saishin", string.Join(", ", updated));
            }

            if (config.Sound.Length > 0 && updated.Count > 0)
            {
                Notifier.PlaySound(config.Sound, config.SoundPlayer);
            }

            return 0;
        }
        catch (ConfigException ex)
        {
            Log.Error(ex.Message);
            return 1;
        }
        catch (AuthException ex)
        {
            Log.Error(ex.Message);
            return 3;
        }
        catch (OperationCanceledException)
        {
            Log.Error("Cancelled.");
            return 130;
        }
        catch (DownloadException ex)
        {
            Log.Error(ex.Message);
            return 2;
        }
        catch (Exception ex)
        {
            Log.Error($"{ex.GetType().Name}: {ex.Message}");
            return 2;
        }
    }

    private static string FindConfig()
    {
        var candidates = new[]
        {
            Path.Combine(Directory.GetCurrentDirectory(), "config.ini"),
            Path.Combine(AppContext.BaseDirectory, "config.ini")
        };

        foreach (var candidate in candidates)
        {
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }

        throw new ConfigException("No config.ini found. Pass one with --config <path>.");
    }

    private static List<SourceConfig> SelectSources(AppConfig config, Options options)
    {
        if (options.Sources.Count == 0)
        {
            return config.Sources;
        }

        var known = config.Sources.Select(s => s.Name).ToList();
        var unknown = options.Sources
            .Where(requested => !known.Any(k => k.Equals(requested, StringComparison.OrdinalIgnoreCase)))
            .ToList();

        if (unknown.Count > 0)
        {
            throw new ConfigException(
                $"Unknown source(s): {string.Join(", ", unknown)}. Configured sources: {string.Join(", ", known)}");
        }

        var wanted = new HashSet<string>(options.Sources, StringComparer.OrdinalIgnoreCase);
        return config.Sources.Where(s => wanted.Contains(s.Name)).ToList();
    }

    private static async Task<string?> ResolveTokenAsync(
        AppConfig config,
        List<SourceConfig> selected,
        Downloader http,
        Options options,
        CancellationToken ct)
    {
        var githubSources = selected.Where(s => s.Kind == SourceKind.GitHub).ToList();
        var actionsSources = selected.Where(s => s.Kind == SourceKind.Actions).ToList();
        if (githubSources.Count == 0 && actionsSources.Count == 0)
        {
            return null;
        }

        var fromEnvironment = Secrets.FromEnvironment();
        if (fromEnvironment is not null)
        {
            Log.Detail("GitHub token taken from the environment.");
            return fromEnvironment;
        }

        var stored = Secrets.FromKeyring(config);
        if (stored is not null)
        {
            Log.Detail("GitHub token taken from the secure store.");
            return stored;
        }

        var requiresAccount = githubSources.Any(s => s.Private) || actionsSources.Count > 0;
        if (!requiresAccount)
        {
            Log.Detail("No token stored; using anonymous access for public releases.");
            return null;
        }

        if (options.NonInteractive)
        {
            throw new AuthException(
                "Some sources need a GitHub account but no token is stored. " +
                "Run 'saishin --login' once interactively, then re-run this scheduled job.");
        }

        Log.Info("This source needs a GitHub account (private repository or authenticated action builds).");
        var token = Secrets.PromptForToken();

        foreach (var source in githubSources.Where(s => s.Private))
        {
            await GitHub.CheckRepositoryAccessAsync(http, config, source, token, ct);
        }

        foreach (var source in actionsSources)
        {
            await GitHub.CheckRepositoryAccessAsync(http, config, source, token, ct);
        }

        Secrets.Store(config, token);
        Log.Info("Token stored in the secure store. Scheduled runs will reuse it without prompting.");
        return token;
    }

    private static async Task<int> LoginAsync(AppConfig config, Downloader http, CancellationToken ct)
    {
        var token = Secrets.PromptForToken();

        try
        {
            var login = await GitHub.ValidateTokenAsync(http, config, token, ct);
            Secrets.Store(config, token);
            Log.Info($"Token verified for '{login}' and stored in the secure store.");
            return 0;
        }
        catch (DownloadException ex) when (ex.Status is 401 or 403)
        {
            Log.Error("GitHub rejected that token. Nothing was stored.");
            return 3;
        }
    }

    private static async Task<List<string>> RunSourceAsync(
        SourceConfig source,
        AppConfig config,
        Downloader http,
        string? token,
        Options options,
        CancellationToken ct)
    {
        Log.Info($"[{source.Name}]");

        List<PendingFile> files;
        if (source.Kind == SourceKind.GitHub)
        {
            Log.Detail($"github {source.Owner}/{source.Repo}, tag '{source.Tag}'");
            files = await ResolveGitHubAsync(source, config, http, token, ct);
        }
        else if (source.Kind == SourceKind.Actions)
        {
            Log.Detail($"actions {source.Owner}/{source.Repo}, workflow '{source.Workflow}'");
            files = await ResolveActionsAsync(source, config, http, token, ct);
        }
        else
        {
            files = ResolveUrls(source);
        }

        if (files.Count == 0)
        {
            Log.Info("    nothing matched");
            return new List<string>();
        }

        var downloadedNames = new List<string>();

        foreach (var file in files)
        {
            ct.ThrowIfCancellationRequested();

            var name = SanitizeFileName(file.FileName);
            if (name.Length == 0)
            {
                Log.Warn($"[{source.Name}] unusable file name, skipped.");
                continue;
            }

            if (source.VersionSuffix && file.ReleaseTag.Trim().Length > 0)
            {
                var version = SanitizeFileName(file.ReleaseTag.Trim());
                if (version.Length > 0)
                {
                    name = Path.GetFileNameWithoutExtension(name) + "-" + version + Path.GetExtension(name);
                }
            }

            var destination = Path.Combine(source.OutDir, name);

            if (options.DryRun)
            {
                var size = file.Size is { } bytes
                    ? Downloader.FormatBytes(bytes)
                    : "size unknown";
                Log.Detail($"would download {name} ({size})");
                continue;
            }

            bool downloaded;
            try
            {
                downloaded = await http.DownloadAsync(file.Url, token, destination, file.Size, options.Force, file.JsonAccept, ct);
            }
            catch (DownloadException ex) when (ex.Status is 401 or 403 or 404)
            {
                throw new AuthException(
                    $"[{source.Name}] the server returned {ex.Status} for {name}. " +
                    "If the repository is private or the build is account-gated, run 'saishin --login' " +
                    "and make sure the token has the 'repo' scope.");
            }

            if (downloaded)
            {
                downloadedNames.Add(name);
            }

            if (downloaded && source.Changelog && file.Changelog.Length > 0)
            {
                var changelogPath = Path.Combine(source.OutDir, Path.GetFileNameWithoutExtension(name) + ".txt");
                File.WriteAllText(changelogPath, file.Changelog);
                Log.Detail($"changelog written to {Path.GetFileName(changelogPath)}");
            }

            if (downloaded && file.PublishedAt is { } published)
            {
                File.SetLastWriteTime(destination, published.LocalDateTime);
                Log.Detail($"timestamp set to publish date {published.LocalDateTime:yyyy-MM-dd HH:mm}");
            }

            if (downloaded && source.Extract is not null && ZipHandler.IsArchive(name))
            {
                Log.Detail($"extracting into {source.Extract.Destination}");
                ZipHandler.Extract(destination, source.Extract, options.Force);

                if (!source.Extract.KeepZip)
                {
                    File.Delete(destination);
                    Log.Detail("archive removed (keep_zip = false)");
                }
            }
        }

        if (!options.DryRun && source.Keep > 0)
        {
            PruneOldFiles(source.OutDir, source.Keep);
        }

        return downloadedNames;
    }

    private static readonly TimeSpan GenerationGap = TimeSpan.FromHours(6);

    private static void PruneOldFiles(string outDir, int keep)
    {
        if (!Directory.Exists(outDir))
        {
            return;
        }

        var files = Directory.GetFiles(outDir)
            .Select(f => new FileInfo(f))
            .OrderByDescending(f => f.LastWriteTime)
            .ToList();

        var generations = new List<List<FileInfo>>();

        foreach (var file in files)
        {
            if (generations.Count > 0 &&
                generations[^1][0].LastWriteTime - file.LastWriteTime <= GenerationGap)
            {
                generations[^1].Add(file);
            }
            else
            {
                generations.Add(new List<FileInfo> { file });
            }
        }

        foreach (var generation in generations.Skip(keep))
        {
            foreach (var file in generation)
            {
                File.Delete(file.FullName);
                Log.Detail($"pruned old file: {file.Name}");
            }
        }
    }

    private static async Task<List<PendingFile>> ResolveActionsAsync(
        SourceConfig source,
        AppConfig config,
        Downloader http,
        string? token,
        CancellationToken ct)
    {
        var runs = await GitHub.GetRunsAsync(http, config, source, token, 100, source.MaxRuns, r =>
            r.Status.Equals("completed", StringComparison.OrdinalIgnoreCase) &&
            r.Conclusion.Equals("success", StringComparison.OrdinalIgnoreCase) &&
            (source.Workflow.Length == 0 || Glob.Matches(r.Name, source.Workflow)), ct);

        var files = new List<PendingFile>();

        foreach (var run in runs)
        {
            var artifacts = await GitHub.GetArtifactsAsync(http, config, run, token, 100, ct);

            foreach (var artifact in artifacts)
            {
                if (artifact.Expired)
                {
                    continue;
                }

                if (source.Filters.Count > 0 && !Glob.MatchesAny(artifact.Name, source.Filters))
                {
                    continue;
                }

                var name = artifact.Name.EndsWith(".zip", StringComparison.OrdinalIgnoreCase)
                    ? artifact.Name
                    : artifact.Name + ".zip";

                if (artifact.ArchiveDownloadUrl.Length == 0)
                {
                    continue;
                }

                files.Add(new PendingFile(artifact.ArchiveDownloadUrl, name, artifact.Size, run.RunNumber.ToString(), string.Empty, run.CreatedAt, true));
            }
        }

        Log.Detail($"matched {files.Count} artifact(s) across {runs.Count} run(s)");
        return files;
    }

    private static async Task<List<PendingFile>> ResolveGitHubAsync(
        SourceConfig source,
        AppConfig config,
        Downloader http,
        string? token,
        CancellationToken ct)
    {
        var releases = await GitHub.GetReleasesAsync(http, config, source, token, 100, ct);
        var selected = GitHub.Select(releases, source.Tag);

        if (selected.Count == 0)
        {
            Log.Warn($"[{source.Name}] no release matched tag '{source.Tag}'.");
            return new List<PendingFile>();
        }

        var files = new List<PendingFile>();

        foreach (var release in selected)
        {
            foreach (var asset in release.Assets)
            {
                if (source.Filters.Count > 0 && !Glob.MatchesAny(asset.Name, source.Filters))
                {
                    continue;
                }

                var url = token is not null ? asset.ApiUrl : asset.BrowserDownloadUrl;
                if (url.Length == 0)
                {
                    continue;
                }

                files.Add(new PendingFile(url, asset.Name, asset.Size, release.Tag, release.Body, release.PublishedAt, false));
            }
        }

        Log.Detail($"matched {files.Count} asset(s) across {selected.Count} release(s)");
        return files;
    }

    private static List<PendingFile> ResolveUrls(SourceConfig source)
    {
        var files = new List<PendingFile>();

        foreach (var url in source.Urls)
        {
            var name = FileNameFromUrl(url);

            if (source.Filters.Count > 0 && !Glob.MatchesAny(name, source.Filters))
            {
                continue;
            }

            files.Add(new PendingFile(url, name, null, string.Empty, string.Empty, null, false));
        }

        Log.Detail($"matched {files.Count} url(s)");
        return files;
    }

    private static string FileNameFromUrl(string url)
    {
        if (!Uri.TryCreate(url, UriKind.Absolute, out var uri))
        {
            return "download.bin";
        }

        var candidate = Uri.UnescapeDataString(Path.GetFileName(uri.AbsolutePath));
        return string.IsNullOrWhiteSpace(candidate) ? "download.bin" : candidate;
    }

    private static string SanitizeFileName(string fileName)
    {
        var name = Path.GetFileName(fileName);
        if (string.IsNullOrWhiteSpace(name))
        {
            return string.Empty;
        }

        var invalid = OperatingSystem.IsWindows() ? "\\/:*?\"<>|" : "/";
        foreach (var character in invalid)
        {
            name = name.Replace(character, '_');
        }

        return name.Trim();
    }

    private static void PrintUsage()
    {
        Console.WriteLine("""
            saishin - download official releases from GitHub and other sources

            USAGE
              saishin [config.ini] [options]

            OPTIONS
              -c, --config <path>    Configuration file (default: ./config.ini)
              -s, --source <name>    Run only this source (repeatable)
                  --dry-run          List what would be downloaded, download nothing
              -f, --force            Re-download and overwrite even when up to date
                  --non-interactive  Never prompt; fail if credentials are missing
                  --login            Store or replace the GitHub token, then exit
                  --logout           Remove the stored GitHub token, then exit
              -q, --quiet            Suppress progress and informational output
              -h, --help             Show this help

            EXIT CODES
              0  success
              1  configuration error
              2  download error
              3  authentication error
            130  interrupted
            """);
    }
}
