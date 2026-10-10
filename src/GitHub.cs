using System.Text.Json;

namespace Saishin;

sealed record GhAsset(string Name, long Size, string BrowserDownloadUrl, string ApiUrl);

sealed record GhRelease(string Tag, bool Prerelease, bool Draft, string Body, DateTimeOffset? PublishedAt, IReadOnlyList<GhAsset> Assets);

sealed record GhRun(long Id, string Name, string Status, string Conclusion, long RunNumber, DateTimeOffset? CreatedAt, string ArtifactsUrl);

sealed record GhArtifact(long Id, string Name, long Size, string ArchiveDownloadUrl, bool Expired);

static class GitHub
{
    private const int MaxPages = 10;
    public static async Task<IReadOnlyList<GhRelease>> GetReleasesAsync(
        Downloader http,
        AppConfig config,
        SourceConfig source,
        string? token,
        int perPage,
        CancellationToken ct)
    {
        var wantsAll = source.Tag.Trim().Equals("all", StringComparison.OrdinalIgnoreCase);
        var releases = new List<GhRelease>();

        for (var page = 1; ; page++)
        {
            var batch = await GetReleasePageAsync(http, config, source, token, perPage, page, ct);
            releases.AddRange(batch);

            if (!wantsAll || batch.Count < perPage || page >= MaxPages)
            {
                break;
            }
        }

        return releases;
    }

    private static async Task<List<GhRelease>> GetReleasePageAsync(
        Downloader http,
        AppConfig config,
        SourceConfig source,
        string? token,
        int perPage,
        int page,
        CancellationToken ct)
    {
        var url =
            $"{config.GitHubApiUrl}/repos/{source.Owner}/{source.Repo}/releases?per_page={perPage}&page={page}";

        string body;
        try
        {
            body = await http.GetStringAsync(url, token, ct);
        }
        catch (DownloadException ex) when (ex.Status is 401 or 403)
        {
            throw new AuthException(
                $"GitHub rejected the request for {source.Owner}/{source.Repo} (HTTP {ex.Status}). " +
                "The token is missing, invalid, or lacks the 'repo' scope. Run 'saishin --login' to replace it.");
        }

        using var document = JsonDocument.Parse(body);
        if (document.RootElement.ValueKind != JsonValueKind.Array)
        {
            throw new DownloadException(0, $"Unexpected releases payload from {url}.");
        }

        var releases = new List<GhRelease>();
        foreach (var element in document.RootElement.EnumerateArray())
        {
            var assets = new List<GhAsset>();
            if (element.TryGetProperty("assets", out var assetArray) &&
                assetArray.ValueKind == JsonValueKind.Array)
            {
                foreach (var asset in assetArray.EnumerateArray())
                {
                    assets.Add(new GhAsset(
                        GetString(asset, "name") ?? string.Empty,
                        GetInt64(asset, "size"),
                        GetString(asset, "browser_download_url") ?? string.Empty,
                        GetString(asset, "url") ?? string.Empty));
                }
            }

            releases.Add(new GhRelease(
                GetString(element, "tag_name") ?? string.Empty,
                GetBool(element, "prerelease"),
                GetBool(element, "draft"),
                GetString(element, "body") ?? string.Empty,
                GetDateTime(element, "published_at"),
                assets));
        }

        return releases;
    }

    public static async Task<IReadOnlyList<GhRun>> GetRunsAsync(
        Downloader http,
        AppConfig config,
        SourceConfig source,
        string? token,
        int perPage,
        int maxRuns,
        Func<GhRun, bool> filter,
        CancellationToken ct)
    {
        var runs = new List<GhRun>();

        for (var page = 1; page <= MaxPages; page++)
        {
            var url = $"{config.GitHubApiUrl}/repos/{source.Owner}/{source.Repo}/actions/runs?per_page={perPage}&page={page}";
            var batch = await GetRunPageAsync(http, config, url, token, perPage, ct);
            runs.AddRange(batch.Where(filter));

            if (batch.Count < perPage || runs.Count >= maxRuns)
            {
                break;
            }
        }

        return runs.Take(maxRuns).ToList();
    }

    private static async Task<List<GhRun>> GetRunPageAsync(
        Downloader http,
        AppConfig config,
        string url,
        string? token,
        int perPage,
        CancellationToken ct)
    {
        string body;
        try
        {
            body = await http.GetStringAsync(url, token, ct);
        }
        catch (DownloadException ex) when (ex.Status is 401 or 403)
        {
            throw new AuthException(
                $"GitHub rejected the request for {config.GitHubApiUrl}/repos/.../actions/runs (HTTP {ex.Status}). " +
                "The token is missing, invalid, or lacks the 'repo' scope. Run 'saishin --login' to replace it.");
        }

        using var document = JsonDocument.Parse(body);
        if (!document.RootElement.TryGetProperty("workflow_runs", out var array) ||
            array.ValueKind != JsonValueKind.Array)
        {
            throw new DownloadException(0, $"Unexpected runs payload from {url}.");
        }

        var runs = new List<GhRun>();
        foreach (var element in array.EnumerateArray())
        {
            runs.Add(new GhRun(
                GetInt64(element, "id"),
                GetString(element, "name") ?? string.Empty,
                GetString(element, "status") ?? string.Empty,
                GetString(element, "conclusion") ?? string.Empty,
                GetInt64(element, "run_number"),
                GetDateTime(element, "created_at"),
                GetString(element, "artifacts_url") ?? string.Empty));
        }

        return runs;
    }

    public static async Task<IReadOnlyList<GhArtifact>> GetArtifactsAsync(
        Downloader http,
        AppConfig config,
        GhRun run,
        string? token,
        int perPage,
        CancellationToken ct)
    {
        var artifacts = new List<GhArtifact>();

        for (var page = 1; ; page++)
        {
            var url = $"{run.ArtifactsUrl}?per_page={perPage}&page={page}";
            var body = await http.GetStringAsync(url, token, ct);

            using var document = JsonDocument.Parse(body);
            if (!document.RootElement.TryGetProperty("artifacts", out var array) ||
                array.ValueKind != JsonValueKind.Array)
            {
                throw new DownloadException(0, $"Unexpected artifacts payload from {url}.");
            }

            var batch = new List<GhArtifact>();
            foreach (var element in array.EnumerateArray())
            {
                batch.Add(new GhArtifact(
                    GetInt64(element, "id"),
                    GetString(element, "name") ?? string.Empty,
                    GetInt64(element, "size_in_bytes"),
                    GetString(element, "archive_download_url") ?? string.Empty,
                    GetBool(element, "expired")));
            }

            artifacts.AddRange(batch);

            if (batch.Count < perPage)
            {
                break;
            }
        }

        return artifacts;
    }

    public static IReadOnlyList<GhRelease> Select(IReadOnlyList<GhRelease> releases, string tag)
    {
        var published = releases.Where(r => !r.Draft).ToList();
        if (published.Count == 0)
        {
            return Array.Empty<GhRelease>();
        }

        var selector = tag.Trim();

        if (selector.Length == 0 || selector.Equals("latest", StringComparison.OrdinalIgnoreCase))
        {
            var stable = published.FirstOrDefault(r => !r.Prerelease);
            return stable is null ? published.Take(1).ToList() : new List<GhRelease> { stable };
        }

        if (selector.Equals("prerelease", StringComparison.OrdinalIgnoreCase) ||
            selector.Equals("pre", StringComparison.OrdinalIgnoreCase))
        {
            var pre = published.FirstOrDefault(r => r.Prerelease);
            return pre is null ? published.Take(1).ToList() : new List<GhRelease> { pre };
        }

        if (selector.Equals("all", StringComparison.OrdinalIgnoreCase))
        {
            return published;
        }

        return published.Where(r => r.Tag.Equals(selector, StringComparison.Ordinal)).ToList();
    }

    public static async Task<string> ValidateTokenAsync(
        Downloader http,
        AppConfig config,
        string token,
        CancellationToken ct)
    {
        var body = await http.GetStringAsync($"{config.GitHubApiUrl}/user", token, ct);
        using var document = JsonDocument.Parse(body);
        return GetString(document.RootElement, "login") ?? "unknown";
    }

    public static async Task CheckRepositoryAccessAsync(
        Downloader http,
        AppConfig config,
        SourceConfig source,
        string token,
        CancellationToken ct)
    {
        var url = $"{config.GitHubApiUrl}/repos/{source.Owner}/{source.Repo}";

        try
        {
            await http.GetStringAsync(url, token, ct);
        }
        catch (DownloadException ex) when (ex.Status is 401 or 403 or 404)
        {
            throw new AuthException(
                $"The token cannot read {source.Owner}/{source.Repo} (HTTP {ex.Status}). " +
                "Give the token the 'repo' scope and confirm your account has access to the repository.");
        }
    }

    private static string? GetString(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;

    private static long GetInt64(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.TryGetInt64(out var parsed) ? parsed : 0L;

    private static bool GetBool(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) &&
        value.ValueKind is JsonValueKind.True;

    private static DateTimeOffset? GetDateTime(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) &&
        value.ValueKind == JsonValueKind.String &&
        DateTimeOffset.TryParse(value.GetString(), out var parsed)
            ? parsed
            : null;
}
