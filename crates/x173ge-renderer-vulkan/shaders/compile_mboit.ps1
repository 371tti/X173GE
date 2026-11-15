$ErrorActionPreference = 'Stop'
$glslc = (Get-Command glslc -ErrorAction Stop).Source
$spirvVal = (Get-Command spirv-val -ErrorAction Stop).Source
$shaderDir = $PSScriptRoot
$jobs = @(
    [pscustomobject]@{ Source = 'mboit_shadow.vert'; Output = 'mboit_shadow.vert.spv' },
    [pscustomobject]@{ Source = 'mboit_shadow.frag'; Output = 'mboit_shadow.frag.spv' }
)
$stagingDir = Join-Path $shaderDir ('.mboit-build-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stagingDir | Out-Null

try {
    foreach ($job in $jobs) {
        $output = Join-Path $stagingDir $job.Output
        & $glslc --target-env=vulkan1.0 -O -I $shaderDir -o $output (Join-Path $shaderDir $job.Source)
        if ($LASTEXITCODE -ne 0) { throw "glslc failed: $($job.Source)" }
        & $spirvVal --target-env vulkan1.0 $output
        if ($LASTEXITCODE -ne 0) { throw "spirv-val failed: $($job.Output)" }
    }

    foreach ($job in $jobs) {
        Move-Item -LiteralPath (Join-Path $stagingDir $job.Output) `
            -Destination (Join-Path $shaderDir $job.Output) -Force
    }
    Write-Output "Compiled and validated $($jobs.Count) MBOIT shadow shader modules."
}
finally {
    foreach ($job in $jobs) {
        $stagedOutput = Join-Path $stagingDir $job.Output
        if (Test-Path -LiteralPath $stagedOutput) {
            Remove-Item -LiteralPath $stagedOutput -Force
        }
    }
    if (Test-Path -LiteralPath $stagingDir) {
        Remove-Item -LiteralPath $stagingDir -Force
    }
}
