using System.Diagnostics;

namespace Saishin;

static class Notifier
{
    public static void Send(string title, string body)
    {
        try
        {
            if (OperatingSystem.IsWindows())
            {
                var script =
                    "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; " +
                    "$x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); " +
                    "$t = $x.GetElementsByTagName('text'); " +
                    "$t.Item(0).AppendChild($x.CreateTextNode(" + PsQuote(title) + ")) | Out-Null; " +
                    "$t.Item(1).AppendChild($x.CreateTextNode(" + PsQuote(body) + ")) | Out-Null; " +
                    "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('saishin').Show($x)";
                Run("powershell", "-NoProfile -NonInteractive -Command " + Quote(script));
            }
            else if (OperatingSystem.IsMacOS())
            {
                Run("osascript", "-e " + Quote($"display notification {Quote(body)} with title {Quote(title)}"));
            }
            else
            {
                Run("notify-send", Quote(title) + " " + Quote(body));
            }
        }
        catch (Exception ex)
        {
            Log.Warn($"Notification failed: {FirstLine(ex.Message)}");
        }
    }

    public static void PlaySound(string path, string player)
    {
        try
        {
            string fileName;
            string arguments;
            if (player.Length > 0)
            {
                var parts = player.Split(' ', 2);
                fileName = parts[0];
                arguments = (parts.Length > 1 ? parts[1] + " " : "") + Quote(path);
            }
            else if (OperatingSystem.IsMacOS())
            {
                fileName = "afplay";
                arguments = Quote(path);
            }
            else
            {
                fileName = "ffplay";
                arguments = "-nodisp -autoexit -loglevel quiet " + Quote(path);
            }
            Run(fileName, arguments, 15000);
        }
        catch (Exception ex)
        {
            Log.Warn($"Sound failed: {FirstLine(ex.Message)}");
        }
    }

    private static void Run(string fileName, string arguments, int timeoutMs = 5000)
    {
        var info = new ProcessStartInfo(fileName, arguments)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true
        };
        using var process = Process.Start(info);
        if (process is not null)
        {
            process.WaitForExit(timeoutMs);
        }
    }

    private static string Quote(string value) => "\"" + value.Replace("\"", "\\\"") + "\"";

    private static string PsQuote(string value) => "'" + value.Replace("'", "''") + "'";

    private static string FirstLine(string message)
    {
        var newline = message.IndexOfAny(new[] { '\n', '\r' });
        return (newline >= 0 ? message[..newline] : message).Trim();
    }
}
