# Interactive protocol smoke test: sends one request, waits for its response,
# then the next. Prints the frames it observed plus a summary.
$ErrorActionPreference = 'Stop'
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path '.\target\debug\destro-harness.exe').Path
$psi.Arguments = "--cwd `"$PWD`" --model mock-1"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$p = [System.Diagnostics.Process]::Start($psi)

$script:seen = New-Object System.Collections.ArrayList
function Send-Json($obj) {
    $p.StandardInput.WriteLine(($obj | ConvertTo-Json -Compress -Depth 12))
    $p.StandardInput.Flush()
}
function Await-Id($id) {
    while ($true) {
        $line = $p.StandardOutput.ReadLine()
        if ($null -eq $line) { throw "stdout closed while awaiting id $id" }
        [void]$script:seen.Add($line)
        $j = $line | ConvertFrom-Json
        if ($j.id -eq $id) { return $j }
    }
}

Send-Json @{ jsonrpc='2.0'; id=0; method='initialize'; params=@{ cwd=$PWD.Path; provider='deepseek-official'; model='mock-1'; approvalRequests=$true } }
$init = Await-Id 0

Send-Json @{ jsonrpc='2.0'; id=1; method='session/query'; params=@{ sessionId='s1'; contentBlocks=@(@{ type='text'; text='hello harness' }) } }
$query = Await-Id 1

Send-Json @{ jsonrpc='2.0'; id=2; method='session/wait'; params=@{ sessionId='s1' } }
$wait = Await-Id 2

Send-Json @{ jsonrpc='2.0'; id=3; method='agent/list'; params=@{} }
$list = Await-Id 3

$agent = $list.result.agents | Select-Object -First 1
Send-Json @{ jsonrpc='2.0'; id=4; method='agent/spawn'; params=@{ parentId=$agent.agentId; title='researcher'; prompt='investigate the tools crate' } }
$spawn = Await-Id 4

Send-Json @{ jsonrpc='2.0'; id=5; method='agent/wait'; params=@{ agentId=$spawn.result.agentId } }
$child = Await-Id 5

Send-Json @{ jsonrpc='2.0'; id=6; method='agent/list'; params=@{} }
$list2 = Await-Id 6

Send-Json @{ jsonrpc='2.0'; id=7; method='agent/state'; params=@{ agentId=$spawn.result.agentId } }
$state = Await-Id 7

$p.StandardInput.Close()
$p.WaitForExit(15000) | Out-Null
$stderr = $p.StandardError.ReadToEnd()

"serverInfo      : $($init.result.serverInfo.name) $($init.result.serverInfo.version)"
"query           : kind=$($query.result.kind) messageId=$($query.result.messageId)"
"root agent      : $($agent.agentId) status=$($agent.status) turn=$($agent.turn)"
"  last message  : $($agent.lastMessage)"
"spawn           : $($spawn.result.agentId) parent=$($agent.agentId)"
"child wait      : status=$($child.result.status) turn=$($child.result.turn)"
"  last message  : $($child.result.lastMessage)"
"agent list      : $($list2.result.agents.Count) agents"
foreach ($a in $list2.result.agents) { "  - $($a.agentId) $($a.status) parent=$($a.parentId) '$($a.title)'" }
"agent state     : $($state.result.agentId) status=$($state.result.status) transcript=$($state.result.transcriptLen)"
$events = $script:seen | Where-Object { $_ -match '"method":"(session.event|session.status|subagent)' }
"notifications   : $($events.Count)"
$eventTypes = $events | ForEach-Object { ($_ | ConvertFrom-Json) } | ForEach-Object { if ($_.params.event) { $_.params.event.type } else { $_.method } } | Group-Object | ForEach-Object { "$($_.Name)=$($_.Count)" }
"event types     : $($eventTypes -join ', ')"
"exit code       : $($p.ExitCode)"
if ($stderr) { "stderr          : $stderr" }
