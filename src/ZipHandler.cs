using System.IO.Compression;

namespace Saishin;

static class ZipHandler
{
    private static readonly string[] ArchiveExtensions = { ".zip", ".vpt" };

    private static readonly StringComparison PathComparison =
        OperatingSystem.IsWindows() || OperatingSystem.IsMacOS()
            ? StringComparison.OrdinalIgnoreCase
            : StringComparison.Ordinal;

    public static bool IsArchive(string fileName)
    {
        var extension = Path.GetExtension(fileName);
        return ArchiveExtensions.Any(e => e.Equals(extension, StringComparison.OrdinalIgnoreCase));
    }

    public static void Extract(string archivePath, ExtractOptions options, bool force)
    {
        var root = Path.GetFullPath(options.Destination);
        Directory.CreateDirectory(root);

        var written = 0;
        var skipped = 0;

        using var archive = ZipFile.OpenRead(archivePath);

        foreach (var entry in archive.Entries)
        {
            if (string.IsNullOrEmpty(entry.Name))
            {
                continue;
            }

            var relative = Normalize(entry.FullName);
            if (HasTraversal(relative))
            {
                Log.Warn($"refusing to extract an entry with a parent-directory segment: {entry.FullName}");
                continue;
            }

            relative = StripPrefix(relative, options.StripPrefix);
            if (relative is null)
            {
                continue;
            }

            if (!Glob.MatchesAny(relative, options.Patterns))
            {
                continue;
            }

            var target = Path.GetFullPath(Path.Combine(root, relative));
            var guard = root.EndsWith(Path.DirectorySeparatorChar) ? root : root + Path.DirectorySeparatorChar;
            if (!target.StartsWith(guard, PathComparison))
            {
                Log.Warn($"refusing to extract outside the destination: {entry.FullName}");
                continue;
            }

            if (File.Exists(target) && !force && !options.Overwrite)
            {
                skipped++;
                continue;
            }

            var directory = Path.GetDirectoryName(target);
            if (!string.IsNullOrEmpty(directory))
            {
                Directory.CreateDirectory(directory);
            }

            entry.ExtractToFile(target, overwrite: true);
            written++;
            Log.Detail($"extracted {relative}");
        }

        Log.Info($"    archive: {written} extracted, {skipped} skipped");
    }

    private static string Normalize(string path) => path.Replace('\\', '/').TrimStart('/');

    private static bool HasTraversal(string path) =>
        path.Split('/', StringSplitOptions.RemoveEmptyEntries)
            .Any(segment => segment == "..");

    private static string? StripPrefix(string path, int count)
    {
        if (count <= 0)
        {
            return path;
        }

        var segments = path.Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length <= count)
        {
            return null;
        }

        return string.Join('/', segments.Skip(count));
    }
}
