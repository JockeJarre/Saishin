using System.Text;
using KeySharp;

namespace Saishin;

static class Secrets
{
    private const string TokenService = "github";
    private const string TokenUser = "token";

    private static bool warnedAboutKeyring;

    public static string? FromEnvironment()
    {
        foreach (var name in new[] { "SAISHIN_TOKEN" })
        {
            var value = Environment.GetEnvironmentVariable(name);
            if (!string.IsNullOrWhiteSpace(value))
            {
                return value.Trim();
            }
        }

        return null;
    }

    public static string? FromKeyring(AppConfig config)
    {
        try
        {
            return Keyring.GetPassword(config.KeyringService, TokenService, TokenUser);
        }
        catch (Exception ex) when (IsMissing(ex))
        {
            WarnNoKeyring();
            return null;
        }
        catch (KeyringException)
        {
            return null;
        }
        catch (Exception ex)
        {
            Log.Warn($"Could not read the keyring: {FirstLine(ex.Message)}");
            return null;
        }
    }

    public static void Store(AppConfig config, string token)
    {
        try
        {
            Keyring.SetPassword(config.KeyringService, TokenService, TokenUser, token);
        }
        catch (Exception ex) when (IsMissing(ex))
        {
            throw new AuthException(
                "No secure credential store is available on this machine, so the token cannot be saved. " +
                "Export GITHUB_TOKEN instead, for example from your scheduler's EnvironmentFile.");
        }
        catch (Exception ex)
        {
            throw new AuthException($"Could not write to the secure credential store: {FirstLine(ex.Message)}");
        }
    }

    public static void Remove(AppConfig config)
    {
        try
        {
            Keyring.DeletePassword(config.KeyringService, TokenService, TokenUser);
            Log.Info("Stored GitHub token removed from the keyring.");
        }
        catch (Exception ex) when (IsMissing(ex))
        {
            Log.Info("No secure credential store is available on this machine, so there is nothing to remove.");
        }
        catch (KeyringException)
        {
            Log.Info("No stored GitHub token found.");
        }
        catch (Exception ex)
        {
            Log.Warn($"Could not use the keyring: {FirstLine(ex.Message)}");
        }
    }

    public static string PromptForToken()
    {
        Log.Info("");
        Log.Info("A GitHub personal access token is required.");
        Log.Info("Create one at https://github.com/settings/tokens with 'repo' scope.");
        Log.Info("It is stored in this machine's secure credential store and never written to disk by saishin.");

        if (Console.IsInputRedirected)
        {
            var piped = Console.ReadLine();
            if (string.IsNullOrWhiteSpace(piped))
            {
                throw new AuthException("No token was provided on stdin.");
            }

            return piped.Trim();
        }

        Console.Write("Token: ");
        var buffer = new StringBuilder();
        while (true)
        {
            var key = Console.ReadKey(intercept: true);
            if (key.Key == ConsoleKey.Enter)
            {
                break;
            }

            if (key.Key == ConsoleKey.Backspace)
            {
                if (buffer.Length > 0)
                {
                    buffer.Length--;
                    Console.Write("\b \b");
                }

                continue;
            }

            if (char.IsControl(key.KeyChar))
            {
                continue;
            }

            buffer.Append(key.KeyChar);
            Console.Write('*');
        }

        Console.WriteLine();
        if (buffer.Length == 0)
        {
            throw new AuthException("No token entered.");
        }

        return buffer.ToString();
    }

    private static bool IsMissing(Exception ex) =>
        ex is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException or TypeLoadException;

    private static void WarnNoKeyring()
    {
        if (warnedAboutKeyring)
        {
            return;
        }

        warnedAboutKeyring = true;
        Log.Warn(
            "No secure credential store is available on this machine, so no token can be read or stored. " +
            "Set SAISHIN_TOKEN for private sources.");
    }

    private static string FirstLine(string message)
    {
        var newline = message.IndexOfAny(new[] { '\n', '\r' });
        return (newline >= 0 ? message[..newline] : message).Trim();
    }
}