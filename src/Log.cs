namespace Saishin;

static class Log
{
    public static bool Quiet { get; set; }

    public static void Info(string message)
    {
        if (!Quiet)
        {
            Console.WriteLine(message);
        }
    }

    public static void Detail(string message)
    {
        if (!Quiet)
        {
            Console.WriteLine("    " + message);
        }
    }

    public static void Warn(string message)
    {
        Console.Error.WriteLine("warning: " + message);
    }

    public static void Error(string message)
    {
        Console.Error.WriteLine("error: " + message);
    }
}

sealed class AuthException : Exception
{
    public AuthException(string message) : base(message)
    {
    }
}
