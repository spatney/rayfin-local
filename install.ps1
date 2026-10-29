# Installs rayfin-local from its GitHub releases, on Windows:
#
#   irm https://raw.githubusercontent.com/spatney/rayfin-local/main/install.ps1 | iex
#
# Environment variables:
#   RAYFIN_LOCAL_VERSION         the version to install, such as 0.1.0 (default: the latest release)
#   RAYFIN_LOCAL_INSTALL_DIR     the folder for rayfin-local.exe
#                                (default: %LOCALAPPDATA%\Programs\rayfin-local)
#   RAYFIN_LOCAL_REPO            the GitHub repository to install from (default: spatney/rayfin-local)
#   RAYFIN_LOCAL_NO_MODIFY_PATH  set to 1 to leave your user PATH alone
#
# The download is checked against the release's SHA256SUMS.txt. Run the script again to update.
# Works in Windows PowerShell 5.1 and PowerShell 7.

& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell's progress bar slows downloads down a lot.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2, which GitHub requires, by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = if ($env:RAYFIN_LOCAL_REPO) { $env:RAYFIN_LOCAL_REPO } else { 'spatney/rayfin-local' }
    $installDir = if ($env:RAYFIN_LOCAL_INSTALL_DIR) {
        $env:RAYFIN_LOCAL_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'Programs\rayfin-local'
    }
    $fromSource = "build it from source instead: cargo install --locked --git https://github.com/$repo rayfin-local"

    function Get-LatestVersion {
        # /releases/latest redirects to /releases/tag/v<version>; reading the redirect avoids the
        # API's rate limit.
        try {
            $request = [System.Net.WebRequest]::Create("https://github.com/$repo/releases/latest")
            $request.Method = 'HEAD'
            $request.AllowAutoRedirect = $false
            $response = $request.GetResponse()
            try { $location = $response.Headers['Location'] } finally { $response.Close() }
            if ($location -match '/releases/tag/v([^/?#]+)$') { return $Matches[1] }
        } catch {
            Write-Verbose "Reading the latest release's redirect failed: $_"
        }
        try {
            $release = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$repo/releases/latest"
            return ([string]$release.tag_name).TrimStart('v')
        } catch {
            throw "Could not find the latest release of $repo (https://github.com/$repo/releases): $($_.Exception.Message)"
        }
    }

    function Add-ToUserPath([string]$dir) {
        # Edit the registry value directly: [Environment]::SetEnvironmentVariable would expand the
        # %VARIABLES% in the other entries and store the value as a plain string.
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $entries = @($current -split ';' | Where-Object { $_ })
            $wanted = $dir.TrimEnd('\')
            foreach ($entry in $entries) {
                if ([Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\') -ieq $wanted) { return $false }
            }
            $key.SetValue('Path', (($entries + $dir) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
        } finally {
            $key.Close()
        }
        # Setting a user variable broadcasts WM_SETTINGCHANGE, so new terminals see the new PATH.
        [Environment]::SetEnvironmentVariable('RAYFIN_LOCAL_INSTALLER', '1', 'User')
        [Environment]::SetEnvironmentVariable('RAYFIN_LOCAL_INSTALLER', $null, 'User')
        return $true
    }

    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host 'There is no ARM64 build for Windows yet; installing the x64 build, which Windows runs under emulation.' }
        default { throw "There is no prebuilt rayfin-local for $arch processors; $fromSource" }
    }
    $target = 'x86_64-pc-windows-msvc'

    $version = if ($env:RAYFIN_LOCAL_VERSION) { $env:RAYFIN_LOCAL_VERSION } else { Get-LatestVersion }
    $version = $version.Trim().TrimStart('v')
    $archive = "rayfin-local-$version-$target.zip"
    $base = "https://github.com/$repo/releases/download/v$version"

    $temp = Join-Path ([IO.Path]::GetTempPath()) ('rayfin-local-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        Write-Host "Downloading rayfin-local $version for $target..."
        $archivePath = Join-Path $temp $archive
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile $archivePath
        } catch {
            throw "Could not download $base/$archive ($($_.Exception.Message)); check that version $version exists at https://github.com/$repo/releases"
        }
        $sumsPath = Join-Path $temp 'SHA256SUMS.txt'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.txt" -OutFile $sumsPath
        } catch {
            throw "Could not download $base/SHA256SUMS.txt ($($_.Exception.Message))"
        }

        $expected = $null
        foreach ($line in Get-Content $sumsPath) {
            $fields = $line.Trim() -split '\s+', 2
            if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -eq $archive) {
                $expected = $fields[0]
                break
            }
        }
        if (-not $expected) { throw "SHA256SUMS.txt has no checksum for $archive" }
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash
        if ($actual -ne $expected) {
            throw "The checksum of $archive doesn't match SHA256SUMS.txt (expected $expected, got $actual)"
        }

        Expand-Archive -Path $archivePath -DestinationPath $temp -Force
        $exe = Join-Path $temp "rayfin-local-$version-$target\rayfin-local.exe"
        if (-not (Test-Path -LiteralPath $exe)) { throw "$archive has no rayfin-local.exe" }

        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        $dest = Join-Path $installDir 'rayfin-local.exe'
        $new = Join-Path $installDir 'rayfin-local.new.exe'
        # Leftovers of earlier updates, if the old binary was running then.
        Get-ChildItem -LiteralPath $installDir -Filter 'rayfin-local.exe.*.old' -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Copy-Item -LiteralPath $exe -Destination $new -Force
        # Run the new binary before it replaces anything, so a system that can't run it keeps what
        # it has. It runs from the install folder because policies may forbid programs in %TEMP%.
        $blocked = $false
        try {
            $installed = & $new --version
            $code = $LASTEXITCODE
        } catch {
            $cause = $_.Exception.InnerException
            if ($cause -is [System.ComponentModel.Win32Exception] -and $cause.NativeErrorCode -in 5, 1260) {
                # Windows refused to start it (access denied, or blocked by a policy): on managed
                # devices, Microsoft Defender's attack surface reduction rules can refuse new,
                # unsigned downloads, depending on the program that starts them. Install anyway.
                $blocked = $true
                $installed = "rayfin-local $version"
                $code = 0
            } else {
                Remove-Item -LiteralPath $new -Force -ErrorAction SilentlyContinue
                throw "The downloaded rayfin-local.exe doesn't run on this system: $($_.Exception.Message)"
            }
        }
        if ($code -ne 0) {
            Remove-Item -LiteralPath $new -Force -ErrorAction SilentlyContinue
            throw "The downloaded rayfin-local.exe doesn't run on this system (exit code $code)"
        }
        # A running rayfin-local.exe can't be overwritten, but it can be renamed.
        $old = $null
        if (Test-Path -LiteralPath $dest) {
            $old = "$dest.$([Guid]::NewGuid().ToString('N')).old"
            Move-Item -LiteralPath $dest -Destination $old
        }
        try {
            Move-Item -LiteralPath $new -Destination $dest
        } catch {
            if ($old) { Move-Item -LiteralPath $old -Destination $dest -Force }
            throw
        }
        if ($old) { Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue }
    } finally {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    }

    Write-Host "Installed $installed to $dest"
    if ($blocked) {
        Write-Warning ("Windows refused to start rayfin-local.exe from this shell (Access is denied). On managed " +
            "devices, Microsoft Defender can block new, unsigned programs downloaded from the internet, depending " +
            "on the program that starts them. If running rayfin-local is blocked too, run it from PowerShell 7 " +
            "(pwsh), or build it on this machine instead: $($fromSource -replace '^build it from source instead: ', '')")
    }

    $onPath = @($env:Path -split ';' | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') }).Count -gt 0
    if ($env:RAYFIN_LOCAL_NO_MODIFY_PATH -eq '1') {
        if (-not $onPath) { Write-Host "$installDir is not on your PATH; add it to run rayfin-local by name." }
    } else {
        if (Add-ToUserPath $installDir) { Write-Host "Added $installDir to your user PATH; open a new terminal to use it everywhere." }
        if (-not $onPath) { $env:Path = "$installDir;$env:Path" }
    }
    Write-Host ''
    Write-Host 'Next, in a Rayfin project: npm install, then rayfin-local serve'
}
