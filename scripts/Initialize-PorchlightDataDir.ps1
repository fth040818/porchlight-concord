[CmdletBinding(SupportsShouldProcess, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory)]
    [string] $Path,

    [switch] $AllowNonEmpty
)

$ErrorActionPreference = 'Stop'
$resolved = [System.IO.Path]::TrimEndingDirectorySeparator(
    [System.IO.Path]::GetFullPath($Path)
)
$separator = [System.IO.Path]::DirectorySeparatorChar

function Test-SameOrAncestor {
    param(
        [Parameter(Mandatory)] [string] $Candidate,
        [Parameter(Mandatory)] [string] $Child
    )

    $candidatePath = [System.IO.Path]::TrimEndingDirectorySeparator(
        [System.IO.Path]::GetFullPath($Candidate)
    )
    $childPath = [System.IO.Path]::TrimEndingDirectorySeparator(
        [System.IO.Path]::GetFullPath($Child)
    )
    return $childPath.Equals($candidatePath, [System.StringComparison]::OrdinalIgnoreCase) -or
        $childPath.StartsWith($candidatePath + $separator, [System.StringComparison]::OrdinalIgnoreCase)
}

$repositoryRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$currentLocation = [System.IO.Path]::GetFullPath((Get-Location).ProviderPath)
$userProfile = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::UserProfile)
$windowsDirectory = Split-Path -Parent ([System.Environment]::SystemDirectory)
$volumeRoot = [System.IO.Path]::GetPathRoot($resolved)

foreach ($protectedPath in @($volumeRoot, $repositoryRoot, $currentLocation, $userProfile, $windowsDirectory)) {
    if ($protectedPath -and (Test-SameOrAncestor -Candidate $resolved -Child $protectedPath)) {
        throw "Refusing to replace ACLs on a broad or protected path: $resolved"
    }
}

if ($windowsDirectory -and (Test-SameOrAncestor -Candidate $windowsDirectory -Child $resolved)) {
    throw "Refusing to replace ACLs inside the Windows directory: $resolved"
}

$ancestor = $resolved
while ($ancestor) {
    if (Test-Path -LiteralPath $ancestor) {
        $ancestorItem = Get-Item -LiteralPath $ancestor -Force
        if (($ancestorItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing a target with a reparse point in its path: $ancestor"
        }
    }
    $parent = Split-Path -Parent $ancestor
    if (-not $parent -or $parent.Equals($ancestor, [System.StringComparison]::OrdinalIgnoreCase)) {
        break
    }
    $ancestor = $parent
}

if (Test-Path -LiteralPath $resolved) {
    $item = Get-Item -LiteralPath $resolved -Force
    if (-not $item.PSIsContainer) {
        throw "Target exists but is not a directory: $resolved"
    }
    if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing to replace ACLs on a reparse point: $resolved"
    }
    $hasContent = $null -ne (Get-ChildItem -LiteralPath $resolved -Force | Select-Object -First 1)
    if ($hasContent -and -not $AllowNonEmpty) {
        throw "Target is not empty. Inspect it, then rerun with -AllowNonEmpty only if it is a dedicated Porchlight data directory: $resolved"
    }
}

if ($PSCmdlet.ShouldProcess($resolved, 'Create directory and replace inherited ACLs')) {
    [System.IO.Directory]::CreateDirectory($resolved) | Out-Null

    $security = [System.Security.AccessControl.DirectorySecurity]::new()
    $security.SetAccessRuleProtection($true, $false)

    $currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $system = [System.Security.Principal.SecurityIdentifier]::new(
        [System.Security.Principal.WellKnownSidType]::LocalSystemSid,
        $null
    )
    $administrators = [System.Security.Principal.SecurityIdentifier]::new(
        [System.Security.Principal.WellKnownSidType]::BuiltinAdministratorsSid,
        $null
    )
    $inheritance = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    $propagation = [System.Security.AccessControl.PropagationFlags]::None
    $rights = [System.Security.AccessControl.FileSystemRights]::FullControl

    foreach ($identity in @($currentUser, $system, $administrators)) {
        $rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
            $identity,
            $rights,
            $inheritance,
            $propagation,
            [System.Security.AccessControl.AccessControlType]::Allow
        )
        $security.AddAccessRule($rule)
    }

    Set-Acl -LiteralPath $resolved -AclObject $security
    Write-Output "Protected Porchlight data directory: $resolved"
}
