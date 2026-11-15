$ErrorActionPreference = 'Stop'
$glslc = (Get-Command glslc -ErrorAction Stop).Source
$spirvVal = (Get-Command spirv-val -ErrorAction Stop).Source
$shaderDir = $PSScriptRoot
$jobs = @(
    [pscustomobject]@{ Source = 'post.frag'; Stage = 'post.frag.spv'; Destination = 'post.frag.spv' },
    [pscustomobject]@{ Source = 'exposure_reduce.frag'; Stage = 'exposure_reduce.frag.spv'; Destination = 'exposure_reduce.frag.spv' },
    [pscustomobject]@{ Source = 'exposure_adapt.frag'; Stage = 'exposure_adapt.frag.spv'; Destination = 'exposure_adapt.frag.spv' },
    [pscustomobject]@{ Source = 'bloom/composite.frag'; Stage = 'bloom_composite.frag.spv'; Destination = 'bloom/composite.frag.spv' }
)
$stagingDir = Join-Path $shaderDir ('.tonemap-build-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stagingDir | Out-Null

try {
    foreach ($job in $jobs) {
        $output = Join-Path $stagingDir $job.Stage
        & $glslc --target-env=vulkan1.0 -O -I $shaderDir -o $output (Join-Path $shaderDir $job.Source)
        if ($LASTEXITCODE -ne 0) { throw "glslc failed: $($job.Source)" }
        & $spirvVal --target-env vulkan1.0 $output
        if ($LASTEXITCODE -ne 0) { throw "spirv-val failed: $($job.Stage)" }
    }

    foreach ($job in $jobs) {
        Move-Item -LiteralPath (Join-Path $stagingDir $job.Stage) `
            -Destination (Join-Path $shaderDir $job.Destination) -Force
    }
    Write-Output "Compiled and validated $($jobs.Count) tone mapping and exposure shader modules."
}
finally {
    foreach ($job in $jobs) {
        $stagedOutput = Join-Path $stagingDir $job.Stage
        if (Test-Path -LiteralPath $stagedOutput) {
            Remove-Item -LiteralPath $stagedOutput -Force
        }
    }
    if (Test-Path -LiteralPath $stagingDir) {
        Remove-Item -LiteralPath $stagingDir -Force
    }
}
