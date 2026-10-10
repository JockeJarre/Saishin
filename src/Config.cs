using IniParser;
using IniParser.Model;

namespace Saishin;

enum SourceKind
{
    GitHub,
    Url,
    Actions
}

sealed class ExtractOptions
{
    public List<string> Patterns { get; } = new();
    public string Destination { get; set; } = "";
    public int StripPrefix { get; set; }
    public bool Overwrite { get; set; }
    public bool KeepZip { get; set; } = true;
}

sealed class SourceConfig
{
    public required string Name { get; init; }
    public SourceKind Kind { get; init; }
    public string Owner { get; init; } = "";
    public string Repo { get; init; } = "";
    public string Tag { get; init; } = "latest";
    public bool Private { get; init; }
    public string Workflow { get; init; } = "";
    public int MaxRuns { get; init; } = 10;
    public int Keep { get; set; }
    public bool Changelog { get; set; }
    public bool VersionSuffix { get; set; }
    public List<string> Urls { get; } = new();
    public List<string> Filters { get; } = new();
    public string OutDir { get; init; } = "";
    public ExtractOptions? Extract { get; init; }
}

sealed class AppConfig
{
    public required string BaseDirectory { get; init; }
    public string DefaultOutDir { get; init; } = "downloads";
    public string GitHubApiUrl { get; init; } = "https://api.github.com";
    public string KeyringService { get; init; } = "saishin";
    public bool Notify { get; init; }
    public string Sound { get; init; } = "";
    public string SoundPlayer { get; init; } = "";
    public List<SourceConfig> Sources { get; } = new();
}

static class Config
{
    private const string GeneralSection = "general";

    public static AppConfig Load(string path)
    {
        if (!File.Exists(path))
        {
            throw new ConfigException($"Configuration file not found: {path}");
        }

        IniData ini;
        try
        {
            ini = new FileIniDataParser().ReadFile(path);
        }
        catch (Exception ex)
        {
            throw new ConfigException($"Could not parse {path}: {ex.Message}");
        }

        var sections = Flatten(ini);
        var fullPath = Path.GetFullPath(path);
        var baseDir = Path.GetDirectoryName(fullPath) ?? Directory.GetCurrentDirectory();

        var config = new AppConfig
        {
            BaseDirectory = baseDir,
            DefaultOutDir = GetString(sections, GeneralSection, "out_dir", "downloads"),
            GitHubApiUrl = GetString(sections, GeneralSection, "github_api_url", "https://api.github.com").TrimEnd('/'),
            KeyringService = GetString(sections, GeneralSection, "keyring_service", "saishin"),
            Notify = GetBool(sections, GeneralSection, "notify", false),
            Sound = ResolveSound(sections, GeneralSection, baseDir),
            SoundPlayer = GetString(sections, GeneralSection, "sound_player", "").Trim()
        };

        foreach (var (name, _) in sections)
        {
            if (name.Equals(GeneralSection, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            var source = ReadSource(sections, name, config.DefaultOutDir, baseDir);
            if (source is not null)
            {
                config.Sources.Add(source);
            }
        }

        if (config.Sources.Count == 0)
        {
            throw new ConfigException(
                $"No sources defined in {path}. Every section other than [{GeneralSection}] needs a 'type' of github or url.");
        }

        return config;
    }

    private static Dictionary<string, Dictionary<string, string>> Flatten(IniData ini)
    {
        var sections = new Dictionary<string, Dictionary<string, string>>(StringComparer.OrdinalIgnoreCase);

        foreach (var sectionData in ini.Sections)
        {
            var keys = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);

            foreach (var keyData in sectionData.Keys)
            {
                keys[keyData.KeyName] = keyData.Value;
            }

            sections[sectionData.SectionName] = keys;
        }

        return sections;
    }

    private static SourceConfig? ReadSource(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string defaultOut,
        string baseDir)
    {
        var type = GetString(sections, section, "type", "").ToLowerInvariant();

        if (type.Length == 0)
        {
            throw new ConfigException($"Section [{section}] is missing the required 'type' key.");
        }

        SourceConfig source;

        switch (type)
        {
            case "github":
            {
                var owner = GetString(sections, section, "owner", "");
                var repo = GetString(sections, section, "repo", "");

                if (owner.Length == 0 || repo.Length == 0)
                {
                    throw new ConfigException($"Section [{section}] of type github requires both 'owner' and 'repo'.");
                }

                var outDir = ResolveOutputDirectory(sections, section, defaultOut, baseDir);

                source = new SourceConfig
                {
                    Name = section,
                    Kind = SourceKind.GitHub,
                    Owner = owner,
                    Repo = repo,
                    Tag = GetString(sections, section, "tag", "latest"),
                    Private = GetBool(sections, section, "private", false),
                    OutDir = outDir,
                    Extract = ReadExtract(sections, section, baseDir, Path.Combine(outDir, section))
                };
                break;
            }

            case "url":
            {
                var outDir = ResolveOutputDirectory(sections, section, defaultOut, baseDir);

                source = new SourceConfig
                {
                    Name = section,
                    Kind = SourceKind.Url,
                    OutDir = outDir,
                    Extract = ReadExtract(sections, section, baseDir, Path.Combine(outDir, section))
                };
                break;
            }

            case "actions":
            {
                var owner = GetString(sections, section, "owner", "");
                var repo = GetString(sections, section, "repo", "");

                if (owner.Length == 0 || repo.Length == 0)
                {
                    throw new ConfigException($"Section [{section}] of type actions requires both 'owner' and 'repo'.");
                }

                var outDir = ResolveOutputDirectory(sections, section, defaultOut, baseDir);

                source = new SourceConfig
                {
                    Name = section,
                    Kind = SourceKind.Actions,
                    Owner = owner,
                    Repo = repo,
                    Workflow = GetString(sections, section, "workflow", ""),
                    MaxRuns = GetInt(sections, section, "runs", 10),
                    OutDir = outDir,
                    Extract = ReadExtract(sections, section, baseDir, Path.Combine(outDir, section))
                };
                break;
            }

            default:
                throw new ConfigException($"Section [{section}] has unknown type '{type}'. Use 'github' or 'url'.");
        }

        foreach (var value in GetList(sections, section, "filters"))
        {
            source.Filters.Add(value);
        }

        source.Keep = GetInt(sections, section, "keep", 0);
        source.Changelog = GetBool(sections, section, "changelog", false);
        source.VersionSuffix = GetBool(sections, section, "version_suffix", false);
        if (source.Keep < 0)
        {
            throw new ConfigException($"Section [{section}]: 'keep' cannot be negative.");
        }

        if (source.MaxRuns < 1)
        {
            throw new ConfigException($"Section [{section}]: 'runs' must be at least 1.");
        }

        if (source.Kind == SourceKind.Url)
        {
            var urls = GetList(sections, section, "urls");
            if (urls.Count == 0)
            {
                throw new ConfigException($"Section [{section}] of type url requires at least one entry in 'urls'.");
            }

            source.Urls.AddRange(urls);
        }

        return source;
    }

    private static ExtractOptions? ReadExtract(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string baseDir,
        string defaultDestination)
    {
        var patterns = GetList(sections, section, "extract");
        if (patterns.Count == 0)
        {
            return null;
        }

        var destination = GetString(sections, section, "extract_to", "");
        var resolved = destination.Length == 0
            ? defaultDestination
            : ResolvePath(destination, baseDir);

        var options = new ExtractOptions
        {
            Destination = resolved,
            StripPrefix = GetInt(sections, section, "strip_prefix", 0),
            Overwrite = GetBool(sections, section, "overwrite", false),
            KeepZip = GetBool(sections, section, "keep_zip", true)
        };
        options.Patterns.AddRange(patterns);

        if (options.StripPrefix < 0)
        {
            throw new ConfigException($"Section [{section}]: 'strip_prefix' cannot be negative.");
        }

        return options;
    }

    private static string ResolveOutputDirectory(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string defaultOut,
        string baseDir)
    {
        var configured = GetString(sections, section, "out", "");
        var relative = configured.Length == 0 ? defaultOut : configured;
        return ResolvePath(relative, baseDir);
    }

    private static string ResolveSound(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string baseDir)
    {
        var configured = GetString(sections, section, "sound", "").Trim();
        return configured.Length == 0 ? "" : ResolvePath(configured, baseDir);
    }

    public static string ResolvePath(string path, string baseDir)
    {
        if (Path.IsPathRooted(path))
        {
            return Path.GetFullPath(path);
        }

        return Path.GetFullPath(Path.Combine(baseDir, path));
    }

    private static string GetString(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string key,
        string fallback)
    {
        if (!sections.TryGetValue(section, out var keys) || !keys.TryGetValue(key, out var raw))
        {
            return fallback;
        }

        var trimmed = raw.Trim();
        return trimmed.Length == 0 ? fallback : trimmed;
    }

    private static bool GetBool(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string key,
        bool fallback)
    {
        var raw = GetString(sections, section, key, "");
        if (raw.Length == 0)
        {
            return fallback;
        }

        return raw.ToLowerInvariant() switch
        {
            "true" or "yes" or "on" or "1" => true,
            "false" or "no" or "off" or "0" => false,
            _ => throw new ConfigException($"Section [{section}]: '{key}' must be true or false, not '{raw}'.")
        };
    }

    private static int GetInt(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string key,
        int fallback)
    {
        var raw = GetString(sections, section, key, "");
        if (raw.Length == 0)
        {
            return fallback;
        }

        if (!int.TryParse(raw, out var parsed))
        {
            throw new ConfigException($"Section [{section}]: '{key}' must be a whole number, not '{raw}'.");
        }

        return parsed;
    }

    private static List<string> GetList(
        Dictionary<string, Dictionary<string, string>> sections,
        string section,
        string key)
    {
        var result = new List<string>();

        if (!sections.TryGetValue(section, out var keys) || !keys.TryGetValue(key, out var raw))
        {
            return result;
        }

        foreach (var part in raw.Split(','))
        {
            var trimmed = part.Trim();
            if (trimmed.Length > 0)
            {
                result.Add(trimmed);
            }
        }

        return result;
    }
}

sealed class ConfigException : Exception
{
    public ConfigException(string message) : base(message)
    {
    }
}