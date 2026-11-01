# Checks that a Windows binary carries the app icon as a PE resource, and fails
# otherwise. Used by the CI jobs that ship a binary: build.rs embeds
# saishin-png-icon-set/saishin.ico best-effort (a missing .ico is only a build
# warning), so this is the step that keeps the promise that the shipped
# executable shows the icon in Explorer and on the taskbar.
#
#   ./scripts/check-icon.ps1 dist/saishin.exe
#
# Why not System.Drawing.Icon.ExtractAssociatedIcon: for an executable without
# an icon resource it returns the system's generic application icon, so it
# reports success on exactly the binary this check exists to catch.
# LoadLibraryEx(LOAD_LIBRARY_AS_DATAFILE) + EnumResourceNames(RT_GROUP_ICON)
# reads what is really in the file.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)][string] $Path
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    Write-Output "::error::No such file: $Path"
    exit 1
}

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class ExeIcon
{
    private const uint LoadLibraryAsDatafile = 0x2;
    private static readonly IntPtr RtGroupIcon = (IntPtr)14;

    private delegate bool EnumResNameProc(IntPtr module, IntPtr type, IntPtr name, IntPtr param);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr LoadLibraryExW(string fileName, IntPtr file, uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FreeLibrary(IntPtr module);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool EnumResourceNamesW(IntPtr module, IntPtr type, EnumResNameProc callback, IntPtr param);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr FindResourceW(IntPtr module, IntPtr name, IntPtr type);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LoadResource(IntPtr module, IntPtr resourceInfo);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LockResource(IntPtr resourceData);

    /// <summary>
    /// The image sizes of the first icon group in the file, as "WxH" strings,
    /// or null when the file has no icon group resource at all.
    /// </summary>
    public static string[] GroupIconSizes(string path)
    {
        IntPtr module = LoadLibraryExW(path, IntPtr.Zero, LoadLibraryAsDatafile);
        if (module == IntPtr.Zero)
            throw new InvalidOperationException(
                "LoadLibraryEx failed for " + path + ", Win32 error " + Marshal.GetLastWin32Error());

        try
        {
            var ids = new List<IntPtr>();
            EnumResNameProc callback = (m, t, name, p) =>
            {
                ids.Add(name);
                return true;
            };
            if (!EnumResourceNamesW(module, RtGroupIcon, callback, IntPtr.Zero))
                return null;
            GC.KeepAlive(callback);
            if (ids.Count == 0)
                return null;

            IntPtr info = FindResourceW(module, ids[0], RtGroupIcon);
            if (info == IntPtr.Zero)
                return null;
            IntPtr data = LockResource(LoadResource(module, info));
            if (data == IntPtr.Zero)
                return null;

            // GRPICONDIR: reserved (2), type (2), count (2), then 14 bytes per
            // GRPICONDIRENTRY. Width and height are bytes, 0 meaning 256.
            int count = Marshal.ReadInt16(data, 4);
            var sizes = new List<string>();
            for (int i = 0; i < count; i++)
            {
                int entry = 6 + (14 * i);
                int width = Marshal.ReadByte(data, entry);
                int height = Marshal.ReadByte(data, entry + 1);
                sizes.Add((width == 0 ? 256 : width) + "x" + (height == 0 ? 256 : height));
            }
            return sizes.ToArray();
        }
        finally
        {
            FreeLibrary(module);
        }
    }
}
'@

$sizes = [ExeIcon]::GroupIconSizes($Path)
if ($null -eq $sizes) {
    Write-Output "::error::$Path carries no icon resource"
    exit 1
}

Write-Output "${Path}: icon group with $($sizes.Count) images: $($sizes -join ', ')"

# 32 px is what Explorer's medium and large views use, 256 px is what the
# taskbar and a high-DPI desktop pick; both come from the generated .ico.
foreach ($needed in @('32x32', '256x256')) {
    if ($sizes -notcontains $needed) {
        Write-Output "::error::$Path is missing the $needed icon image"
        exit 1
    }
}
