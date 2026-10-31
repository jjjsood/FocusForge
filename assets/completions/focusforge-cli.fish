# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_focusforge_cli_global_optspecs
    string join \n v q/quiet h/help V/version
end

function __fish_focusforge_cli_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_focusforge_cli_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_focusforge_cli_using_subcommand
    set -l cmd (__fish_focusforge_cli_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -s V -l version -d 'Print version'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "info" -d 'Print project, TIFF or RAW metadata for a file, or for every frame in a directory'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "algorithms" -d 'List every registered algorithm, or print one named algorithm\'s full parameter schema'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "stack" -d 'Fuse supported TIFF or RAW frames of a directory into one **project** per detected stack, through a registered stacking algorithm. A run prepares projects for review; it does not export images -- run `focusforge-cli export` for that'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "export" -d 'Turn a project into an image file: the image with `-o`, the Z-Map with `--zmap`, either or both in one invocation. At least one of the two is required. This is the only command that writes an image'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "retouch" -d 'Append an op list to a project as new states, one per op, each the previous one\'s child: on the current head, or on `--from` as a branch. `export --checkpoint` writes any of them'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "history" -d 'List, delete or compact a project\'s checkpoints. Every history operation is reachable through this command, headless'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "align" -d 'Compute and report the per-frame transforms for a stack: readable text by default, a `--json` document on request, and an optional per-frame warped-TIFF dump for a visual check'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "review" -d 'Report one record per result found under a directory of projects, or a batch run\'s output directory: identity, outcome, sharpness ratio and Z-Map spread, with a failed stack a visible record rather than a missing file. Reads a run manifest when one is present, otherwise a non-recursive listing of `*.ffp` files; never groups frames itself and writes nothing'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "cache" -d 'Remove every entry from the stage cache under `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any project'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "diagnostics" -d 'Package this installation\'s logs, crash reports, system summary, graphics adapter record, effective configuration and version into one ZIP, listing every file before writing it. The home directory and the user name are redacted out of every text entry, image data is never included, and nothing is sent anywhere'
complete -c focusforge-cli -n "__fish_focusforge_cli_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand info" -l json -d 'Emit the JSON document instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand info" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand info" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand info" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand algorithms" -l set -d 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand algorithms" -l json -d 'Emit the JSON document instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand algorithms" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand algorithms" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand algorithms" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -s o -l output -d 'Directory to collect this run\'s projects into. Omit to write each project beside its own frames' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l algorithm -d 'Registered stacking algorithm id (`focusforge-cli algorithms` lists them). Required unless a `--preset` supplies `algo`' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l set -d 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l groups -d 'Group the frames as this TOML file says instead of proposing a grouping: one `[[group]]` block per stack, each with a `name` and a `frames` array of paths relative to `<dir>`. Every frame found under `<dir>` must appear in exactly one group' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l write-groups -d 'Write the proposed grouping to this path, in exactly the format `--groups` reads, then carry on with the run. Edit it and feed it back with `--groups` to override the proposal' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l only -d 'Run only the named result(s), repeatable: matches a group\'s identity (what `review --json`\'s `identity` field prints, and what this run\'s own `--status` stream uses) or its plain name. Every unselected group is skipped -- named on stderr, not re-run -- and its project on disk is left untouched. Combine with `--set`/`--align-set` to cheaply re-run a subset after changing a fusion or alignment parameter' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l align-algorithm -d 'Alignment algorithm id. Optional: defaults to the sole registered alignment algorithm, refusing (and listing the choices) when more than one is registered' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l align-set -d 'Set an alignment parameter: `--align-set key=value`, repeatable. A repeated key is last-wins' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l tile-size -d 'Override the scheduler\'s outer tile size, in pixels. Omit to use the scheduler\'s own default' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l max-host-memory -d 'A **ceiling only** on the adaptive host-memory budget: it can lower the budget the host-derived estimate resolved to, and can never raise it. Nobody has to pass this to get the machine\'s own concurrency -- omit it entirely and the budget is derived from the host. Accepts suffixed sizes (`8GiB`, `512MiB`, `4096`)' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l max-vram -d 'A **ceiling only** on the adaptive device (VRAM) budget: it can lower the budget the startup probe settled on, and can never raise it. Omit it entirely to get the probed value. Accepts the same suffixed sizes as `--max-host-memory`. Only present when the `gpu` feature is compiled in' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l preset -d 'Read parameter values from a TOML preset: the `algo` and `align-algo` keys plus the `[stack]` and `[align]` value tables. A flag always beats the file -- `--set`/`--align-set` override the tables, `--algorithm`/`--align-algorithm` override the keys' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l write-preset -d 'Write the preset that reproduces this run to this path: the algorithm ids plus the values you actually chose. Values left to an `auto` closure stay live and are never frozen into the file' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l status -d 'Append one NDJSON status record for this stack to this path: identity, outcome, outputs and the error, if any. Every terminal path writes exactly one record' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l no-align -d 'Skip alignment: frames are fused as read and the output keeps the full frame'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l no-crop -d 'Skip the auto-crop: the output keeps the full frame extent even when alignment is on'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l strict -d 'Refuse the run when an alignment fails. Meaningless with `--no-align`'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l resume -d 'Resume a previously cancelled run of this same stack, found by stack fingerprint under `.scratch/resume/` -- no argument. Refuses at exit 1 when no matching scratch state exists or it disagrees with this invocation\'s inputs'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l no-progress -d 'Print no stage progress'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l dry-run -d 'Resolve everything, print what would happen -- parameters with their provenance, tile geometry, output paths -- and exit 0 without computing or writing anything. Stops *before* alignment, so it reports no crop rect and no output extent'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l cpu -d 'Force the CPU path even when a usable GPU adapter is available. Only in builds with GPU support'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -l no-cache -d 'Skip the stage cache: recompute the alignment even when a valid entry exists, and store no new entry'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand stack" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -s o -l output -d 'Write the image to this path' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l zmap -d 'Write the Z-Map to this path' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l checkpoint -d 'A checkpoint id or unique id prefix. Defaults to the head. An unedited image export needs no source frames; an edited one re-reads and verifies the nearest fused ancestor\'s. `--zmap` alone needs none' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l format -d 'The image format to write: `tiff`, `png`, `jpeg` or `dng` (a linear DNG of a RAW project). Inferred from `-o`\'s extension when omitted (`.tif`/`.tiff` -> tiff, `.png` -> png, `.jpg`/`.jpeg` -> jpeg, `.dng` -> dng, no extension -> tiff, the default); refused if it names a format other than a recognised extension\'s' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l tiff-bits -d 'TIFF bit depth: `16` (default) or `8`. TIFF only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l tiff-compression -d 'TIFF strip compression: `none` (default), `lzw` or `deflate`. TIFF only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l png-bits -d 'PNG bit depth: `16` (default) or `8`. PNG only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l png-compression -d 'PNG compression effort: `fast`, `default` (default) or `best`. PNG only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l jpeg-quality -d 'JPEG quality, 1-100, default 95. JPEG only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l jpeg-subsampling -d 'JPEG chroma subsampling: `420` (default) or `444`. JPEG only' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l exif -d 'Carry the alignment reference frame\'s EXIF: `on` (default) or `off`' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l icc -d 'Embed an ICC profile: `on` (default) or `off`. Has no effect on a DNG, whose colour is defined by its camera matrices' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l xmp -d 'Carry the source frame\'s XMP packet verbatim: `on` or `off` (default)' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l iptc -d 'Carry the source frame\'s IPTC block verbatim: `on` or `off` (default). Never written to PNG' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l frame -d 'Write this source frame "as fused" -- warped by its recorded transform and offset by the applied crop, at the resolved checkpoint\'s own composite width and height. Requires `--frame-output`' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l frame-output -d 'Where to write the frame `--frame` names' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand export" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -l ops -d 'The op list: a JSON array of ops in the persisted `Op` shape. A brush stroke blends frame `source_id` over the image along its path: fully within `radius * (1 - softness)`, fading linearly to nothing at `radius`. Every op is checked before anything is written; positions in error messages count from 0 in list order' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -l from -d 'The state to append under -- any unique prefix of its id. Defaults to the project\'s current head' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -l name -d 'Name the last appended state' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand retouch" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -f -a "list" -d 'List every visible checkpoint, newest first'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -f -a "delete" -d 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -f -a "compact" -d 'Rewrite the project to its live checkpoints only and report the bytes reclaimed'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and not __fish_seen_subcommand_from list delete compact help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from list" -l json -d 'Emit the JSON document instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from list" -l accept-changed-sources -d 'Accept changed source frames, printing their warning instead of refusing'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from list" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from list" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -l checkpoint -d 'The checkpoint to delete -- any unique prefix of its id' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -l accept-changed-sources -d 'Accept changed source frames, printing their warning instead of refusing'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from delete" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from compact" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from compact" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from compact" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from compact" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from help" -f -a "list" -d 'List every visible checkpoint, newest first'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from help" -f -a "delete" -d 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from help" -f -a "compact" -d 'Rewrite the project to its live checkpoints only and report the bytes reclaimed'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand history; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l algorithm -d 'Registered alignment algorithm id. Optional: defaults to the sole registered alignment algorithm' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l write-warped -d 'Write one warped TIFF per frame into this directory, at source dimensions, uncropped' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l set -d 'Set a parameter: `--set key=value`, repeatable. A repeated key is last-wins' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l max-host-memory -d 'A **ceiling only** on the adaptive host-memory budget the alignment workers are sized from: it can lower the budget the host-derived estimate resolved to, and can never raise it. Omit it entirely and the budget is derived from the host. Accepts suffixed sizes (`8GiB`, `512MiB`, `4096`)' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l max-vram -d 'A **ceiling only** on the adaptive device (VRAM) budget, for the alignment\'s GPU levels and the `--write-warped` fuse backend: it can lower the budget the startup probe settled on, and can never raise it. Accepts the same suffixed sizes as `--max-host-memory`. Only present when the `gpu` feature is compiled in' -r
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l preset -d 'Read parameter values from a TOML preset. `align` uses only `align-algo` and the `[align]` table; a non-empty `[stack]` table or `algo` key is unused here and says so on stderr, so one preset file serves both subcommands' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l write-preset -d 'Write the preset that reproduces this run to this path: `align-algo` plus the `[align]` values you actually chose' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l status -d 'Append one NDJSON status record for this run to this path' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l strict -d 'Refuse the run when an alignment fails'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l no-progress -d 'Print no progress at all. `align` has no fuse-tile units to count: this suppresses only the one stage line `align` otherwise prints'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -l cpu -d 'Force the CPU path even when a usable GPU adapter is available: the alignment\'s large levels and the `--write-warped` fuse backend both stay on the CPU. Only present when the `gpu` feature is compiled in'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand align" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand review" -l json -d 'Emit one wrapped JSON document instead of a human-readable table'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand review" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand review" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand review" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -f -a "clear" -d 'Remove every entry from the stage cache'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and not __fish_seen_subcommand_from clear help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from clear" -l json -d 'Emit the JSON report instead of human-readable text'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from clear" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from clear" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from clear" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from help" -f -a "clear" -d 'Remove every entry from the stage cache'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand cache; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -s o -l output -d 'Where to write the ZIP. Omit for a dated name in the current directory' -r -F
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -l include-minidumps -d 'Include the minidump binaries. They cannot be redacted'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -l force -d 'Overwrite the output path when it already exists'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -s v -d 'Increase log detail (repeatable)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -s q -l quiet -d 'Log errors only'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand diagnostics" -s h -l help -d 'Print help'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "info" -d 'Print project, TIFF or RAW metadata for a file, or for every frame in a directory'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "algorithms" -d 'List every registered algorithm, or print one named algorithm\'s full parameter schema'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "stack" -d 'Fuse supported TIFF or RAW frames of a directory into one **project** per detected stack, through a registered stacking algorithm. A run prepares projects for review; it does not export images -- run `focusforge-cli export` for that'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "export" -d 'Turn a project into an image file: the image with `-o`, the Z-Map with `--zmap`, either or both in one invocation. At least one of the two is required. This is the only command that writes an image'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "retouch" -d 'Append an op list to a project as new states, one per op, each the previous one\'s child: on the current head, or on `--from` as a branch. `export --checkpoint` writes any of them'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "history" -d 'List, delete or compact a project\'s checkpoints. Every history operation is reachable through this command, headless'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "align" -d 'Compute and report the per-frame transforms for a stack: readable text by default, a `--json` document on request, and an optional per-frame warped-TIFF dump for a visual check'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "review" -d 'Report one record per result found under a directory of projects, or a batch run\'s output directory: identity, outcome, sharpness ratio and Z-Map spread, with a failed stack a visible record rather than a missing file. Reads a run manifest when one is present, otherwise a non-recursive listing of `*.ffp` files; never groups frames itself and writes nothing'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "cache" -d 'Remove every entry from the stage cache under `.scratch/stage-cache/`. Never touches `.scratch/resume/` or any project'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "diagnostics" -d 'Package this installation\'s logs, crash reports, system summary, graphics adapter record, effective configuration and version into one ZIP, listing every file before writing it. The home directory and the user name are redacted out of every text entry, image data is never included, and nothing is sent anywhere'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and not __fish_seen_subcommand_from info algorithms stack export retouch history align review cache diagnostics help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and __fish_seen_subcommand_from history" -f -a "list" -d 'List every visible checkpoint, newest first'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and __fish_seen_subcommand_from history" -f -a "delete" -d 'Delete one checkpoint: it is hidden, and dropped from the project once no visible checkpoint is built on it; its space is reclaimed by `history compact`. Refuses the last visible checkpoint'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and __fish_seen_subcommand_from history" -f -a "compact" -d 'Rewrite the project to its live checkpoints only and report the bytes reclaimed'
complete -c focusforge-cli -n "__fish_focusforge_cli_using_subcommand help; and __fish_seen_subcommand_from cache" -f -a "clear" -d 'Remove every entry from the stage cache'
