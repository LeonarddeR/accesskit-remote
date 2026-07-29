<#
    uia.ps1 - Windows PowerShell 5.1 helper for driving the AccessKit remote
    provider on WSLg RAIL windows via System.Windows.Automation (UIA). Run it
    with powershell.exe, not pwsh. It finds the RAIL_WINDOW top-level element
    inside msrdc.exe, locates its AccessKit-FrameworkId child, and lets you
    list/walk/drive that subtree.

    Examples:
      powershell.exe -File uia.ps1 list
      powershell.exe -File uia.ps1 tree -Window "Text Editor"
      powershell.exe -File uia.ps1 invoke -Name "New Tab"
      powershell.exe -File uia.ps1 toggle -Name "Word Wrap"
      powershell.exe -File uia.ps1 expand -Name "Recent Files"
      powershell.exe -File uia.ps1 collapse -Name "Recent Files"
      powershell.exe -File uia.ps1 select -Name "Tab 2"
      powershell.exe -File uia.ps1 setvalue -Name "Zoom" -Value 150
      powershell.exe -File uia.ps1 range -Name "Zoom"
      powershell.exe -File uia.ps1 focus -Seconds 20
      powershell.exe -File uia.ps1 activate -Window "Text Editor"
#>

param(
    [Parameter(Position = 0)]
    [string]$Command,

    [string]$Window,
    [string]$Name,
    [double]$Value,
    [int]$Seconds = 15
)

try {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes -ErrorAction Stop
} catch {
    Write-Error ("Failed to load UI Automation assemblies. Run this script with powershell.exe (Windows PowerShell 5.1), not pwsh. Details: {0}" -f $_.Exception.Message)
    exit 1
}

if (-not ([System.Management.Automation.PSTypeName]'UiaNative').Type) {
    try {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class UiaNative
{
    [DllImport("user32.dll", SetLastError = true)]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool SetForegroundWindow(IntPtr hWnd);
}
'@ -ErrorAction Stop
    } catch {
        Write-Error ("Failed to define native Win32 helper type: {0}" -f $_.Exception.Message)
        exit 1
    }
}

# ---- Helpers ---------------------------------------------------------------

function Format-ElementInfo {
    param([System.Windows.Automation.AutomationElement]$Element)

    $ct = '<unknown>'
    $nm = ''
    $aid = ''
    $cls = ''
    try { $ct = $Element.Current.ControlType.ProgrammaticName } catch { }
    try { $nm = $Element.Current.Name } catch { }
    try { $aid = $Element.Current.AutomationId } catch { }
    try { $cls = $Element.Current.ClassName } catch { }

    return ("ControlType={0} Name='{1}' AutomationId='{2}' ClassName='{3}'" -f $ct, $nm, $aid, $cls)
}

function Get-PatternIdentifier {
    param([string]$Label)

    switch ($Label) {
        'Invoke'         { return [System.Windows.Automation.InvokePattern]::Pattern }
        'Toggle'         { return [System.Windows.Automation.TogglePattern]::Pattern }
        'ExpandCollapse' { return [System.Windows.Automation.ExpandCollapsePattern]::Pattern }
        'SelectionItem'  { return [System.Windows.Automation.SelectionItemPattern]::Pattern }
        'RangeValue'     { return [System.Windows.Automation.RangeValuePattern]::Pattern }
        'Value'          { return [System.Windows.Automation.ValuePattern]::Pattern }
        'Text'           { return [System.Windows.Automation.TextPattern]::Pattern }
        default          { return $null }
    }
}

function Get-PatternAvailableProperty {
    param([string]$Label)

    switch ($Label) {
        'Invoke'         { return [System.Windows.Automation.AutomationElement]::IsInvokePatternAvailableProperty }
        'Toggle'         { return [System.Windows.Automation.AutomationElement]::IsTogglePatternAvailableProperty }
        'ExpandCollapse' { return [System.Windows.Automation.AutomationElement]::IsExpandCollapsePatternAvailableProperty }
        'SelectionItem'  { return [System.Windows.Automation.AutomationElement]::IsSelectionItemPatternAvailableProperty }
        'RangeValue'     { return [System.Windows.Automation.AutomationElement]::IsRangeValuePatternAvailableProperty }
        'Value'          { return [System.Windows.Automation.AutomationElement]::IsValuePatternAvailableProperty }
        'Text'           { return [System.Windows.Automation.AutomationElement]::IsTextPatternAvailableProperty }
        default          { return $null }
    }
}

function Get-PatternFlags {
    param([System.Windows.Automation.AutomationElement]$Element)

    $labels = @('Invoke', 'Toggle', 'ExpandCollapse', 'SelectionItem', 'RangeValue', 'Value', 'Text')

    $supported = $null
    try { $supported = $Element.GetSupportedPatterns() } catch { $supported = $null }
    if ($null -eq $supported) { return '(patterns unavailable)' }

    $names = [System.Collections.Generic.List[string]]::new()
    foreach ($label in $labels) {
        $pat = Get-PatternIdentifier -Label $label
        if ($supported -contains $pat) {
            $names.Add($label)
        }
    }

    if ($names.Count -eq 0) { return '(none)' }
    return ($names -join ',')
}

function Get-AccessKitChild {
    param([System.Windows.Automation.AutomationElement]$Element)

    $prop = [System.Windows.Automation.AutomationElement]::FrameworkIdProperty
    $cond = [System.Windows.Automation.PropertyCondition]::new($prop, 'AccessKit')
    try {
        return $Element.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
    } catch {
        return $null
    }
}

# Bounded breadth-first search for the first descendant whose FrameworkId
# equals FrameworkId, visiting at most MaxElements nodes down to MaxDepth.
function Test-SubtreeHasFrameworkId {
    param(
        [System.Windows.Automation.AutomationElement]$Element,
        [string]$FrameworkId,
        [int]$MaxDepth = 6,
        [int]$MaxElements = 300
    )

    $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
    $visited = 0
    $queue = [System.Collections.Generic.Queue[psobject]]::new()
    $queue.Enqueue([pscustomobject]@{ El = $Element; Depth = 0 })

    while ($queue.Count -gt 0 -and $visited -lt $MaxElements) {
        $item = $queue.Dequeue()
        $el = $item.El
        $depth = $item.Depth
        $visited++

        try {
            if ($el.Current.FrameworkId -eq $FrameworkId) {
                return $el
            }
        } catch { }

        if ($depth -ge $MaxDepth) { continue }

        $child = $null
        try { $child = $walker.GetFirstChild($el) } catch { $child = $null }
        while ($null -ne $child -and $visited -lt $MaxElements) {
            $queue.Enqueue([pscustomobject]@{ El = $child; Depth = ($depth + 1) })
            $next = $null
            try { $next = $walker.GetNextSibling($child) } catch { $next = $null }
            $child = $next
        }
    }

    return $null
}

function Find-TargetRailWindow {
    param([string]$TitleSubstring)

    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $classProp = [System.Windows.Automation.AutomationElement]::ClassNameProperty
    $classCond = [System.Windows.Automation.PropertyCondition]::new($classProp, 'RAIL_WINDOW')

    $candidates = $null
    try {
        $candidates = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $classCond)
    } catch {
        return $null
    }

    $needle = $null
    if ($TitleSubstring) { $needle = $TitleSubstring.ToLowerInvariant() }

    foreach ($cand in $candidates) {
        if ($needle) {
            $n = ''
            try { $n = $cand.Current.Name } catch { $n = '' }
            if (-not ($n -and $n.ToLowerInvariant().Contains($needle))) {
                continue
            }
        }

        $akChild = Get-AccessKitChild -Element $cand
        if ($null -ne $akChild) {
            return [pscustomobject]@{ Window = $cand; AccessKitRoot = $akChild }
        }
    }

    return $null
}

function Find-RailWindowByTitle {
    param([string]$TitleSubstring)

    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $classProp = [System.Windows.Automation.AutomationElement]::ClassNameProperty
    $classCond = [System.Windows.Automation.PropertyCondition]::new($classProp, 'RAIL_WINDOW')

    $candidates = $null
    try {
        $candidates = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $classCond)
    } catch {
        return $null
    }

    $needle = ''
    if ($TitleSubstring) { $needle = $TitleSubstring.ToLowerInvariant() }

    foreach ($cand in $candidates) {
        $n = ''
        try { $n = $cand.Current.Name } catch { $n = '' }
        if ($n -and $needle -and $n.ToLowerInvariant().Contains($needle)) {
            return $cand
        }
    }

    return $null
}

# Native Descendants search filtered by the pattern's IsXPatternAvailable
# property, then a substring match on Name over that result set. Falls back
# to a bounded RawViewWalker walk if the native search itself fails.
function Find-ElementByNameAndPattern {
    param(
        [System.Windows.Automation.AutomationElement]$SubtreeRoot,
        [string]$NameSubstring,
        [string]$PatternLabel,
        $RequiredPattern
    )

    $needle = $NameSubstring.ToLowerInvariant()
    $availProp = Get-PatternAvailableProperty -Label $PatternLabel

    $descendantMatches = $null
    if ($null -ne $availProp) {
        try {
            $cond = [System.Windows.Automation.PropertyCondition]::new($availProp, $true)
            $descendantMatches = $SubtreeRoot.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
        } catch {
            $descendantMatches = $null
        }
    }

    if ($null -ne $descendantMatches) {
        foreach ($el in $descendantMatches) {
            $nm = ''
            try { $nm = $el.Current.Name } catch { $nm = '' }
            if ($nm -and $nm.ToLowerInvariant().Contains($needle)) {
                return $el
            }
        }
        return $null
    }

    return Find-ElementByNameAndPatternWalk -SubtreeRoot $SubtreeRoot -NameSubstring $NameSubstring -RequiredPattern $RequiredPattern
}

# Bounded fallback walk used only if the native Descendants search above
# throws. Caps depth at 12 and total visited elements at 500.
function Find-ElementByNameAndPatternWalk {
    param(
        [System.Windows.Automation.AutomationElement]$SubtreeRoot,
        [string]$NameSubstring,
        $RequiredPattern,
        [int]$MaxDepth = 12,
        [int]$MaxElements = 500
    )

    $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
    $needle = $NameSubstring.ToLowerInvariant()
    $visited = 0

    $queue = [System.Collections.Generic.Queue[psobject]]::new()
    $queue.Enqueue([pscustomobject]@{ El = $SubtreeRoot; Depth = 0 })

    while ($queue.Count -gt 0 -and $visited -lt $MaxElements) {
        $item = $queue.Dequeue()
        $el = $item.El
        $depth = $item.Depth
        $visited++

        $nm = ''
        try { $nm = $el.Current.Name } catch { $nm = '' }
        if ($nm -and $nm.ToLowerInvariant().Contains($needle)) {
            $supported = $null
            try { $supported = $el.GetSupportedPatterns() } catch { $supported = $null }
            if ($null -ne $supported -and ($supported -contains $RequiredPattern)) {
                return $el
            }
        }

        if ($depth -ge $MaxDepth) { continue }

        $child = $null
        try { $child = $walker.GetFirstChild($el) } catch { $child = $null }
        while ($null -ne $child -and $visited -lt $MaxElements) {
            $queue.Enqueue([pscustomobject]@{ El = $child; Depth = ($depth + 1) })
            $next = $null
            try { $next = $walker.GetNextSibling($child) } catch { $next = $null }
            $child = $next
        }
    }

    return $null
}

function Get-TopLevelWindow {
    param([System.Windows.Automation.AutomationElement]$Element)

    $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $current = $Element
    $result = $Element
    $hops = 0

    while ($null -ne $current -and $hops -lt 50) {
        $hops++
        $isRoot = $false
        try { $isRoot = $current.Equals($root) } catch { $isRoot = $false }
        if ($isRoot) { break }
        $result = $current
        $parent = $null
        try { $parent = $walker.GetParent($current) } catch { $parent = $null }
        $current = $parent
    }

    return $result
}

function Invoke-PatternAction {
    param(
        [string]$PatternLabel,
        [string]$NameSubstring,
        [scriptblock]$Action
    )

    if (-not $NameSubstring) {
        Write-Error ("{0} requires -Name <substring>" -f $PatternLabel)
        exit 1
    }

    $target = Find-TargetRailWindow -TitleSubstring $Window
    if ($null -eq $target) {
        Write-Error "No RAIL window with an AccessKit child was found. Is msrdc.exe running with the AccessKit DVC plugin loaded?"
        exit 1
    }
    Write-Host ("Target window: '{0}'" -f $target.Window.Current.Name)

    $reqPattern = Get-PatternIdentifier -Label $PatternLabel
    $element = Find-ElementByNameAndPattern -SubtreeRoot $target.Window -NameSubstring $NameSubstring -PatternLabel $PatternLabel -RequiredPattern $reqPattern
    if ($null -eq $element) {
        Write-Error ("No descendant with Name containing '{0}' supporting {1} was found." -f $NameSubstring, $PatternLabel)
        exit 1
    }

    Write-Host ("Found: {0}" -f (Format-ElementInfo -Element $element))

    $patternObj = $null
    try {
        $patternObj = $element.GetCurrentPattern($reqPattern)
    } catch {
        Write-Error ("Failed to get {0} pattern instance: {1}" -f $PatternLabel, $_.Exception.Message)
        exit 1
    }

    try {
        & $Action $patternObj $element
    } catch {
        Write-Error ("Action failed: {0}" -f $_.Exception.Message)
        exit 1
    }

    exit 0
}

# ---- Command validation -----------------------------------------------------

if (-not $Command) {
    Write-Error "Usage: uia.ps1 <list|tree|invoke|toggle|expand|collapse|select|setvalue|range|focus|activate> [-Window <substr>] [-Name <substr>] [-Value <double>] [-Seconds <n>]"
    exit 1
}

# ---- Dispatch ---------------------------------------------------------------

try {
    switch ($Command.ToLowerInvariant()) {

        'list' {
            $root = [System.Windows.Automation.AutomationElement]::RootElement
            $allChildren = $null
            try {
                $allChildren = $root.FindAll([System.Windows.Automation.TreeScope]::Children, [System.Windows.Automation.Condition]::TrueCondition)
            } catch {
                Write-Error ("Failed to enumerate the desktop's top-level windows: {0}" -f $_.Exception.Message)
                exit 1
            }

            $rows = [System.Collections.Generic.List[psobject]]::new()

            foreach ($child in $allChildren) {
                $cls = ''
                try { $cls = $child.Current.ClassName } catch { $cls = '' }

                $isRail = ($cls -eq 'RAIL_WINDOW')
                $akChild = Get-AccessKitChild -Element $child
                $hasAkChild = ($null -ne $akChild)
                $include = ($isRail -or $hasAkChild)

                if (-not $include) {
                    $found = Test-SubtreeHasFrameworkId -Element $child -FrameworkId 'AccessKit' -MaxDepth 3 -MaxElements 60
                    if ($null -ne $found) {
                        $include = $true
                        $hasAkChild = $true
                    }
                }

                if ($include) {
                    $name = ''
                    $fwid = ''
                    $hwnd = ''
                    $procId = ''
                    try { $name = $child.Current.Name } catch { }
                    try { $fwid = $child.Current.FrameworkId } catch { }
                    try { $hwnd = $child.Current.NativeWindowHandle } catch { }
                    try { $procId = $child.Current.ProcessId } catch { }

                    $rows.Add([pscustomobject]@{
                        Name              = $name
                        ClassName         = $cls
                        FrameworkId       = $fwid
                        NativeWindowHandle = $hwnd
                        ProcessId         = $procId
                        HasAccessKitChild = $hasAkChild
                    })
                }
            }

            if ($rows.Count -eq 0) {
                Write-Host "No RAIL windows and no AccessKit provider found on the desktop."
                Write-Host "Make sure msrdc.exe (WSLg RAIL) is running with a RAIL_WINDOW and the AccessKit DVC plugin is attached."
                exit 1
            }

            $rows | Format-Table -AutoSize | Out-String | Write-Host
            exit 0
        }

        'tree' {
            $target = Find-TargetRailWindow -TitleSubstring $Window
            if ($null -eq $target) {
                if ($Window) {
                    Write-Error ("No RAIL window matching title '{0}' with an AccessKit child was found." -f $Window)
                } else {
                    Write-Error "No RAIL window with an AccessKit child was found. Is msrdc.exe running with the AccessKit DVC plugin loaded?"
                }
                exit 1
            }

            Write-Host ("Window: {0}" -f (Format-ElementInfo -Element $target.Window))
            Write-Host ("AccessKit root: {0}" -f (Format-ElementInfo -Element $target.AccessKitRoot))
            Write-Host '---'

            $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
            $maxDepth = 12
            $maxElements = 500
            $printed = 0

            $stack = [System.Collections.Generic.Stack[psobject]]::new()
            $stack.Push([pscustomobject]@{ El = $target.Window; Depth = 0 })

            while ($stack.Count -gt 0 -and $printed -lt $maxElements) {
                $item = $stack.Pop()
                $el = $item.El
                $depth = $item.Depth

                $ct = '<unknown>'
                $nm = ''
                $aid = ''
                try { $ct = $el.Current.ControlType.ProgrammaticName } catch { }
                try { $nm = $el.Current.Name } catch { }
                try { $aid = $el.Current.AutomationId } catch { }
                $flags = Get-PatternFlags -Element $el

                $indent = '  ' * $depth
                Write-Host ("{0}{1} Name='{2}' AutomationId='{3}' Patterns=[{4}]" -f $indent, $ct, $nm, $aid, $flags)
                $printed++

                if ($depth -ge $maxDepth -or $printed -ge $maxElements) { continue }

                $children = [System.Collections.Generic.List[psobject]]::new()
                $child = $null
                try { $child = $walker.GetFirstChild($el) } catch { $child = $null }
                while ($null -ne $child) {
                    $children.Add($child)
                    $next = $null
                    try { $next = $walker.GetNextSibling($child) } catch { $next = $null }
                    $child = $next
                }
                for ($i = $children.Count - 1; $i -ge 0; $i--) {
                    $stack.Push([pscustomobject]@{ El = $children[$i]; Depth = ($depth + 1) })
                }
            }

            if ($printed -ge $maxElements) {
                Write-Host ("(stopped after {0} elements - cap reached)" -f $maxElements)
            }
            exit 0
        }

        'invoke' {
            Invoke-PatternAction -PatternLabel 'Invoke' -NameSubstring $Name -Action {
                param($pattern, $element)
                $pattern.Invoke()
                Write-Host 'Invoke succeeded.'
            }
        }

        'toggle' {
            Invoke-PatternAction -PatternLabel 'Toggle' -NameSubstring $Name -Action {
                param($pattern, $element)
                $before = $pattern.Current.ToggleState
                Write-Host ("ToggleState before: {0}" -f $before)
                $pattern.Toggle()
                Start-Sleep -Milliseconds 3500
                $after = '<unreadable>'
                try { $after = $pattern.Current.ToggleState } catch { }
                Write-Host ("ToggleState after:  {0}" -f $after)
            }
        }

        'expand' {
            Invoke-PatternAction -PatternLabel 'ExpandCollapse' -NameSubstring $Name -Action {
                param($pattern, $element)
                $pattern.Expand()
                Write-Host 'Expand succeeded.'
            }
        }

        'collapse' {
            Invoke-PatternAction -PatternLabel 'ExpandCollapse' -NameSubstring $Name -Action {
                param($pattern, $element)
                $pattern.Collapse()
                Write-Host 'Collapse succeeded.'
            }
        }

        'select' {
            Invoke-PatternAction -PatternLabel 'SelectionItem' -NameSubstring $Name -Action {
                param($pattern, $element)
                $pattern.Select()
                Write-Host 'Select succeeded.'
            }
        }

        'setvalue' {
            if (-not $PSBoundParameters.ContainsKey('Name') -or [string]::IsNullOrEmpty($Name)) {
                Write-Error "setvalue requires -Name <substring>"
                exit 1
            }
            if (-not $PSBoundParameters.ContainsKey('Value')) {
                Write-Error "setvalue requires -Value <double>"
                exit 1
            }

            $target = Find-TargetRailWindow -TitleSubstring $null
            if ($null -eq $target) {
                Write-Error "No RAIL window with an AccessKit child was found. Is msrdc.exe running with the AccessKit DVC plugin loaded?"
                exit 1
            }

            $reqPattern = Get-PatternIdentifier -Label 'RangeValue'
            $element = Find-ElementByNameAndPattern -SubtreeRoot $target.Window -NameSubstring $Name -PatternLabel 'RangeValue' -RequiredPattern $reqPattern
            if ($null -eq $element) {
                Write-Error ("No descendant with Name containing '{0}' supporting RangeValue was found." -f $Name)
                exit 1
            }

            Write-Host ("Found: {0}" -f (Format-ElementInfo -Element $element))

            try {
                $rvp = $element.GetCurrentPattern($reqPattern)
                Write-Host ("Before: Value={0} Minimum={1} Maximum={2}" -f $rvp.Current.Value, $rvp.Current.Minimum, $rvp.Current.Maximum)
                $rvp.SetValue([double]$Value)
                Start-Sleep -Milliseconds 3500
                $newValue = $rvp.Current.Value
                Write-Host ("After 500ms: Value={0}" -f $newValue)
            } catch {
                Write-Error ("SetValue failed: {0}" -f $_.Exception.Message)
                exit 1
            }
            exit 0
        }

        'range' {
            if (-not $PSBoundParameters.ContainsKey('Name') -or [string]::IsNullOrEmpty($Name)) {
                Write-Error "range requires -Name <substring>"
                exit 1
            }

            $target = Find-TargetRailWindow -TitleSubstring $null
            if ($null -eq $target) {
                Write-Error "No RAIL window with an AccessKit child was found. Is msrdc.exe running with the AccessKit DVC plugin loaded?"
                exit 1
            }

            $reqPattern = Get-PatternIdentifier -Label 'RangeValue'
            $element = Find-ElementByNameAndPattern -SubtreeRoot $target.Window -NameSubstring $Name -PatternLabel 'RangeValue' -RequiredPattern $reqPattern
            if ($null -eq $element) {
                Write-Error ("No descendant with Name containing '{0}' supporting RangeValue was found." -f $Name)
                exit 1
            }

            Write-Host ("Found: {0}" -f (Format-ElementInfo -Element $element))

            try {
                $rvp = $element.GetCurrentPattern($reqPattern)
                Write-Host ("Value={0} Minimum={1} Maximum={2} SmallChange={3}" -f $rvp.Current.Value, $rvp.Current.Minimum, $rvp.Current.Maximum, $rvp.Current.SmallChange)
            } catch {
                Write-Error ("Failed to read RangeValue pattern: {0}" -f $_.Exception.Message)
                exit 1
            }
            exit 0
        }

        'focus' {
            $seconds = $Seconds
            if ($seconds -le 0) { $seconds = 15 }

            $handler = [System.Windows.Automation.AutomationFocusChangedEventHandler]{
                param($sender, $e)
                try {
                    $el = [System.Windows.Automation.AutomationElement]::FocusedElement
                    $nm = ''
                    $ct = '<unknown>'
                    $fw = ''
                    try { $nm = $el.Current.Name } catch { }
                    try { $ct = $el.Current.ControlType.ProgrammaticName } catch { }
                    try { $fw = $el.Current.FrameworkId } catch { }

                    $topTitle = '<unknown>'
                    try {
                        $top = Get-TopLevelWindow -Element $el
                        $topTitle = $top.Current.Name
                    } catch { }

                    $fg = [UiaNative]::GetForegroundWindow()
                    Write-Host ("[{0}] Focus: {1} Name='{2}' FrameworkId='{3}' Window='{4}' Foreground=0x{5:X}" -f (Get-Date -Format 'HH:mm:ss.fff'), $ct, $nm, $fw, $topTitle, $fg.ToInt64())
                } catch {
                    Write-Host ("Focus handler error: {0}" -f $_.Exception.Message)
                }
            }

            try {
                [System.Windows.Automation.Automation]::AddAutomationFocusChangedEventHandler($handler)
                Write-Host ("Listening for focus changes for {0} second(s)... (Ctrl+C to stop early)" -f $seconds)
                $endTime = (Get-Date).AddSeconds($seconds)
                while ((Get-Date) -lt $endTime) {
                    Start-Sleep -Milliseconds 200
                }
            } catch {
                Write-Error ("Failed to subscribe to focus events: {0}" -f $_.Exception.Message)
                exit 1
            } finally {
                try {
                    [System.Windows.Automation.Automation]::RemoveAutomationFocusChangedEventHandler($handler)
                } catch { }
            }
            exit 0
        }

        'activate' {
            if (-not $Window) {
                Write-Error "activate requires -Window <title-substring>"
                exit 1
            }

            $win = Find-RailWindowByTitle -TitleSubstring $Window
            if ($null -eq $win) {
                Write-Error ("No RAIL_WINDOW matching title '{0}' was found." -f $Window)
                exit 1
            }

            Write-Host ("Target: {0}" -f (Format-ElementInfo -Element $win))

            $hwndInt = 0
            try { $hwndInt = $win.Current.NativeWindowHandle } catch { $hwndInt = 0 }
            if ($hwndInt -eq 0) {
                Write-Error "Target window has no NativeWindowHandle; cannot activate."
                exit 1
            }
            $hwnd = [IntPtr]$hwndInt

            $before = [UiaNative]::GetForegroundWindow()
            Write-Host ("Foreground before: 0x{0:X}" -f $before.ToInt64())

            $ok = $false
            try {
                $ok = [UiaNative]::SetForegroundWindow($hwnd)
            } catch {
                Write-Error ("SetForegroundWindow failed: {0}" -f $_.Exception.Message)
                exit 1
            }

            $after = [UiaNative]::GetForegroundWindow()
            Write-Host ("Foreground after:  0x{0:X}" -f $after.ToInt64())

            if (-not $ok) {
                Write-Error "SetForegroundWindow returned false."
                exit 1
            }
            exit 0
        }

        default {
            Write-Error ("Unknown command '{0}'. Valid commands: list, tree, invoke, toggle, expand, collapse, select, setvalue, range, focus, activate." -f $Command)
            exit 1
        }
    }
} catch {
    Write-Error ("Unexpected error: {0}" -f $_.Exception.Message)
    exit 1
}
