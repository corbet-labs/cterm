# Windows UI automation test for cterm
# Launches the app, types commands, takes screenshots, and closes

param(
    [string]$CtermPath = "target\debug\cterm.exe",
    [string]$OutputDir = "test_output"
)

$ErrorActionPreference = "Stop"

# Create output directory
$OutputDir = [System.IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

# Log file
$LogFile = Join-Path $OutputDir "test.log"

function Log {
    param([string]$Message)
    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    "$timestamp - $Message" | Tee-Object -FilePath $LogFile -Append
}

function Wait-ForCtermLog {
    param(
        [string]$Pattern,
        [int]$TimeoutSeconds = 10
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $env:CTERM_LOG_FILE) -and
            (Select-String -Path $env:CTERM_LOG_FILE -Pattern $Pattern -Quiet)) {
            return
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out waiting for cterm log pattern: $Pattern"
}

function Get-CtermLogMatchCount {
    param([string]$Pattern)
    if (-not (Test-Path $env:CTERM_LOG_FILE)) {
        return 0
    }
    return @(
        Select-String -Path $env:CTERM_LOG_FILE -Pattern $Pattern -AllMatches
    ).Count
}

function Wait-ForCtermLogCount {
    param(
        [string]$Pattern,
        [int]$Minimum,
        [int]$TimeoutSeconds = 10
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if ((Get-CtermLogMatchCount -Pattern $Pattern) -ge $Minimum) {
            return
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out waiting for $Minimum cterm log matches: $Pattern"
}

function Wait-ForFileText {
    param(
        [string]$Path,
        [int]$TimeoutSeconds = 10
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $Path) {
            try {
                $content = (Get-Content -Raw $Path).Trim()
                if (-not [string]::IsNullOrWhiteSpace($content)) {
                    return $content
                }
            } catch {
                # The producer may still have the file open; retry until timeout.
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out waiting for non-empty file: $Path"
}

function Take-Screenshot {
    param(
        [string]$Name,
        [System.IntPtr]$Hwnd = [System.IntPtr]::Zero
    )

    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing

    $outputPath = Join-Path $OutputDir "$Name.png"

    if ($Hwnd -ne [System.IntPtr]::Zero) {
        # Get window rectangle
        $rect = New-Object RECT
        [User32]::GetWindowRect($Hwnd, [ref]$rect) | Out-Null

        $width = $rect.Right - $rect.Left
        $height = $rect.Bottom - $rect.Top

        if ($width -gt 0 -and $height -gt 0) {
            $bitmap = New-Object System.Drawing.Bitmap($width, $height)
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, [System.Drawing.Size]::new($width, $height))
            $bitmap.Save($outputPath, [System.Drawing.Imaging.ImageFormat]::Png)
            $graphics.Dispose()
            $bitmap.Dispose()
            Log "Screenshot saved: $outputPath"
            return $true
        }
    }

    # Fallback: full screen
    $screen = [System.Windows.Forms.Screen]::PrimaryScreen
    $bitmap = New-Object System.Drawing.Bitmap($screen.Bounds.Width, $screen.Bounds.Height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($screen.Bounds.Location, [System.Drawing.Point]::Empty, $screen.Bounds.Size)
    $bitmap.Save($outputPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Log "Full screen screenshot saved: $outputPath"
    return $true
}

# Add Win32 types
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Collections.Generic;

public struct RECT {
    public int Left;
    public int Top;
    public int Right;
    public int Bottom;
}

public class User32 {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern IntPtr FindWindow(string lpClassName, string lpWindowName);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, ref RECT lpRect);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumChildWindows(IntPtr hWndParent, EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern int GetWindowTextLength(IntPtr hWnd);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "SendMessageW")]
    public static extern IntPtr SendMessageValue(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "SendMessageW")]
    public static extern IntPtr SendMessageText(IntPtr hWnd, uint msg, IntPtr wParam, System.Text.StringBuilder lParam);

    [DllImport("user32.dll")]
    public static extern IntPtr GetMenu(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern int GetMenuItemCount(IntPtr hMenu);

    [DllImport("user32.dll")]
    public static extern IntPtr GetSubMenu(IntPtr hMenu, int nPos);

    [DllImport("user32.dll")]
    public static extern IntPtr GetDlgItem(IntPtr hDlg, int nIDDlgItem);

    [DllImport("user32.dll")]
    public static extern uint GetMenuItemID(IntPtr hMenu, int nPos);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "GetMenuStringW")]
    public static extern int GetMenuString(IntPtr hMenu, uint uIDItem, System.Text.StringBuilder lpString, int cchMax, uint flags);

    public const uint WM_CLOSE = 0x0010;
    public const uint WM_COMMAND = 0x0111;
    public const uint WM_GETTEXT = 0x000D;
    public const uint WM_GETTEXTLENGTH = 0x000E;
    public const uint BM_CLICK = 0x00F5;
    public const uint MF_BYPOSITION = 0x0400;

    public static IntPtr FindWindowByProcessId(uint processId) {
        IntPtr result = IntPtr.Zero;
        EnumWindows(delegate(IntPtr hWnd, IntPtr lParam) {
            uint pid;
            GetWindowThreadProcessId(hWnd, out pid);
            if (pid == processId && IsWindowVisible(hWnd)) {
                int length = GetWindowTextLength(hWnd);
                if (length > 0) {
                    result = hWnd;
                    return false; // Stop enumeration
                }
            }
            return true; // Continue
        }, IntPtr.Zero);
        return result;
    }

    public static string GetDescendantText(IntPtr parent) {
        System.Text.StringBuilder result = new System.Text.StringBuilder();
        EnumChildWindows(parent, delegate(IntPtr hWnd, IntPtr lParam) {
            int length = SendMessageValue(
                hWnd,
                WM_GETTEXTLENGTH,
                IntPtr.Zero,
                IntPtr.Zero
            ).ToInt32();
            if (length > 0) {
                System.Text.StringBuilder text = new System.Text.StringBuilder(length + 1);
                SendMessageText(hWnd, WM_GETTEXT, new IntPtr(text.Capacity), text);
                if (text.Length > 0) {
                    if (result.Length > 0) {
                        result.AppendLine();
                    }
                    result.Append(text);
                }
            }
            return true;
        }, IntPtr.Zero);
        return result.ToString();
    }
}
"@

function Get-MenuLabel {
    param(
        [System.IntPtr]$Menu,
        [int]$Position
    )

    $label = [System.Text.StringBuilder]::new(256)
    [User32]::GetMenuString(
        $Menu,
        [uint32]$Position,
        $label,
        $label.Capacity,
        [User32]::MF_BYPOSITION
    ) | Out-Null
    return (($label.ToString() -replace '&', '') -split "`t")[0].Trim()
}

function Find-SubmenuByLabel {
    param(
        [System.IntPtr]$Menu,
        [string]$ExpectedLabel
    )

    for ($position = 0; $position -lt [User32]::GetMenuItemCount($Menu); $position++) {
        if ((Get-MenuLabel -Menu $Menu -Position $position) -eq $ExpectedLabel) {
            return [User32]::GetSubMenu($Menu, $position)
        }
    }
    return [System.IntPtr]::Zero
}

function Find-MenuCommandByLabel {
    param(
        [System.IntPtr]$Menu,
        [string]$ExpectedLabel
    )

    for ($position = 0; $position -lt [User32]::GetMenuItemCount($Menu); $position++) {
        if ((Get-MenuLabel -Menu $Menu -Position $position) -eq $ExpectedLabel) {
            return [User32]::GetMenuItemID($Menu, $position)
        }
    }
    throw "Menu command not found: $ExpectedLabel"
}

function Normalize-TestPath {
    param([string]$Path)
    return [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
}

Log "=== cterm UI Automation Test ==="
Log "Executable: $CtermPath"

# Check if executable exists
if (-not (Test-Path $CtermPath)) {
    Log "ERROR: cterm.exe not found at $CtermPath"
    throw "cterm.exe not found at $CtermPath"
}

$roamingAppData = [System.Environment]::GetFolderPath(
    [System.Environment+SpecialFolder]::ApplicationData
)
$configDir = Join-Path $roamingAppData "cterm\cterm\config"
$configPath = Join-Path $configDir "config.toml"
$shortcutsPath = Join-Path $configDir "shortcuts_windows.toml"
$localAppData = [System.Environment]::GetFolderPath(
    [System.Environment+SpecialFolder]::LocalApplicationData
)
$pluginDataDir = Join-Path $localAppData "cterm\cterm\data"
$pluginDir = Join-Path $pluginDataDir "plugins\org.example.ui"
$pluginGrantPath = Join-Path $pluginDataDir "plugin-grants.toml"
$pluginBackupDir = Join-Path $OutputDir "plugin-backup"
$hadConfig = Test-Path $configPath
$hadShortcuts = Test-Path $shortcutsPath
$hadPlugin = Test-Path $pluginDir
$hadPluginGrant = Test-Path $pluginGrantPath
$configBackup = if ($hadConfig) { [System.IO.File]::ReadAllBytes($configPath) } else { $null }
$shortcutsBackup = if ($hadShortcuts) { [System.IO.File]::ReadAllBytes($shortcutsPath) } else { $null }
$pluginGrantBackup = if ($hadPluginGrant) { [System.IO.File]::ReadAllBytes($pluginGrantPath) } else { $null }
$process = $null

try {
New-Item -ItemType Directory -Force -Path $configDir | Out-Null
if ($hadPlugin) {
    Copy-Item -Recurse -Force -Path $pluginDir -Destination $pluginBackupDir
    Remove-Item -Recurse -Force -Path $pluginDir
}
if (Test-Path -LiteralPath $pluginGrantPath) {
    [System.IO.File]::Delete($pluginGrantPath)
}
New-Item -ItemType Directory -Force -Path $pluginDir | Out-Null
$pluginManifest = @'
manifest_version = 1
id = "org.example.ui"
name = "UI Test Plugin"
version = "1.0.0"
abi = "1.0"

[[commands]]
id = "new-tab"
title = "Open Test Tab"

[capabilities.invoke-actions]
allow = ["cterm:new-tab"]
'@
[System.IO.File]::WriteAllText(
    (Join-Path $pluginDir "cterm-plugin.toml"),
    $pluginManifest
)
$pluginFixture = Join-Path $PSScriptRoot "..\crates\cterm-plugin-host\tests\fixtures\ui_new_tab.wasm.base64"
$pluginWasm = [Convert]::FromBase64String(
    ([System.IO.File]::ReadAllText($pluginFixture)).Trim()
)
[System.IO.File]::WriteAllBytes((Join-Path $pluginDir "plugin.wasm"), $pluginWasm)
Log "Prepared isolated native plugin fixture"

$toolWorkingDirectory = Join-Path $OutputDir "active cwd"
New-Item -ItemType Directory -Force -Path $toolWorkingDirectory | Out-Null
$workingDirectoryToml = ConvertTo-Json -Compress $toolWorkingDirectory
$configContent = @"
[general]
default_shell = "cmd.exe"
shell_args = ["/d"]
working_directory = $workingDirectoryToml
"@
$shortcutsContent = @'
[[tools]]
name = "Record Active CWD"
command = "powershell.exe"
args = ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "[System.IO.File]::WriteAllText($env:CTERM_UI_TOOL_MARKER, (Get-Location).Path)"]
'@
[System.IO.File]::WriteAllText($configPath, $configContent)
[System.IO.File]::WriteAllText($shortcutsPath, $shortcutsContent)

$activeCwdMarker = Join-Path $OutputDir "active-terminal-cwd.txt"
$toolCwdMarker = Join-Path $OutputDir "configured-tool-cwd.txt"
[System.IO.File]::Delete($activeCwdMarker)
[System.IO.File]::Delete($toolCwdMarker)
$env:CTERM_UI_TOOL_MARKER = $toolCwdMarker

# Set environment for logging
$env:RUST_LOG = "debug"
$env:CTERM_LOG_FILE = Join-Path $OutputDir "cterm.log"
[System.IO.File]::Delete($env:CTERM_LOG_FILE)

Log "Starting cterm..."
$process = Start-Process -FilePath $CtermPath -PassThru

Log "Process started with PID: $($process.Id)"

# Wait for window to appear
Log "Waiting for window (PID: $($process.Id))..."
$hwnd = [System.IntPtr]::Zero
$attempts = 0
$maxAttempts = 30

while ($hwnd -eq [System.IntPtr]::Zero -and $attempts -lt $maxAttempts) {
    Start-Sleep -Milliseconds 500
    $hwnd = [User32]::FindWindowByProcessId($process.Id)
    $attempts++
    if ($attempts % 5 -eq 0) {
        Log "  Attempt $attempts/$maxAttempts..."
    }
}

if ($hwnd -eq [System.IntPtr]::Zero) {
    Log "ERROR: Window not found after $maxAttempts attempts"
    # Take screenshot of whatever is visible
    Take-Screenshot -Name "error_no_window"

    # Check if process is still running
    if (-not $process.HasExited) {
        Log "Process is still running, killing..."
        $process.Kill()
    }

    # Copy cterm log if exists
    if (Test-Path $env:CTERM_LOG_FILE) {
        Log "cterm log contents:"
        Get-Content $env:CTERM_LOG_FILE | ForEach-Object { Log "  $_" }
    }

    throw "Window not found after $maxAttempts attempts"
}

Log "Window found: $hwnd"

# Bring window to foreground
[User32]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Seconds 1
Wait-ForCtermLog -Pattern "Pane source .* attached to daemon session"

# Take initial screenshot
Take-Screenshot -Name "01_startup" -Hwnd $hwnd

# Send keystrokes using SendKeys
Add-Type -AssemblyName System.Windows.Forms

# Record the active shell's real working directory before invoking a tool.
Log "Recording active terminal working directory..."
[System.Windows.Forms.SendKeys]::SendWait("cd > `"$activeCwdMarker`"")
[System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
$activeCwd = Normalize-TestPath (Wait-ForFileText -Path $activeCwdMarker)
$expectedCwd = Normalize-TestPath $toolWorkingDirectory
if (-not [string]::Equals($activeCwd, $expectedCwd, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Active terminal CWD mismatch: expected '$expectedCwd', got '$activeCwd'"
}
Log "Active terminal CWD verified: $activeCwd"

# Inspect the native menu hierarchy, then dispatch the exact configured tool
# command ID. This proves the product menu and command registry are connected,
# not merely that ToolShortcutEntry can spawn a process in isolation.
$menuBar = [User32]::GetMenu($hwnd)
if ($menuBar -eq [System.IntPtr]::Zero) {
    throw "cterm window has no native menu bar"
}
$toolsMenu = Find-SubmenuByLabel -Menu $menuBar -ExpectedLabel "Tools"
if ($toolsMenu -eq [System.IntPtr]::Zero) {
    throw "Tools submenu is missing from the native menu bar"
}
$toolCommandId = Find-MenuCommandByLabel -Menu $toolsMenu -ExpectedLabel "Record Active CWD"
if ($toolCommandId -eq [uint32]::MaxValue) {
    throw "Configured tool resolved to a submenu instead of a command"
}
Log "Invoking configured tool through native command ID $toolCommandId..."
if (-not [User32]::PostMessage(
    $hwnd,
    [User32]::WM_COMMAND,
    [System.IntPtr]$toolCommandId,
    [System.IntPtr]::Zero
)) {
    throw "Failed to dispatch configured tool command ID $toolCommandId"
}
Wait-ForCtermLog -Pattern "Launched configured tool 'Record Active CWD' in"
$toolCwd = Normalize-TestPath (Wait-ForFileText -Path $toolCwdMarker)
if (-not [string]::Equals($toolCwd, $activeCwd, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Configured tool CWD mismatch: active terminal '$activeCwd', tool '$toolCwd'"
}
Log "Tools submenu invoked configured tool in active CWD: $toolCwd"

$pluginsMenu = Find-SubmenuByLabel -Menu $toolsMenu -ExpectedLabel "Plugins"
if ($pluginsMenu -eq [System.IntPtr]::Zero) {
    throw "Plugins submenu is missing from the native Tools menu"
}
$pluginCommandId = Find-MenuCommandByLabel -Menu $pluginsMenu -ExpectedLabel "UI Test Plugin — Open Test Tab"
$attachCountBefore = Get-CtermLogMatchCount -Pattern "Pane source .* attached to daemon session"
Log "Invoking plugin through native command ID $pluginCommandId..."
if (-not [User32]::PostMessage(
    $hwnd,
    [User32]::WM_COMMAND,
    [System.IntPtr]$pluginCommandId,
    [System.IntPtr]::Zero
)) {
    throw "Failed to dispatch plugin command ID $pluginCommandId"
}

$pluginDialog = [System.IntPtr]::Zero
$dialogDeadline = (Get-Date).AddSeconds(10)
while ($pluginDialog -eq [System.IntPtr]::Zero -and (Get-Date) -lt $dialogDeadline) {
    $candidate = [User32]::FindWindow("#32770", "Allow plugin command?")
    if ($candidate -ne [System.IntPtr]::Zero) {
        [uint32]$dialogProcessId = 0
        [User32]::GetWindowThreadProcessId($candidate, [ref]$dialogProcessId) | Out-Null
        if ($dialogProcessId -eq $process.Id) {
            $pluginDialog = $candidate
            break
        }
    }
    Start-Sleep -Milliseconds 100
}
if ($pluginDialog -eq [System.IntPtr]::Zero) {
    throw "Native plugin approval dialog did not appear"
}
# MessageBox implementations do not promise a stable control ID for the text.
# Read every descendant through WM_GETTEXT and assert against the combined
# accessibility surface instead of assuming the traditional IDC_STATIC value.
$promptText = [User32]::GetDescendantText($pluginDialog)
if (-not $promptText.Contains("UI Test Plugin (org.example.ui)")) {
    throw "Plugin identity is absent from native prompt: $promptText"
}
if (-not $promptText.Contains("cterm:new-tab (new)")) {
    throw "Exact new action is absent from native prompt: $promptText"
}
$yesButton = [User32]::GetDlgItem($pluginDialog, 6)
if ($yesButton -eq [System.IntPtr]::Zero -or -not [User32]::PostMessage(
    $yesButton,
    [User32]::BM_CLICK,
    [System.IntPtr]::Zero,
    [System.IntPtr]::Zero
)) {
    throw "Could not accept the native plugin approval dialog"
}
Wait-ForCtermLogCount -Pattern "Pane source .* attached to daemon session" -Minimum ($attachCountBefore + 1)
if (-not (Test-Path $pluginGrantPath) -or
    -not (Select-String -Path $pluginGrantPath -SimpleMatch 'plugin = "org.example.ui"' -Quiet)) {
    throw "Accepted native plugin prompt did not persist its exact local grant"
}
Log "Plugin menu, native approval, isolated runner, and action dispatch succeeded"

Log "Typing 'echo hello world'..."
[System.Windows.Forms.SendKeys]::SendWait("echo hello world")
Start-Sleep -Milliseconds 500

# Take screenshot after typing
Take-Screenshot -Name "02_after_typing" -Hwnd $hwnd

# Press Enter
Log "Pressing Enter..."
[System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
Start-Sleep -Seconds 1

# Take screenshot after command execution
Take-Screenshot -Name "03_after_enter" -Hwnd $hwnd

# Capturing a file is not sufficient: require the native terminal frame to
# change after keyboard input so a frozen/blank Direct2D renderer fails CI.
$startupScreenshot = Join-Path $OutputDir "01_startup.png"
$commandScreenshot = Join-Path $OutputDir "03_after_enter.png"
$startupHash = (Get-FileHash -Algorithm SHA256 $startupScreenshot).Hash
$commandHash = (Get-FileHash -Algorithm SHA256 $commandScreenshot).Hash
if ($startupHash -eq $commandHash) {
    throw "Terminal screenshot did not change after command input"
}
Log "Renderer produced a changed frame after command input"

# Type another command
Log "Typing 'dir'..."
[System.Windows.Forms.SendKeys]::SendWait("dir")
Start-Sleep -Milliseconds 500
[System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
Start-Sleep -Seconds 1

# Take screenshot after dir
Take-Screenshot -Name "04_after_dir" -Hwnd $hwnd

# Test Ctrl+T for new tab
Log "Testing Ctrl+T (new tab)..."
[System.Windows.Forms.SendKeys]::SendWait("^t")
Start-Sleep -Seconds 1

# Take screenshot showing tabs
Take-Screenshot -Name "05_new_tab" -Hwnd $hwnd

# Pane commands are verified through semantic log markers; the screenshot is
# retained as a visual artifact but is not the pass/fail oracle.
Log "Testing Ctrl+Shift+Backslash (horizontal split)..."
[System.Windows.Forms.SendKeys]::SendWait("^+\")
Wait-ForCtermLog -Pattern "Split tab .*Horizontal"
Take-Screenshot -Name "06_split_pane" -Hwnd $hwnd

Log "Testing Ctrl+Shift+Minus (vertical split)..."
[System.Windows.Forms.SendKeys]::SendWait("^+-")
Wait-ForCtermLog -Pattern "Split tab .*Vertical"

Log "Testing Ctrl+Alt+Up (directional pane focus)..."
[System.Windows.Forms.SendKeys]::SendWait("^%{UP}")
Wait-ForCtermLog -Pattern "Focused pane .*Up"

Log "Testing Ctrl+Alt+Shift+Left (pane resize)..."
[System.Windows.Forms.SendKeys]::SendWait("^%+{LEFT}")
Wait-ForCtermLog -Pattern "Resized pane .*Left"

Log "Testing Ctrl+Shift+Enter (pane zoom)..."
[System.Windows.Forms.SendKeys]::SendWait("^+{ENTER}")
Wait-ForCtermLog -Pattern "Pane zoom true"

Log "Testing Ctrl+Shift+Enter (unzoom)..."
[System.Windows.Forms.SendKeys]::SendWait("^+{ENTER}")
Wait-ForCtermLog -Pattern "Pane zoom false"

Log "Testing Ctrl+Shift+Delete (close pane)..."
[System.Windows.Forms.SendKeys]::SendWait("^+{DELETE}")
Wait-ForCtermLog -Pattern "Closed pane"
Take-Screenshot -Name "07_closed_pane" -Hwnd $hwnd

# Close the window
Log "Closing window..."
[User32]::PostMessage($hwnd, [User32]::WM_CLOSE, [System.IntPtr]::Zero, [System.IntPtr]::Zero) | Out-Null

# Wait for process to exit
$exited = $process.WaitForExit(5000)
if (-not $exited) {
    Log "Process did not exit gracefully, killing..."
    $process.Kill()
    $process.WaitForExit(3000) | Out-Null
}

Log "Process exited with code: $($process.ExitCode)"

# Copy cterm log
if (Test-Path $env:CTERM_LOG_FILE) {
    Log ""
    Log "=== cterm application log ==="
    Get-Content $env:CTERM_LOG_FILE | ForEach-Object { Log $_ }
}

Log ""
Log "=== Test completed ==="
Log "Screenshots saved to: $OutputDir"
Get-ChildItem $OutputDir -Filter "*.png" | ForEach-Object { Log "  - $($_.Name)" }

} finally {
    if ($null -ne $process -and -not $process.HasExited) {
        [User32]::PostMessage(
            [User32]::FindWindowByProcessId($process.Id),
            [User32]::WM_CLOSE,
            [System.IntPtr]::Zero,
            [System.IntPtr]::Zero
        ) | Out-Null
        if (-not $process.WaitForExit(3000)) {
            $process.Kill()
            $process.WaitForExit(3000) | Out-Null
        }
    }

    if ($hadConfig) {
        [System.IO.File]::WriteAllBytes($configPath, $configBackup)
    } else {
        [System.IO.File]::Delete($configPath)
    }
    if ($hadShortcuts) {
        [System.IO.File]::WriteAllBytes($shortcutsPath, $shortcutsBackup)
    } else {
        [System.IO.File]::Delete($shortcutsPath)
    }
    if (Test-Path $pluginDir) {
        Remove-Item -Recurse -Force -Path $pluginDir
    }
    if ($hadPlugin) {
        Copy-Item -Recurse -Force -Path $pluginBackupDir -Destination $pluginDir
    }
    if ($hadPluginGrant) {
        [System.IO.File]::WriteAllBytes($pluginGrantPath, $pluginGrantBackup)
    } elseif (Test-Path -LiteralPath $pluginGrantPath) {
        [System.IO.File]::Delete($pluginGrantPath)
    }
    Remove-Item Env:\CTERM_UI_TOOL_MARKER -ErrorAction SilentlyContinue
}

exit 0
