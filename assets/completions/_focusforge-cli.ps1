
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'focusforge-cli' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'focusforge-cli'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'focusforge-cli' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Print project, TIFF or RAW metadata for a file, or for every frame in a directory')
            [CompletionResult]::new('algorithms', 'algorithms', [CompletionResultType]::ParameterValue, 'List every registered algorithm, or print one named algorithm''s full parameter schema')
            [CompletionResult]::new('stack', 'stack', [CompletionResultType]::ParameterValue, 'Fuse supported TIFF or RAW frames of a directory into one **project** per detected stack, through a registered stacking algorithm. A run prepares projects for review; it does not export images -- run `focusforge-cli export` for that')
            [CompletionResult]::new('export', 'export', [CompletionResultType]::ParameterValue, 'Turn a project into an image file: the image with `-o`, the Z-Map with `--zmap`, either or both in one invocation. At least one of the two is required. This is the only command that writes an image')
            [CompletionResult]::new('retouch', 'retouch', [CompletionResultType]::ParameterValue, 'Append an op list to a project as new states, one per op, each the previous one''s child: on the current head, or on `--from` as a branch. `export --checkpoint` writes any of them')
            [CompletionResult]::new('history', 'history', [CompletionResultType]::ParameterValue, 'List, delete or compact a project''s checkpoints. Every history operation is reachable through this command, headless')
            [CompletionResult]::new('align', 'align', [CompletionResultType]::ParameterValue, 'Compute and report the per-frame transforms for a stack: readable text by default, a `--json` document on request, and an optional per-frame warped-TIFF dump for a visual check')
            [CompletionResult]::new('review', 'review', [CompletionResultType]::ParameterValue, 'Report one record per result found under a directory of projects, or a batch run''s output directory: identity, outcome, sharpness ratio and Z-Map spread, with a failed stack a visible record rather than a missing file. Reads a run manifest when one is present, otherwise a non-recursive listing of `*.ffp` files; never groups frames itself and writes nothing')
            [CompletionResult]::new('cache', 'cache', [CompletionResultType]::ParameterValue, 'Remove every entry from the stage cache under `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any project')
            [CompletionResult]::new('diagnostics', 'diagnostics', [CompletionResultType]::ParameterValue, 'Package this installation''s logs, crash reports, system summary, graphics adapter record, effective configuration and version into one ZIP, listing every file before writing it. The home directory and the user name are redacted out of every text entry, image data is never included, and nothing is sent anywhere')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;info' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON document instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;algorithms' {
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON document instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;stack' {
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Directory to collect this run''s projects into. Omit to write each project beside its own frames')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Directory to collect this run''s projects into. Omit to write each project beside its own frames')
            [CompletionResult]::new('--algorithm', '--algorithm', [CompletionResultType]::ParameterName, 'Registered stacking algorithm id (`focusforge-cli algorithms` lists them). Required unless a `--preset` supplies `algo`')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins')
            [CompletionResult]::new('--groups', '--groups', [CompletionResultType]::ParameterName, 'Group the frames as this TOML file says instead of proposing a grouping: one `[[group]]` block per stack, each with a `name` and a `frames` array of paths relative to `<dir>`. Every frame found under `<dir>` must appear in exactly one group')
            [CompletionResult]::new('--write-groups', '--write-groups', [CompletionResultType]::ParameterName, 'Write the proposed grouping to this path, in exactly the format `--groups` reads, then carry on with the run. Edit it and feed it back with `--groups` to override the proposal')
            [CompletionResult]::new('--only', '--only', [CompletionResultType]::ParameterName, 'Run only the named result(s), repeatable: matches a group''s identity (what `review --json`''s `identity` field prints, and what this run''s own `--status` stream uses) or its plain name. Every unselected group is skipped -- named on stderr, not re-run -- and its project on disk is left untouched. Combine with `--set`/`--align-set` to cheaply re-run a subset after changing a fusion or alignment parameter')
            [CompletionResult]::new('--align-algorithm', '--align-algorithm', [CompletionResultType]::ParameterName, 'Alignment algorithm id. Optional: defaults to the sole registered alignment algorithm, refusing (and listing the choices) when more than one is registered')
            [CompletionResult]::new('--align-set', '--align-set', [CompletionResultType]::ParameterName, 'Set an alignment parameter: `--align-set key=value`, repeatable. A repeated key is last-wins')
            [CompletionResult]::new('--tile-size', '--tile-size', [CompletionResultType]::ParameterName, 'Override the scheduler''s outer tile size, in pixels. Omit to use the scheduler''s own default')
            [CompletionResult]::new('--max-host-memory', '--max-host-memory', [CompletionResultType]::ParameterName, 'A **ceiling only** on the adaptive host-memory budget: it can lower the budget the host-derived estimate resolved to, and can never raise it. Nobody has to pass this to get the machine''s own concurrency -- omit it entirely and the budget is derived from the host. Accepts suffixed sizes (`8GiB`, `512MiB`, `4096`)')
            [CompletionResult]::new('--max-vram', '--max-vram', [CompletionResultType]::ParameterName, 'A **ceiling only** on the adaptive device (VRAM) budget: it can lower the budget the startup probe settled on, and can never raise it. Omit it entirely to get the probed value. Accepts the same suffixed sizes as `--max-host-memory`. Only present when the `gpu` feature is compiled in')
            [CompletionResult]::new('--preset', '--preset', [CompletionResultType]::ParameterName, 'Read parameter values from a TOML preset: the `algo` and `align-algo` keys plus the `[stack]` and `[align]` value tables. A flag always beats the file -- `--set`/`--align-set` override the tables, `--algorithm`/`--align-algorithm` override the keys')
            [CompletionResult]::new('--write-preset', '--write-preset', [CompletionResultType]::ParameterName, 'Write the preset that reproduces this run to this path: the algorithm ids plus the values you actually chose. Values left to an `auto` closure stay live and are never frozen into the file')
            [CompletionResult]::new('--status', '--status', [CompletionResultType]::ParameterName, 'Append one NDJSON status record for this stack to this path: identity, outcome, outputs and the error, if any. Every terminal path writes exactly one record')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('--no-align', '--no-align', [CompletionResultType]::ParameterName, 'Skip alignment: frames are fused as read and the output keeps the full frame')
            [CompletionResult]::new('--no-crop', '--no-crop', [CompletionResultType]::ParameterName, 'Skip the auto-crop: the output keeps the full frame extent even when alignment is on')
            [CompletionResult]::new('--strict', '--strict', [CompletionResultType]::ParameterName, 'Refuse the run when an alignment fails. Meaningless with `--no-align`')
            [CompletionResult]::new('--resume', '--resume', [CompletionResultType]::ParameterName, 'Resume a previously cancelled run of this same stack, found by stack fingerprint under `.scratch/resume/` -- no argument. Refuses at exit 1 when no matching scratch state exists or it disagrees with this invocation''s inputs')
            [CompletionResult]::new('--no-progress', '--no-progress', [CompletionResultType]::ParameterName, 'Print no stage progress')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Resolve everything, print what would happen -- parameters with their provenance, tile geometry, output paths -- and exit 0 without computing or writing anything. Stops *before* alignment, so it reports no crop rect and no output extent')
            [CompletionResult]::new('--cpu', '--cpu', [CompletionResultType]::ParameterName, 'Force the CPU path even when a usable GPU adapter is available. Only in builds with GPU support')
            [CompletionResult]::new('--no-cache', '--no-cache', [CompletionResultType]::ParameterName, 'Skip the stage cache: recompute the alignment even when a valid entry exists, and store no new entry')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;export' {
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Write the image to this path')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Write the image to this path')
            [CompletionResult]::new('--zmap', '--zmap', [CompletionResultType]::ParameterName, 'Write the Z-Map to this path')
            [CompletionResult]::new('--checkpoint', '--checkpoint', [CompletionResultType]::ParameterName, 'A checkpoint id or unique id prefix. Defaults to the head. An unedited image export needs no source frames; an edited one re-reads and verifies the nearest fused ancestor''s. `--zmap` alone needs none')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'The image format to write: `tiff`, `png` or `jpeg`. Inferred from `-o`''s extension when omitted (`.tif`/`.tiff` -> tiff, `.png` -> png, `.jpg`/`.jpeg` -> jpeg, no extension -> tiff, the default); refused if it names a format other than a recognised extension''s')
            [CompletionResult]::new('--tiff-bits', '--tiff-bits', [CompletionResultType]::ParameterName, 'TIFF bit depth: `16` (default) or `8`. TIFF only')
            [CompletionResult]::new('--tiff-compression', '--tiff-compression', [CompletionResultType]::ParameterName, 'TIFF strip compression: `none` (default), `lzw` or `deflate`. TIFF only')
            [CompletionResult]::new('--png-bits', '--png-bits', [CompletionResultType]::ParameterName, 'PNG bit depth: `16` (default) or `8`. PNG only')
            [CompletionResult]::new('--png-compression', '--png-compression', [CompletionResultType]::ParameterName, 'PNG compression effort: `fast`, `default` (default) or `best`. PNG only')
            [CompletionResult]::new('--jpeg-quality', '--jpeg-quality', [CompletionResultType]::ParameterName, 'JPEG quality, 1-100, default 95. JPEG only')
            [CompletionResult]::new('--jpeg-subsampling', '--jpeg-subsampling', [CompletionResultType]::ParameterName, 'JPEG chroma subsampling: `420` (default) or `444`. JPEG only')
            [CompletionResult]::new('--exif', '--exif', [CompletionResultType]::ParameterName, 'Carry the alignment reference frame''s EXIF: `on` (default) or `off`')
            [CompletionResult]::new('--icc', '--icc', [CompletionResultType]::ParameterName, 'Embed an ICC profile: `on` (default) or `off`')
            [CompletionResult]::new('--xmp', '--xmp', [CompletionResultType]::ParameterName, 'Carry the source frame''s XMP packet verbatim: `on` or `off` (default)')
            [CompletionResult]::new('--iptc', '--iptc', [CompletionResultType]::ParameterName, 'Carry the source frame''s IPTC block verbatim: `on` or `off` (default). Never written to PNG')
            [CompletionResult]::new('--frame', '--frame', [CompletionResultType]::ParameterName, 'Write this source frame "as fused" -- warped by its recorded transform and offset by the applied crop, at the resolved checkpoint''s own composite width and height. Requires `--frame-output`')
            [CompletionResult]::new('--frame-output', '--frame-output', [CompletionResultType]::ParameterName, 'Where to write the frame `--frame` names')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;retouch' {
            [CompletionResult]::new('--ops', '--ops', [CompletionResultType]::ParameterName, 'The op list: a JSON array of ops in the persisted `Op` shape. A brush stroke blends frame `source_id` over the image along its path: fully within `radius * (1 - softness)`, fading linearly to nothing at `radius`. Every op is checked before anything is written; positions in error messages count from 0 in list order')
            [CompletionResult]::new('--from', '--from', [CompletionResultType]::ParameterName, 'The state to append under -- any unique prefix of its id. Defaults to the project''s current head')
            [CompletionResult]::new('--name', '--name', [CompletionResultType]::ParameterName, 'Name the last appended state')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;history' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List every visible checkpoint, newest first')
            [CompletionResult]::new('delete', 'delete', [CompletionResultType]::ParameterValue, 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint')
            [CompletionResult]::new('compact', 'compact', [CompletionResultType]::ParameterValue, 'Rewrite the project to its live checkpoints only and report the bytes reclaimed')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;history;list' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON document instead of human-readable text')
            [CompletionResult]::new('--accept-changed-sources', '--accept-changed-sources', [CompletionResultType]::ParameterName, 'Accept changed source frames, printing their warning instead of refusing')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;history;delete' {
            [CompletionResult]::new('--checkpoint', '--checkpoint', [CompletionResultType]::ParameterName, 'The checkpoint to delete -- any unique prefix of its id')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('--accept-changed-sources', '--accept-changed-sources', [CompletionResultType]::ParameterName, 'Accept changed source frames, printing their warning instead of refusing')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;history;compact' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;history;help' {
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List every visible checkpoint, newest first')
            [CompletionResult]::new('delete', 'delete', [CompletionResultType]::ParameterValue, 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint')
            [CompletionResult]::new('compact', 'compact', [CompletionResultType]::ParameterValue, 'Rewrite the project to its live checkpoints only and report the bytes reclaimed')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;history;help;list' {
            break
        }
        'focusforge-cli;history;help;delete' {
            break
        }
        'focusforge-cli;history;help;compact' {
            break
        }
        'focusforge-cli;history;help;help' {
            break
        }
        'focusforge-cli;align' {
            [CompletionResult]::new('--algorithm', '--algorithm', [CompletionResultType]::ParameterName, 'Registered alignment algorithm id. Optional: defaults to the sole registered alignment algorithm')
            [CompletionResult]::new('--write-warped', '--write-warped', [CompletionResultType]::ParameterName, 'Write one warped TIFF per frame into this directory, at source dimensions, uncropped')
            [CompletionResult]::new('--set', '--set', [CompletionResultType]::ParameterName, 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins')
            [CompletionResult]::new('--max-host-memory', '--max-host-memory', [CompletionResultType]::ParameterName, 'A **ceiling only** on the adaptive host-memory budget the alignment workers are sized from: it can lower the budget the host-derived estimate resolved to, and can never raise it. Omit it entirely and the budget is derived from the host. Accepts suffixed sizes (`8GiB`, `512MiB`, `4096`)')
            [CompletionResult]::new('--max-vram', '--max-vram', [CompletionResultType]::ParameterName, 'A **ceiling only** on the adaptive device (VRAM) budget, for the alignment''s GPU levels and the `--write-warped` fuse backend: it can lower the budget the startup probe settled on, and can never raise it. Accepts the same suffixed sizes as `--max-host-memory`. Only present when the `gpu` feature is compiled in')
            [CompletionResult]::new('--preset', '--preset', [CompletionResultType]::ParameterName, 'Read parameter values from a TOML preset. `align` uses only `align-algo` and the `[align]` table; a non-empty `[stack]` table or `algo` key is unused here and says so on stderr, so one preset file serves both subcommands')
            [CompletionResult]::new('--write-preset', '--write-preset', [CompletionResultType]::ParameterName, 'Write the preset that reproduces this run to this path: `align-algo` plus the `[align]` values you actually chose')
            [CompletionResult]::new('--status', '--status', [CompletionResultType]::ParameterName, 'Append one NDJSON status record for this run to this path')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('--strict', '--strict', [CompletionResultType]::ParameterName, 'Refuse the run when an alignment fails')
            [CompletionResult]::new('--no-progress', '--no-progress', [CompletionResultType]::ParameterName, 'Print no progress at all. `align` has no fuse-tile units to count: this suppresses only the one stage line `align` otherwise prints')
            [CompletionResult]::new('--cpu', '--cpu', [CompletionResultType]::ParameterName, 'Force the CPU path even when a usable GPU adapter is available: the alignment''s large levels and the `--write-warped` fuse backend both stay on the CPU. Only present when the `gpu` feature is compiled in')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;review' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit one wrapped JSON document instead of a human-readable table')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;cache' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('clear', 'clear', [CompletionResultType]::ParameterValue, 'Remove every entry from the stage cache')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;cache;clear' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit the JSON report instead of human-readable text')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;cache;help' {
            [CompletionResult]::new('clear', 'clear', [CompletionResultType]::ParameterValue, 'Remove every entry from the stage cache')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;cache;help;clear' {
            break
        }
        'focusforge-cli;cache;help;help' {
            break
        }
        'focusforge-cli;diagnostics' {
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Where to write the ZIP. Omit for a dated name in the current directory')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Where to write the ZIP. Omit for a dated name in the current directory')
            [CompletionResult]::new('--include-minidumps', '--include-minidumps', [CompletionResultType]::ParameterName, 'Include the minidump binaries. They cannot be redacted')
            [CompletionResult]::new('--force', '--force', [CompletionResultType]::ParameterName, 'Overwrite the output path when it already exists')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Increase log detail (repeatable)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Log errors only')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'focusforge-cli;help' {
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Print project, TIFF or RAW metadata for a file, or for every frame in a directory')
            [CompletionResult]::new('algorithms', 'algorithms', [CompletionResultType]::ParameterValue, 'List every registered algorithm, or print one named algorithm''s full parameter schema')
            [CompletionResult]::new('stack', 'stack', [CompletionResultType]::ParameterValue, 'Fuse supported TIFF or RAW frames of a directory into one **project** per detected stack, through a registered stacking algorithm. A run prepares projects for review; it does not export images -- run `focusforge-cli export` for that')
            [CompletionResult]::new('export', 'export', [CompletionResultType]::ParameterValue, 'Turn a project into an image file: the image with `-o`, the Z-Map with `--zmap`, either or both in one invocation. At least one of the two is required. This is the only command that writes an image')
            [CompletionResult]::new('retouch', 'retouch', [CompletionResultType]::ParameterValue, 'Append an op list to a project as new states, one per op, each the previous one''s child: on the current head, or on `--from` as a branch. `export --checkpoint` writes any of them')
            [CompletionResult]::new('history', 'history', [CompletionResultType]::ParameterValue, 'List, delete or compact a project''s checkpoints. Every history operation is reachable through this command, headless')
            [CompletionResult]::new('align', 'align', [CompletionResultType]::ParameterValue, 'Compute and report the per-frame transforms for a stack: readable text by default, a `--json` document on request, and an optional per-frame warped-TIFF dump for a visual check')
            [CompletionResult]::new('review', 'review', [CompletionResultType]::ParameterValue, 'Report one record per result found under a directory of projects, or a batch run''s output directory: identity, outcome, sharpness ratio and Z-Map spread, with a failed stack a visible record rather than a missing file. Reads a run manifest when one is present, otherwise a non-recursive listing of `*.ffp` files; never groups frames itself and writes nothing')
            [CompletionResult]::new('cache', 'cache', [CompletionResultType]::ParameterValue, 'Remove every entry from the stage cache under `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any project')
            [CompletionResult]::new('diagnostics', 'diagnostics', [CompletionResultType]::ParameterValue, 'Package this installation''s logs, crash reports, system summary, graphics adapter record, effective configuration and version into one ZIP, listing every file before writing it. The home directory and the user name are redacted out of every text entry, image data is never included, and nothing is sent anywhere')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'focusforge-cli;help;info' {
            break
        }
        'focusforge-cli;help;algorithms' {
            break
        }
        'focusforge-cli;help;stack' {
            break
        }
        'focusforge-cli;help;export' {
            break
        }
        'focusforge-cli;help;retouch' {
            break
        }
        'focusforge-cli;help;history' {
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List every visible checkpoint, newest first')
            [CompletionResult]::new('delete', 'delete', [CompletionResultType]::ParameterValue, 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint')
            [CompletionResult]::new('compact', 'compact', [CompletionResultType]::ParameterValue, 'Rewrite the project to its live checkpoints only and report the bytes reclaimed')
            break
        }
        'focusforge-cli;help;history;list' {
            break
        }
        'focusforge-cli;help;history;delete' {
            break
        }
        'focusforge-cli;help;history;compact' {
            break
        }
        'focusforge-cli;help;align' {
            break
        }
        'focusforge-cli;help;review' {
            break
        }
        'focusforge-cli;help;cache' {
            [CompletionResult]::new('clear', 'clear', [CompletionResultType]::ParameterValue, 'Remove every entry from the stage cache')
            break
        }
        'focusforge-cli;help;cache;clear' {
            break
        }
        'focusforge-cli;help;diagnostics' {
            break
        }
        'focusforge-cli;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
