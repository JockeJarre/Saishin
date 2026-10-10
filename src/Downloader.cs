using System.Net;
using System.Net.Http.Headers;

namespace Saishin;

sealed class Downloader : IDisposable
{
    private readonly HttpClient _client;

    public Downloader(string userAgent)
    {
        var handler = new HttpClientHandler
        {
            AllowAutoRedirect = true,
            AutomaticDecompression = DecompressionMethods.All
        };

        _client = new HttpClient(handler)
        {
            Timeout = TimeSpan.FromMinutes(30)
        };
        _client.DefaultRequestHeaders.UserAgent.ParseAdd(userAgent);
    }

    public static HttpRequestMessage CreateRequest(string url, string? token, bool jsonAccept)
    {
        var request = new HttpRequestMessage(HttpMethod.Get, url);
        request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue(
            jsonAccept ? "application/json" : "application/octet-stream"));

        if (!string.IsNullOrEmpty(token))
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        }

        return request;
    }

    public async Task<string> GetStringAsync(string url, string? token, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, url);
        request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue("application/vnd.github+json"));
        request.Headers.Add("X-GitHub-Api-Version", "2022-11-28");
        if (!string.IsNullOrEmpty(token))
        {
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        }

        using var response = await _client.SendAsync(request, ct);
        var body = await response.Content.ReadAsStringAsync(ct);

        if (!response.IsSuccessStatusCode)
        {
            throw new DownloadException(
                (int)response.StatusCode,
                $"GET {url} failed with {(int)response.StatusCode} {response.StatusCode}: {Summarize(body)}");
        }

        return body;
    }

    public async Task<long?> TryGetContentLengthAsync(string url, string? token, CancellationToken ct)
    {
        try
        {
            using var request = new HttpRequestMessage(HttpMethod.Head, url);
            if (!string.IsNullOrEmpty(token))
            {
                request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
            }

            using var response = await _client.SendAsync(request, ct);
            return response.IsSuccessStatusCode ? response.Content.Headers.ContentLength : null;
        }
        catch (HttpRequestException)
        {
            return null;
        }
        catch (TaskCanceledException)
        {
            return null;
        }
    }

    private const int ParallelChunks = 4;
    private const long ParallelMinBytes = 8_000_000;

    public async Task<bool> DownloadAsync(
        string url,
        string? token,
        string destination,
        long? expectedSize,
        bool force,
        bool jsonAccept,
        CancellationToken ct)
    {
        if (!force && File.Exists(destination))
        {
            var actual = SafeFileLength(destination);
            var target = expectedSize ?? await TryGetContentLengthAsync(url, token, ct);

            if (actual is not null && (target is null || actual == target))
            {
                Log.Detail($"skip (already up to date): {Path.GetFileName(destination)}");
                return false;
            }
        }

        var directory = Path.GetDirectoryName(destination);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        if (expectedSize is > ParallelMinBytes && await SupportsRangeAsync(url, token, jsonAccept, ct))
        {
            return await DownloadParallelAsync(url, token, jsonAccept, destination, expectedSize.Value, ct);
        }

        return await DownloadSingleAsync(url, token, jsonAccept, destination, expectedSize, ct);
    }

    private async Task<bool> SupportsRangeAsync(string url, string? token, bool jsonAccept, CancellationToken ct)
    {
        var request = CreateRequest(url, token, jsonAccept);
        request.Headers.Range = new RangeHeaderValue(0, 0);

        using var response = await _client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, ct);
        return response.StatusCode == HttpStatusCode.PartialContent;
    }

    private async Task<bool> DownloadParallelAsync(
        string url,
        string? token,
        bool jsonAccept,
        string destination,
        long total,
        CancellationToken ct)
    {
        var chunkSize = (total + ParallelChunks - 1) / ParallelChunks;
        var files = new string[ParallelChunks];
        var starts = new long[ParallelChunks];
        var ends = new long[ParallelChunks];
        var lengths = new long[ParallelChunks];
        long written = 0;

        for (var i = 0; i < ParallelChunks; i++)
        {
            starts[i] = i * chunkSize;
            ends[i] = Math.Min(total - 1, starts[i] + chunkSize - 1);
            lengths[i] = ends[i] - starts[i] + 1;
            files[i] = destination + ".part." + i;

            if (File.Exists(files[i]))
            {
                var have = SafeFileLength(files[i]) ?? 0;
                if (have > lengths[i])
                {
                    new FileStream(files[i], FileMode.Truncate).Dispose();
                    have = 0;
                }

                written += have;
            }
        }

        var name = Path.GetFileName(destination);
        RenderProgress(name, written, total);

        var tasks = new Task[ParallelChunks];
        for (var i = 0; i < ParallelChunks; i++)
        {
            var index = i;
            tasks[i] = DownloadChunkAsync(
                url,
                token,
                jsonAccept,
                files[index],
                starts[index] + Math.Min(SafeFileLength(files[index]) ?? 0, lengths[index]),
                ends[index],
                n =>
                {
                    Interlocked.Add(ref written, n);
                    RenderProgress(name, written, total);
                },
                ct);
        }

        await Task.WhenAll(tasks);

        for (var i = 0; i < ParallelChunks; i++)
        {
            if (SafeFileLength(files[i]) != lengths[i])
            {
                throw new DownloadException(0, $"chunk {i} of {name} is incomplete");
            }
        }

        await using (var outStream = new FileStream(destination, FileMode.Create, FileAccess.Write, FileShare.None, 81920, useAsync: true))
        {
            for (var i = 0; i < ParallelChunks; i++)
            {
                await using var chunk = new FileStream(files[i], FileMode.Open, FileAccess.Read, FileShare.Read, 81920, useAsync: true);
                await chunk.CopyToAsync(outStream, ct);
            }
        }

        foreach (var file in files)
        {
            File.Delete(file);
        }

        RenderProgress(name, total, total);
        Console.WriteLine();
        return true;
    }

    private async Task DownloadChunkAsync(
        string url,
        string? token,
        bool jsonAccept,
        string file,
        long start,
        long end,
        Action<long> report,
        CancellationToken ct)
    {
        if (start > end)
        {
            if (!File.Exists(file))
            {
                new FileStream(file, FileMode.Create).Dispose();
            }

            return;
        }

        var buffer = new byte[81920];
        var request = CreateRequest(url, token, jsonAccept);
        request.Headers.Range = new RangeHeaderValue(start, end);

        using var response = await _client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, ct);
        if (!response.IsSuccessStatusCode)
        {
            var body = await response.Content.ReadAsStringAsync(ct);
            throw new DownloadException(
                (int)response.StatusCode,
                $"GET {url} chunk {start}-{end} failed with {(int)response.StatusCode} {response.StatusCode}: {Summarize(body)}");
        }

        if (response.StatusCode != HttpStatusCode.PartialContent)
        {
            throw new DownloadException((int)response.StatusCode, $"server ignored the Range header for {file}");
        }

        await using (var source = await response.Content.ReadAsStreamAsync(ct))
        await using (var target = new FileStream(file, File.Exists(file) ? FileMode.Append : FileMode.Create, FileAccess.Write, FileShare.None, buffer.Length, useAsync: true))
        {
            int read;
            while ((read = await source.ReadAsync(buffer, ct)) > 0)
            {
                await target.WriteAsync(buffer.AsMemory(0, read), ct);
                report(read);
            }
        }
    }

    private async Task<bool> DownloadSingleAsync(
        string url,
        string? token,
        bool jsonAccept,
        string destination,
        long? expectedSize,
        CancellationToken ct)
    {
        var temporary = destination + ".part";
        var existing = File.Exists(temporary) ? SafeFileLength(temporary) ?? 0 : 0;

        if (expectedSize is > 0 && existing == expectedSize)
        {
            File.Move(temporary, destination, overwrite: true);
            Log.Detail($"resumed complete: {Path.GetFileName(destination)}");
            return true;
        }

        HttpResponseMessage? response = null;

        try
        {
            var request = CreateRequest(url, token, jsonAccept);
            if (existing > 0)
            {
                request.Headers.Range = new RangeHeaderValue(existing, null);
            }

            response = await _client.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, ct);
            if (!response.IsSuccessStatusCode)
            {
                var body = await response.Content.ReadAsStringAsync(ct);
                throw new DownloadException(
                    (int)response.StatusCode,
                    $"GET {url} failed with {(int)response.StatusCode} {response.StatusCode}: {Summarize(body)}");
            }

            var append = existing > 0 && response.StatusCode == HttpStatusCode.PartialContent;
            var buffer = new byte[81920];
            var written = append ? existing : 0;
            var total = expectedSize ?? (append ? existing + response.Content.Headers.ContentLength : response.Content.Headers.ContentLength);

            await using (var source = await response.Content.ReadAsStreamAsync(ct))
            await using (var target = new FileStream(
                             temporary,
                             append ? FileMode.Append : FileMode.Create,
                             FileAccess.Write,
                             FileShare.None,
                             buffer.Length,
                             useAsync: true))
            {
                int read;
                while ((read = await source.ReadAsync(buffer, ct)) > 0)
                {
                    await target.WriteAsync(buffer.AsMemory(0, read), ct);
                    written += read;
                    RenderProgress(Path.GetFileName(destination), written, total);
                }
            }

            RenderProgress(Path.GetFileName(destination), written, total);
            Console.WriteLine();

            File.Move(temporary, destination, overwrite: true);
            return true;
        }
        catch
        {
            TryDelete(temporary);
            throw;
        }
        finally
        {
            response?.Dispose();
        }
    }

    private static void RenderProgress(string name, long written, long? total)
    {
        if (Log.Quiet)
        {
            return;
        }

        string line;
        if (total is > 0)
        {
            var percent = (int)Math.Clamp(written * 100 / total.Value, 0, 100);
            if (percent == 100 && written < total)
            {
                return;
            }

            line = $"    {name}  {percent,3}%  {FormatBytes(written)} / {FormatBytes(total.Value)}";
        }
        else
        {
            line = $"    {name}  {FormatBytes(written)}";
        }

        Console.Write("\r" + line.PadRight(78));
    }

    public static string FormatBytes(long bytes)
    {
        string[] units = { "B", "KB", "MB", "GB", "TB" };
        double value = bytes;
        var unit = 0;
        while (value >= 1024 && unit < units.Length - 1)
        {
            value /= 1024;
            unit++;
        }

        return unit == 0
            ? $"{bytes} {units[unit]}"
            : $"{value:0.##} {units[unit]}";
    }

    private static long? SafeFileLength(string path)
    {
        try
        {
            return new FileInfo(path).Length;
        }
        catch (IOException)
        {
            return null;
        }
        catch (UnauthorizedAccessException)
        {
            return null;
        }
    }

    private static void TryDelete(string path)
    {
        try
        {
            if (File.Exists(path))
            {
                File.Delete(path);
            }
        }
        catch (IOException)
        {
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    private static string Summarize(string body)
    {
        var single = body.Replace('\n', ' ').Replace('\r', ' ').Trim();
        return single.Length <= 200 ? single : single[..200] + "...";
    }

    public void Dispose() => _client.Dispose();
}

sealed class DownloadException : Exception
{
    public int Status { get; }

    public DownloadException(int status, string message) : base(message)
    {
        Status = status;
    }
}
