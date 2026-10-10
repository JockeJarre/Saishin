using System.Text;
using System.Text.RegularExpressions;

namespace Saishin;

static class Glob
{
    private static readonly RegexOptions Options =
        OperatingSystem.IsWindows() || OperatingSystem.IsMacOS()
            ? RegexOptions.IgnoreCase | RegexOptions.CultureInvariant
            : RegexOptions.CultureInvariant;

    private static readonly Dictionary<string, Regex> Cache = new(StringComparer.Ordinal);
    private static readonly object CacheLock = new();

    public static Regex Compile(string pattern)
    {
        lock (CacheLock)
        {
            if (Cache.TryGetValue(pattern, out var cached))
            {
                return cached;
            }

            var regex = new Regex(Translate(pattern), Options);
            Cache[pattern] = regex;
            return regex;
        }
    }

    private static string Translate(string pattern)
    {
        var sb = new StringBuilder("^");
        for (var i = 0; i < pattern.Length; i++)
        {
            var c = pattern[i];
            switch (c)
            {
                case '*':
                    if (i + 1 < pattern.Length && pattern[i + 1] == '*')
                    {
                        sb.Append(".*");
                        i++;
                    }
                    else
                    {
                        sb.Append("[^/]*");
                    }

                    break;
                case '?':
                    sb.Append("[^/]");
                    break;
                default:
                    sb.Append(Regex.Escape(c.ToString()));
                    break;
            }
        }

        sb.Append('$');
        return sb.ToString();
    }

    public static bool MatchesAny(string value, IEnumerable<string> patterns)
    {
        var matched = false;
        var hasPositive = false;

        foreach (var pattern in patterns)
        {
            if (pattern.StartsWith('!'))
            {
                if (Matches(value, pattern[1..]))
                {
                    return false;
                }
            }
            else
            {
                hasPositive = true;
                if (Matches(value, pattern))
                {
                    matched = true;
                }
            }
        }

        return hasPositive ? matched : true;
    }

    public static bool Matches(string value, string pattern)
    {
        if (Compile(pattern).IsMatch(value))
        {
            return true;
        }

        if (pattern.Contains('/') || pattern.Contains('\\'))
        {
            return false;
        }

        var separator = value.LastIndexOf('/');
        return separator >= 0 && Compile(pattern).IsMatch(value[(separator + 1)..]);
    }
}