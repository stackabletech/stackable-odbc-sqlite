<#
.SYNOPSIS
    Create or edit a Stackable SQLite ODBC data source.

.DESCRIPTION
    Presents a dialog covering the driver's whole connection-string surface,
    which for SQLite is one keyword, and writes the result as an ODBC data
    source.

    The write goes through the installer's SQLConfigDataSource, which calls the
    driver's own ConfigDSN entry point, rather than writing the registry
    directly. That keeps the driver in the loop and inherits whatever validation
    it performs.

    This is also what the ODBC Data Source Administrator's "Add..." and
    "Configure..." buttons display. Those load the driver's setup DLL and ask
    it for a dialog; the driver's Backend::configure_dsn hook runs this script
    with -Emit and writes the keywords it returns. Run the script directly to
    get the same dialog without going through the Administrator.

.PARAMETER Dsn
    Data source to edit. Omitted, the dialog starts empty.

.PARAMETER System
    Start on System scope (HKLM) rather than User (HKCU). Needs elevation.

.PARAMETER NoGui
    Write the data source from -Set without displaying a dialog. Intended for
    scripted installs and for testing the write path.

.PARAMETER Set
    Key/value pairs for -NoGui, keyed by connection-string keyword.

.PARAMETER Emit
    Display the dialog and print the resulting keywords to stdout as JSON
    instead of writing a data source. Reads the keywords to pre-fill from
    stdin, also as JSON. This is the mode the driver's ConfigDSN hook uses:
    the driver, not this script, performs the write.

    Exit codes are the channel for the verdict, because stdout carries the
    payload: 0 accepted, 2 cancelled, anything else a failure whose reason is
    on stderr.

.EXAMPLE
    .\configure-dsn.ps1

.EXAMPLE
    .\configure-dsn.ps1 -Dsn sales_db

.EXAMPLE
    .\configure-dsn.ps1 -NoGui -Set @{ DSN='sales_db'; Database='C:\data\sales.db' }
#>
[CmdletBinding()]
param(
    [string]$Dsn,
    [switch]$System,
    [switch]$NoGui,
    [hashtable]$Set,
    [switch]$Emit,
    [string]$DriverName = 'stackable_odbc_sqlite'
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Field table
# ---------------------------------------------------------------------------
# The one place a connection-string keyword is named. Layout, the read path,
# the write path and validation are all generated from this, so adding a
# keyword is one entry here rather than an edit in four places. SQLite needs
# exactly one; the table is still a table so that a second one costs an entry
# rather than a rewrite, which is the shape the Trino driver grew into.
#
# Key      the connection-string keyword, lower case, matching the PARAM_
#          constants in src/backend/types/connect_params.rs.
# Type     Text | File
#
# `dsn_keys_match_the_connection_string_parser` in src/lib.rs fails the build
# if this list and the parser ever disagree.

$script:Fields = @(
    @{ Key='database'; Label='Database file'; Type='File'; Required=$true
       Help='Path to the SQLite database file, or :memory: for a throwaway in-memory database. A file that does not exist yet is created on first connect.' }
)

function Get-Field { param([string]$Key) $script:Fields | Where-Object { $_.Key -eq $Key } }

function Get-FieldDefault {
    param($Field)
    if ($Field.Contains('Default')) { return $Field.Default }
    return ''
}

function ConvertTo-FieldValues {
    <#
        Normalise a caller-supplied keyword map onto the field table's own
        keys: case folded, DSN lifted out as the name.

        Shared by -NoGui and -Emit, the two paths whose input comes from a
        caller rather than from the dialog, and so the only two that can be
        handed a keyword the table does not carry.

        Unknown keywords are kept aside in Extra rather than rejected. -Emit
        receives a data source's whole stored section, which carries keywords
        this dialog does not model (Driver, and anything written by hand), and
        returning fewer keywords than arrived would delete them.
        -NoGui rejects them instead: there the map is something a person just
        typed, so an unrecognised keyword is far more likely a typo than a
        keyword worth preserving, and silently ignoring it would write a data
        source missing the setting they asked for.
    #>
    param([hashtable]$Set, [switch]$KeepUnknown)

    $values = @{}
    $extra = @{}
    $name = ''
    foreach ($k in $Set.Keys) {
        $lk = "$k".ToLowerInvariant()
        if ($lk -eq 'dsn') { $name = "$($Set[$k])"; continue }
        $f = Get-Field $lk
        if (-not $f) {
            if ($KeepUnknown) { $extra[$k] = "$($Set[$k])"; continue }
            throw "Unknown connection-string keyword: $k"
        }
        $values[$f.Key] = "$($Set[$k])"
    }
    @{ Values = $values; Name = $name; Extra = $extra }
}

# ---------------------------------------------------------------------------
# ODBC installer interop
# ---------------------------------------------------------------------------

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class OdbcInstaller {
    // BOOL, so 4 bytes: the default bool marshalling is correct here.
    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool SQLConfigDataSourceW(IntPtr hwndParent, ushort fRequest,
        string lpszDriver, string lpszAttributes);

    // RETCODE is SQLSMALLINT: 16 bits. Declaring this as bool reads the wrong
    // width and loses the error record entirely.
    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode)]
    public static extern short SQLInstallerErrorW(ushort iError, out int pfErrorCode,
        StringBuilder lpszErrorMsg, ushort cbErrorMsgMax, out ushort pcbErrorMsg);

    [DllImport("odbccp32.dll", CharSet = CharSet.Unicode)]
    public static extern int SQLGetPrivateProfileStringW(string lpszSection, string lpszEntry,
        string lpszDefault, StringBuilder RetBuffer, int cbRetBuffer, string lpszFilename);

    [DllImport("odbccp32.dll")]
    public static extern bool SQLSetConfigMode(ushort wConfigMode);
}
"@ -ErrorAction SilentlyContinue

# ConfigDSN fRequest values, from odbcinst.h.
$script:ODBC_ADD_DSN        = 1
$script:ODBC_CONFIG_DSN     = 2
$script:ODBC_ADD_SYS_DSN    = 4
$script:ODBC_CONFIG_SYS_DSN = 5
# SQLSetConfigMode values.
$script:ODBC_USER_DSN   = 1
$script:ODBC_SYSTEM_DSN = 2

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-InstallerErrors {
    <# Drain the installer error buffer. Empty when the last call succeeded. #>
    $out = @()
    for ($i = 1; $i -le 8; $i++) {
        $code = 0; $pcb = 0
        $sb = New-Object System.Text.StringBuilder 1024
        $rc = [OdbcInstaller]::SQLInstallerErrorW([uint16]$i, [ref]$code, $sb, [uint16]1024, [ref]$pcb)
        # SQL_SUCCESS = 0, SQL_SUCCESS_WITH_INFO = 1; anything else ends the list.
        if ($rc -ne 0 -and $rc -ne 1) { break }
        $out += "[$code] $($sb.ToString())"
    }
    $out
}

function Read-Dsn {
    <#
        Pre-fill from an existing data source. Returns a hashtable keyed by
        connection-string keyword, holding only the keywords present.
    #>
    param([string]$Name, [bool]$IsSystem)

    $mode = if ($IsSystem) { $script:ODBC_SYSTEM_DSN } else { $script:ODBC_USER_DSN }
    [void][OdbcInstaller]::SQLSetConfigMode([uint16]$mode)

    $values = @{}
    foreach ($f in $script:Fields) {
        $sb = New-Object System.Text.StringBuilder 4096
        $n = [OdbcInstaller]::SQLGetPrivateProfileStringW($Name, $f.Key, '', $sb, 4096, 'ODBC.INI')
        if ($n -gt 0) { $values[$f.Key] = $sb.ToString() }
    }
    [void][OdbcInstaller]::SQLSetConfigMode(0)
    $values
}

function Get-ExistingDsnNames {
    param([bool]$IsSystem)
    $hive = if ($IsSystem) { 'HKLM:' } else { 'HKCU:' }
    $path = "$hive\SOFTWARE\ODBC\ODBC.INI\ODBC Data Sources"
    if (-not (Test-Path $path)) { return @() }
    $item = Get-Item $path
    $item.GetValueNames() | Where-Object { $item.GetValue($_) -eq $DriverName } | Sort-Object
}

function Write-Dsn {
    <# Write the data source through the driver's own ConfigDSN. #>
    param([hashtable]$Values, [string]$Name, [bool]$IsSystem, [bool]$Replace)

    $pairs = @("DSN=$Name")
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $v = $Values[$f.Key]
        if ([string]::IsNullOrEmpty($v)) { continue }
        $pairs += "$($f.Key)=$v"
    }
    # ConfigDSN takes a doubly null-terminated list of keyword-value pairs.
    $attributes = ($pairs -join "`0") + "`0"

    $request = if ($IsSystem) {
        if ($Replace) { $script:ODBC_CONFIG_SYS_DSN } else { $script:ODBC_ADD_SYS_DSN }
    } else {
        if ($Replace) { $script:ODBC_CONFIG_DSN } else { $script:ODBC_ADD_DSN }
    }

    [void](Get-InstallerErrors)   # clear anything stale before the call
    $ok = [OdbcInstaller]::SQLConfigDataSourceW([IntPtr]::Zero, [uint16]$request,
                                                $DriverName, $attributes)
    if (-not $ok) {
        # @() around the call: PowerShell unrolls an empty array return to
        # $null, and Set-StrictMode makes .Count on it an error.
        $errs = @(Get-InstallerErrors)
        $detail = if ($errs.Count) { $errs -join "`r`n" } else { 'the installer reported no detail' }
        throw "Writing the data source failed:`r`n$detail"
    }
}

function Build-ConnectionString {
    <#
        A DSN-less connection string for the Test button, so a configuration is
        proved before it is written.
    #>
    param([hashtable]$Values)

    $parts = @("Driver=$DriverName")
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $v = $Values[$f.Key]
        if ([string]::IsNullOrEmpty($v)) { continue }
        $parts += "$($f.Key)=$v"
    }
    ($parts -join ';') + ';'
}

function Test-DsnConnection {
    param([hashtable]$Values)

    $cs = Build-ConnectionString $Values
    $conn = New-Object System.Data.Odbc.OdbcConnection $cs
    # Opening a SQLite file is local and immediate, so this bounds a hung
    # network filesystem rather than a slow server.
    $conn.ConnectionTimeout = 15
    try {
        $conn.Open()
        $cmd = $conn.CreateCommand()
        # The version of the SQLite compiled into the driver, and the count of
        # tables in the file. The count is what distinguishes "opened your
        # database" from "created an empty file because the path was wrong",
        # which is the mistake this button exists to catch: SQLite creates a
        # missing file rather than refusing, so a typo in the path connects
        # perfectly well and finds nothing.
        $cmd.CommandText =
            "SELECT sqlite_version(), (SELECT count(*) FROM sqlite_master WHERE type = 'table')"
        $r = $cmd.ExecuteReader()
        $facts = @()
        $tables = $null
        if ($r.Read()) {
            $tables = "$($r[1])"
            # Objects rather than two-element arrays: PowerShell flattens a
            # nested array literal, so a list of pairs collapses into a list of
            # strings and indexing a "pair" then indexes into a *string*.
            $facts = @(
                [PSCustomObject]@{ Name = 'Database'; Value = "$($Values['database'])" }
                [PSCustomObject]@{ Name = 'SQLite';   Value = "$($r[0])" }
                [PSCustomObject]@{ Name = 'Tables';   Value = $tables }
            )
        }
        $r.Close()
        # Held in its own variable rather than read back out of $facts by
        # index: an index into that list silently follows any reordering of it,
        # and under Set-StrictMode an out-of-range one is a terminating error
        # inside a handler WinForms would swallow.
        $message = if ($tables -eq '0') {
            'Connected, but the database holds no tables.'
        } else {
            'Connected.'
        }
        return @{ Ok = $true; Message = $message; Facts = $facts }
    } catch {
        return @{ Ok = $false; Message = $_.Exception.Message; Facts = @() }
    } finally {
        if ($conn.State -ne 'Closed') { $conn.Close() }
    }
}

function Show-ConnectionResult {
    <#
        Report a connection test.

        A success gets its own small form rather than a MessageBox, because the
        facts are a two-column table and a MessageBox cannot align one: its
        font is proportional, so padding a label with spaces lines nothing up.
        A failure stays a MessageBox, because the driver's diagnostic is a
        paragraph and not a table.
    #>
    param([hashtable]$Result)

    if (-not $Result.Ok) {
        [void][System.Windows.Forms.MessageBox]::Show($Result.Message,
            'Connection failed', 'OK', 'Error')
        return
    }

    $dlg = New-Object System.Windows.Forms.Form
    $dlg.Text = 'Connection succeeded'
    $dlg.FormBorderStyle = 'FixedDialog'
    $dlg.StartPosition = 'CenterScreen'
    $dlg.MinimizeBox = $false
    $dlg.MaximizeBox = $false
    $dlg.ShowInTaskbar = $false
    # Same reason the main dialog sets it under -Emit: this belongs to a
    # separate process from the ODBC Administrator that is waiting on it.
    $dlg.TopMost = [bool]$Emit
    # The form sizes itself to the layout below. Positioning by hand from a
    # panel's Right/Bottom does not work, because an AutoSize panel has not
    # been measured yet at that point. That yields a window sized from stale
    # bounds, invisible and modal, which locks its parent out of all input with
    # nothing on screen to explain why.
    $dlg.AutoSize = $true
    $dlg.AutoSizeMode = 'GrowAndShrink'
    $dlg.Padding = New-Object System.Windows.Forms.Padding(14)

    $root = New-Object System.Windows.Forms.TableLayoutPanel
    $root.ColumnCount = 2
    $root.AutoSize = $true
    $root.AutoSizeMode = 'GrowAndShrink'
    $root.Dock = 'Fill'

    $icon = New-Object System.Windows.Forms.PictureBox
    $icon.Image = [System.Drawing.SystemIcons]::Information.ToBitmap()
    $icon.SizeMode = 'AutoSize'
    $icon.Margin = New-Object System.Windows.Forms.Padding(4, 4, 14, 8)
    $root.Controls.Add($icon, 0, 0)

    $head = New-Object System.Windows.Forms.Label
    $head.Text = $Result.Message
    $head.Font = New-Object System.Drawing.Font($dlg.Font, [System.Drawing.FontStyle]::Bold)
    $head.AutoSize = $true
    $head.Margin = New-Object System.Windows.Forms.Padding(0, 8, 0, 10)
    $root.Controls.Add($head, 1, 0)

    # Two columns, so the values share a left edge whatever the labels measure.
    $grid = New-Object System.Windows.Forms.TableLayoutPanel
    $grid.ColumnCount = 2
    $grid.AutoSize = $true
    $grid.AutoSizeMode = 'GrowAndShrink'
    $grid.Margin = New-Object System.Windows.Forms.Padding(0)
    foreach ($f in $Result.Facts) {
        $k = New-Object System.Windows.Forms.Label
        $k.Text = "$($f.Name):"
        $k.AutoSize = $true
        $k.Margin = New-Object System.Windows.Forms.Padding(0, 3, 16, 3)
        $v = New-Object System.Windows.Forms.Label
        $v.Text = $f.Value
        $v.AutoSize = $true
        $v.Margin = New-Object System.Windows.Forms.Padding(0, 3, 0, 3)
        $grid.Controls.Add($k)
        $grid.Controls.Add($v)
    }
    $root.Controls.Add($grid, 1, 1)

    $ok = New-Object System.Windows.Forms.Button
    $ok.Text = 'OK'
    $ok.Size = New-Object System.Drawing.Size(90, 28)
    $ok.Anchor = 'Right'
    $ok.Margin = New-Object System.Windows.Forms.Padding(0, 16, 0, 0)
    $ok.DialogResult = [System.Windows.Forms.DialogResult]::OK
    $root.Controls.Add($ok, 1, 2)

    $dlg.Controls.Add($root)
    $dlg.AcceptButton = $ok
    $dlg.CancelButton = $ok

    [void]$dlg.ShowDialog()
    $dlg.Dispose()
}

function Test-Values {
    <#
        Only the rules that are cheap and certain here. Everything else is left
        to the driver, which is the authority and reports through
        SQLGetDiagRec; duplicating its rules would let the two disagree.

        In particular the database path is *not* checked for existence. SQLite
        creates a missing file on first connect, so a path that does not exist
        yet is a legitimate way to make a new database, and refusing it here
        would forbid something the driver permits.
    #>
    param([hashtable]$Values, [string]$Name)

    $problems = @()
    if ([string]::IsNullOrWhiteSpace($Name)) { $problems += 'A data source name is required.' }
    foreach ($f in $script:Fields | Where-Object { $_.Contains('Required') -and $_.Required }) {
        if (-not $Values.Contains($f.Key) -or [string]::IsNullOrWhiteSpace($Values[$f.Key])) {
            $problems += "$($f.Label) is required."
        }
    }
    $problems
}

# ---------------------------------------------------------------------------
# Headless path
# ---------------------------------------------------------------------------

if ($NoGui) {
    if (-not $Set) { throw '-NoGui requires -Set.' }

    $parsed = ConvertTo-FieldValues $Set
    $values = $parsed.Values
    $name = if ($parsed.Name) { $parsed.Name } else { $Dsn }

    $problems = @(Test-Values $values $name)
    if ($problems.Count) { throw ($problems -join "`r`n") }

    if ($System -and -not (Test-Elevated)) {
        throw 'A System data source needs an elevated session. Run as Administrator, or omit -System.'
    }
    $exists = @(Get-ExistingDsnNames ([bool]$System)) -contains $name
    Write-Dsn $values $name ([bool]$System) $exists
    $scope = if ($System) { 'System' } else { 'User' }
    Write-Output "$scope data source '$name' written."
    return
}

# ---------------------------------------------------------------------------
# Emit mode input
# ---------------------------------------------------------------------------
# The keywords to pre-fill arrive on stdin as a JSON object. A pipe rather than
# a file because a Configure... payload is the data source's whole stored
# section, and this script does not get to assume every keyword in it is one
# this driver models and therefore harmless on disk.

$script:EmitExtra = @{}
$script:EmitPrefill = @{}
$script:EmitValues = @{}
$script:EmitNameFixed = $false

if ($Emit) {
    $stdin = [Console]::In.ReadToEnd()
    $incoming = @{}
    if (-not [string]::IsNullOrWhiteSpace($stdin)) {
        $json = $stdin | ConvertFrom-Json
        foreach ($p in $json.PSObject.Properties) { $incoming[$p.Name] = "$($p.Value)" }
    }

    $parsed = ConvertTo-FieldValues $incoming -KeepUnknown
    $script:EmitExtra = $parsed.Extra
    $script:EmitPrefill = $parsed.Values
    if ($parsed.Name) {
        $Dsn = $parsed.Name
        # The spec: "if a data source name was passed to it, ConfigDSN displays
        # that name but does not allow the user to change it." The driver's
        # core enforces this on the map coming back, so an editable box here
        # would only produce a failed call.
        $script:EmitNameFixed = $true
    }
}

# ---------------------------------------------------------------------------
# Dialog
# ---------------------------------------------------------------------------

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Stackable SQLite ODBC - Data Source'
# Tall enough for the header row, one field row, the button row and the hint
# beneath it, with the title bar and border taken off the top. Grown rather
# than fitted exactly: a larger system font pushes every row down, and a
# clipped hint is worse than a little empty space.
$form.Size = New-Object System.Drawing.Size(620, 260)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedDialog'
$form.MaximizeBox = $false
# The ODBC Administrator owns the foreground while it waits on ConfigDSN, and
# this dialog belongs to a separate process, so without this it opens behind
# the window that asked for it.
$form.TopMost = [bool]$Emit

$tip = New-Object System.Windows.Forms.ToolTip
$tip.AutoPopDelay = 20000

# --- header: name and scope ---
$lblName = New-Object System.Windows.Forms.Label
$lblName.Text = 'Data source name'
$lblName.Location = New-Object System.Drawing.Point(12, 15)
$lblName.Size = New-Object System.Drawing.Size(130, 20)
$form.Controls.Add($lblName)

$txtName = New-Object System.Windows.Forms.TextBox
$txtName.Location = New-Object System.Drawing.Point(148, 12)
$txtName.Size = New-Object System.Drawing.Size(200, 22)
$form.Controls.Add($txtName)

$rbUser = New-Object System.Windows.Forms.RadioButton
$rbUser.Text = 'User'
$rbUser.Location = New-Object System.Drawing.Point(370, 11)
$rbUser.Size = New-Object System.Drawing.Size(60, 24)
$rbUser.Checked = -not $System
$form.Controls.Add($rbUser)

$rbSystem = New-Object System.Windows.Forms.RadioButton
$rbSystem.Text = 'System'
$rbSystem.Location = New-Object System.Drawing.Point(434, 11)
$rbSystem.Size = New-Object System.Drawing.Size(80, 24)
$rbSystem.Checked = [bool]$System
$form.Controls.Add($rbSystem)

$lblElev = New-Object System.Windows.Forms.Label
$lblElev.Location = New-Object System.Drawing.Point(370, 36)
$lblElev.Size = New-Object System.Drawing.Size(220, 18)
$lblElev.ForeColor = [System.Drawing.Color]::FromArgb(160, 90, 0)
if (-not (Test-Elevated)) {
    $lblElev.Text = 'System needs an elevated session'
    $rbSystem.Enabled = $false
    if ($System) { $rbUser.Checked = $true }
}
$form.Controls.Add($lblElev)

# Under -Emit the driver performs the write, and the Administrator has already
# chosen the scope and set the installer's config mode accordingly. Offering a
# choice the dialog cannot honour would be a lie, so the radios go away.
if ($Emit) {
    $rbUser.Visible = $false
    $rbSystem.Visible = $false
    $lblElev.Visible = $false
}
if ($script:EmitNameFixed) { $txtName.ReadOnly = $true }

# --- fields, built from the field table ---
# A flat panel rather than the Trino driver's TabControl: one keyword does not
# need tabs, and the loop is the same shape either way if a second arrives.
$panel = New-Object System.Windows.Forms.Panel
$panel.Location = New-Object System.Drawing.Point(12, 62)
$panel.Size = New-Object System.Drawing.Size(580, 70)
$form.Controls.Add($panel)

$script:Controls = @{}

$y = 6
foreach ($f in $script:Fields) {
    $label = New-Object System.Windows.Forms.Label
    $label.Text = $f.Label
    $label.Location = New-Object System.Drawing.Point(0, ($y + 3))
    $label.Size = New-Object System.Drawing.Size(130, 20)
    $panel.Controls.Add($label)

    $ctl = New-Object System.Windows.Forms.TextBox
    $ctl.Location = New-Object System.Drawing.Point(136, $y)
    $ctl.Text = (Get-FieldDefault $f)

    if ($f.Type -eq 'File') {
        $ctl.Size = New-Object System.Drawing.Size(340, 22)
        $browse = New-Object System.Windows.Forms.Button
        $browse.Text = 'Browse...'
        $browse.Location = New-Object System.Drawing.Point(482, ($y - 1))
        $browse.Size = New-Object System.Drawing.Size(90, 24)
        $target = $ctl
        $browse.Add_Click({
            $dlg = New-Object System.Windows.Forms.OpenFileDialog
            $dlg.Filter = 'SQLite databases (*.db;*.sqlite;*.sqlite3)|*.db;*.sqlite;*.sqlite3|All files (*.*)|*.*'
            # SQLite creates a database that is not there yet, so naming one is
            # how a new database is made. The default would refuse the name and
            # send the user off to create an empty file by hand first.
            $dlg.CheckFileExists = $false
            if ($dlg.ShowDialog() -eq 'OK') { $target.Text = $dlg.FileName }
        }.GetNewClosure())
        $panel.Controls.Add($browse)
    } else {
        $ctl.Size = New-Object System.Drawing.Size(436, 22)
    }

    if ($f.Contains('Help')) { $tip.SetToolTip($ctl, $f.Help) }
    $panel.Controls.Add($ctl)
    $script:Controls[$f.Key] = $ctl
    $y += 30
}

function Get-FormValues {
    $values = @{}
    foreach ($f in $script:Fields) {
        $v = $script:Controls[$f.Key].Text
        if (-not [string]::IsNullOrEmpty($v)) { $values[$f.Key] = $v }
    }
    $values
}

function Set-FormValues {
    param([hashtable]$Values)
    foreach ($f in $script:Fields) {
        if (-not $Values.Contains($f.Key)) { continue }
        $script:Controls[$f.Key].Text = $Values[$f.Key]
    }
}

# --- buttons ---
$lblHint = New-Object System.Windows.Forms.Label
$lblHint.Text = 'A file that does not exist yet is created on first connect.'
$lblHint.Location = New-Object System.Drawing.Point(12, 176)
$lblHint.Size = New-Object System.Drawing.Size(580, 18)
$lblHint.ForeColor = [System.Drawing.Color]::FromArgb(110, 110, 110)
$form.Controls.Add($lblHint)

$btnTest = New-Object System.Windows.Forms.Button
$btnTest.Text = 'Test connection'
$btnTest.Location = New-Object System.Drawing.Point(12, 140)
$btnTest.Size = New-Object System.Drawing.Size(130, 30)
$btnTest.Add_Click({
    $values = Get-FormValues
    $problems = @(Test-Values $values $txtName.Text)
    if ($problems.Count) {
        [void][System.Windows.Forms.MessageBox]::Show(($problems -join "`r`n"),
            'Incomplete', 'OK', 'Warning')
        return
    }
    $form.Cursor = [System.Windows.Forms.Cursors]::WaitCursor
    $btnTest.Enabled = $false
    # One catch around the whole thing: WinForms swallows an exception thrown
    # from a handler, so anything uncaught here leaves the button looking as
    # though it did nothing at all.
    try {
        try { $result = Test-DsnConnection $values }
        finally { $form.Cursor = [System.Windows.Forms.Cursors]::Default; $btnTest.Enabled = $true }
        Show-ConnectionResult $result
    } catch {
        [void][System.Windows.Forms.MessageBox]::Show($_.Exception.ToString(),
            'Could not test the connection', 'OK', 'Error')
    }
})
$form.Controls.Add($btnTest)

$btnOk = New-Object System.Windows.Forms.Button
$btnOk.Text = 'OK'
$btnOk.Location = New-Object System.Drawing.Point(406, 140)
$btnOk.Size = New-Object System.Drawing.Size(90, 30)
$btnOk.Add_Click({
    $values = Get-FormValues
    $problems = @(Test-Values $values $txtName.Text)
    if ($problems.Count) {
        [void][System.Windows.Forms.MessageBox]::Show(($problems -join "`r`n"),
            'Incomplete', 'OK', 'Warning')
        return
    }
    if (-not $Emit) {
        $isSystem = $rbSystem.Checked
        $exists = @(Get-ExistingDsnNames $isSystem) -contains $txtName.Text
        try {
            Write-Dsn $values $txtName.Text $isSystem $exists
        } catch {
            [void][System.Windows.Forms.MessageBox]::Show($_.Exception.Message,
                'Could not write the data source', 'OK', 'Error')
            return
        }
    }
    # The handler is a scriptblock with its own scope, and -Emit needs these
    # after ShowDialog returns.
    $script:EmitValues = $values
    $form.DialogResult = [System.Windows.Forms.DialogResult]::OK
    $form.Close()
})
$form.Controls.Add($btnOk)

$btnCancel = New-Object System.Windows.Forms.Button
$btnCancel.Text = 'Cancel'
$btnCancel.Location = New-Object System.Drawing.Point(502, 140)
$btnCancel.Size = New-Object System.Drawing.Size(90, 30)
$btnCancel.Add_Click({ $form.DialogResult = [System.Windows.Forms.DialogResult]::Cancel; $form.Close() })
$form.Controls.Add($btnCancel)
$form.CancelButton = $btnCancel

# --- pre-fill when editing ---
if ($Dsn) { $txtName.Text = $Dsn }
if ($Emit) {
    # The driver has already merged the data source's stored keywords in, so
    # reading ODBC.INI again here would only be able to disagree with it.
    if ($script:EmitPrefill.Count) { Set-FormValues $script:EmitPrefill }
} elseif ($Dsn) {
    $existing = Read-Dsn $Dsn ([bool]$System)
    if ($existing.Count) { Set-FormValues $existing }
}

$result = $form.ShowDialog()

if (-not $Emit) {
    if ($result -eq [System.Windows.Forms.DialogResult]::OK) {
        $scope = if ($rbSystem.Checked) { 'System' } else { 'User' }
        Write-Output "$scope data source '$($txtName.Text)' written."
    }
    return
}

# --- emit mode: the verdict is the exit code, the payload is stdout ---
if ($result -ne [System.Windows.Forms.DialogResult]::OK) {
    # Cancelled. The driver returns Ok(None) and ConfigDSN posts no installer
    # error, because nothing failed.
    exit 2
}

$out = [ordered]@{ DSN = $txtName.Text }
# Keywords the dialog does not model are returned exactly as they arrived. On
# a Configure... this is the whole rest of the data source's section, and
# dropping them would delete settings the user never touched.
foreach ($k in $script:EmitExtra.Keys) { $out[$k] = $script:EmitExtra[$k] }
foreach ($f in $script:Fields) {
    if ($script:EmitValues.Contains($f.Key)) { $out[$f.Key] = $script:EmitValues[$f.Key] }
}
[Console]::Out.Write(($out | ConvertTo-Json -Compress -Depth 3))
exit 0
