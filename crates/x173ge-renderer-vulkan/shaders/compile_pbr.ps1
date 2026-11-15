$ErrorActionPreference = 'Stop'
$glslc = (Get-Command glslc -ErrorAction Stop).Source
$spirvVal = (Get-Command spirv-val -ErrorAction Stop).Source
$shaderDir = $PSScriptRoot
$jobs = @(
    [pscustomobject]@{ Source = 'pbr.frag'; Output = 'pbr.frag.spv'; Defines = @() },
    [pscustomobject]@{ Source = 'pbr.frag'; Output = 'pbr_mboit.frag.spv'; Defines = @('X173GE_MBOIT') },
    [pscustomobject]@{ Source = 'pbr_ssr.frag'; Output = 'pbr_ssr.frag.spv'; Defines = @() },
    [pscustomobject]@{ Source = 'pbr_ssr.frag'; Output = 'pbr_ssr_mboit.frag.spv'; Defines = @('X173GE_MBOIT') },
    [pscustomobject]@{ Source = 'ssr.frag'; Output = 'ssr.frag.spv'; Defines = @() }
)
$stagingDir = Join-Path $shaderDir ('.pbr-build-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stagingDir | Out-Null

try {
    foreach ($job in $jobs) {
        $output = Join-Path $stagingDir $job.Output
        $compilerArgs = @('--target-env=vulkan1.0', '-O', '-I', $shaderDir)
        foreach ($define in $job.Defines) { $compilerArgs += "-D$define=1" }
        $compilerArgs += @('-o', $output, (Join-Path $shaderDir $job.Source))
        & $glslc @compilerArgs
        if ($LASTEXITCODE -ne 0) { throw "glslc failed: $($job.Source)" }
        & $spirvVal --target-env vulkan1.0 $output
        if ($LASTEXITCODE -ne 0) { throw "spirv-val failed: $($job.Output)" }
    }

    foreach ($job in $jobs) {
        Move-Item -LiteralPath (Join-Path $stagingDir $job.Output) `
            -Destination (Join-Path $shaderDir $job.Output) -Force
    }
    Write-Output "Compiled and validated $($jobs.Count) PBR and SSR fragment shader modules."
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
