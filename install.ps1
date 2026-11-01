# Installs git-multibox on Windows, from PowerShell:
#
#   irm https://raw.githubusercontent.com/fpgames/multibox/main/install.ps1 | iex
#
# It downloads the latest release, checks it against SHA256SUMS, puts it in
# %LOCALAPPDATA%\Programs\multibox and adds that directory to your PATH.
# MULTIBOX_VERSION picks another release, such as v0.1.1. MULTIBOX_INSTALL_DIR
# picks another directory, which is then left to you to put on PATH.

# A script block keeps variables out of the caller's session, and errors are
# thrown rather than exited, so that `iex` does not close the window.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'fpgames/multibox'
    $version = $env:MULTIBOX_VERSION
    if (-not $version) {
        $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    }
    $dir = $env:MULTIBOX_INSTALL_DIR
    $managePath = -not $dir
    if (-not $dir) {
        $dir = Join-Path $env:LOCALAPPDATA 'Programs\multibox'
    }
    $name = "git-multibox-$version-x86_64-pc-windows-msvc"
    $base = "https://github.com/$repo/releases/download/$version"

    $temp = Join-Path ([IO.Path]::GetTempPath()) "multibox-install-$PID"
    New-Item -ItemType Directory -Force $temp | Out-Null
    try {
        try {
            Invoke-WebRequest "$base/$name.zip" -OutFile "$temp\$name.zip" -UseBasicParsing
            Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$temp\SHA256SUMS" -UseBasicParsing
        } catch {
            throw "Could not download $name.zip from $base ($($_.Exception.Message)). Is $version a release of $repo?"
        }
        $line = Get-Content "$temp\SHA256SUMS" | Where-Object { $_.EndsWith("  $name.zip") }
        $expected = "$line".Split(' ')[0]
        $actual = (Get-FileHash "$temp\$name.zip" -Algorithm SHA256).Hash.ToLower()
        if (-not $expected -or $expected -ne $actual) {
            throw "$name.zip does not match SHA256SUMS; nothing was installed"
        }
        Expand-Archive "$temp\$name.zip" $temp -Force
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item "$temp\$name\git-multibox.exe" $dir -Force
    } finally {
        Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
    }

    $exe = Join-Path $dir 'git-multibox.exe'
    Write-Host "Installed $(& $exe --version) in $dir"
    if ($managePath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @("$userPath".Split(';') | Where-Object { $_ })
        if ($entries -notcontains $dir) {
            [Environment]::SetEnvironmentVariable('Path', (($entries + $dir) -join ';'), 'User')
            Write-Host "Added $dir to your PATH for new terminals."
        }
        if ($env:Path.Split(';') -notcontains $dir) {
            $env:Path = "$env:Path;$dir"
        }
    } else {
        Write-Host "Put $dir on PATH so that git finds it as 'git multibox'."
    }
    $first = Get-Command git-multibox.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($first -and $first.Source -ne $exe) {
        Write-Warning "$($first.Source) comes first on PATH, so git will run that one instead."
    }
    Write-Host "Next, inside a repository: git multibox doctor"
}
