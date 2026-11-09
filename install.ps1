# Kori one-command installer and updater for Windows. Requires Docker Desktop and git.
#   irm https://raw.githubusercontent.com/darkzmaj/Kori-Discord-Bot/main/install.ps1 | iex
# Re-run it at any time to update. Set $env:KORI_DIR to choose the install folder.
$ErrorActionPreference = 'Stop'
$RepoUrl = 'https://github.com/darkzmaj/Kori-Discord-Bot.git'
$Dir = if ($env:KORI_DIR) { $env:KORI_DIR } else { Join-Path $HOME 'kori' }
$Permissions = 139855252560

function Say($m) { Write-Host "==> $m" -ForegroundColor Green }
function Die($m) { Write-Host "error: $m" -ForegroundColor Red; exit 1 }
function Ask($prompt, $default, $pattern, $hint) {
    while ($true) {
        $a = Read-Host "$prompt [$default]"
        if (-not $a) { $a = $default }
        if ($a -match $pattern) { return $a }
        Write-Host "  $hint" -ForegroundColor Yellow
    }
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) { Die 'Docker Desktop is required: https://docs.docker.com/desktop/' }
docker compose version *> $null; if ($LASTEXITCODE -ne 0) { Die "Docker Compose v2 is required (the 'docker compose' command)." }

if ((Test-Path docker-compose.yml) -and (Test-Path .env.example) -and (Select-String -Quiet kori docker-compose.yml)) {
    $Dir = (Get-Location).Path
} elseif (Test-Path (Join-Path $Dir '.git')) {
    Say "Updating $Dir"; git -C $Dir pull --ff-only; if ($LASTEXITCODE -ne 0) { Die 'git pull failed.' }
} else {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { Die 'git is required: https://git-scm.com/download/win' }
    Say "Downloading Kori into $Dir"; git clone --depth 1 $RepoUrl $Dir; if ($LASTEXITCODE -ne 0) { Die 'git clone failed.' }
}
Set-Location $Dir

$appId = ''
if (-not (Test-Path .env)) {
    Say 'First-time setup. Press Enter to keep the [default].'
    Write-Host '  Token: https://discord.com/developers/applications -> your app -> Bot -> Reset Token'
    Write-Host "  Also turn on 'Message Content Intent' on that page (needed for ticket transcripts)."
    Write-Host '  Log channels are set in Discord afterwards: run /ticket in your server.'
    while ($true) {
        $secure = Read-Host 'Discord bot token' -AsSecureString
        $token = [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
        if ($token -match '^[A-Za-z0-9._-]{50,}$') { break }
        Write-Host '  That does not look like a bot token.' -ForegroundColor Yellow
    }
    $appId = Ask 'Application ID (General Information page, optional)' '' '^([0-9]{17,20})?$' 'Use the numeric ID, or leave it empty.'

    $values = @{ DISCORD_TOKEN = $token; APPLICATION_ID = $appId }
    $lines = Get-Content .env.example | ForEach-Object {
        $key = ($_ -split '=', 2)[0]
        if ($values.ContainsKey($key)) { "$key=$($values[$key])" } else { $_ }
    }
    [IO.File]::WriteAllLines((Join-Path $Dir '.env'), $lines)  # UTF-8 without BOM, which docker compose expects
    Remove-Variable token, secure
    # Restrict .env to the current user.
    icacls .env /inheritance:r /grant:r "$($env:USERNAME):F" *> $null
    Say "Saved settings to $Dir\.env (edit it any time, then re-run this script)."
} else {
    $line = Select-String -Path .env -Pattern '^APPLICATION_ID=(.*)$' | Select-Object -First 1
    if ($line) { $appId = $line.Matches[0].Groups[1].Value.Trim() }
}

Say 'Fetching the Kori image'
docker compose pull kori 2> $null
if ($LASTEXITCODE -ne 0) {
    Say 'No prebuilt image for this platform; building locally (the first build takes several minutes).'
    docker compose build kori; if ($LASTEXITCODE -ne 0) { Die 'Build failed.' }
}
docker compose up -d --remove-orphans; if ($LASTEXITCODE -ne 0) { Die 'Could not start Kori.' }

Say 'Kori is running.'
Write-Host "  Logs:    cd `"$Dir`"; docker compose logs -f"
Write-Host '  Update:  re-run this installer (or: docker compose pull; docker compose up -d)'
Write-Host "  Stop:    cd `"$Dir`"; docker compose down"
if ($appId) { Write-Host "  Invite:  https://discord.com/oauth2/authorize?client_id=$appId&scope=bot+applications.commands&permissions=$Permissions" }
