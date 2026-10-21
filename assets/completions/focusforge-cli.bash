_focusforge__cli() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="focusforge__cli"
                ;;
            focusforge__cli,algorithms)
                cmd="focusforge__cli__subcmd__algorithms"
                ;;
            focusforge__cli,align)
                cmd="focusforge__cli__subcmd__align"
                ;;
            focusforge__cli,cache)
                cmd="focusforge__cli__subcmd__cache"
                ;;
            focusforge__cli,diagnostics)
                cmd="focusforge__cli__subcmd__diagnostics"
                ;;
            focusforge__cli,export)
                cmd="focusforge__cli__subcmd__export"
                ;;
            focusforge__cli,help)
                cmd="focusforge__cli__subcmd__help"
                ;;
            focusforge__cli,history)
                cmd="focusforge__cli__subcmd__history"
                ;;
            focusforge__cli,info)
                cmd="focusforge__cli__subcmd__info"
                ;;
            focusforge__cli,retouch)
                cmd="focusforge__cli__subcmd__retouch"
                ;;
            focusforge__cli,review)
                cmd="focusforge__cli__subcmd__review"
                ;;
            focusforge__cli,stack)
                cmd="focusforge__cli__subcmd__stack"
                ;;
            focusforge__cli__subcmd__cache,clear)
                cmd="focusforge__cli__subcmd__cache__subcmd__clear"
                ;;
            focusforge__cli__subcmd__cache,help)
                cmd="focusforge__cli__subcmd__cache__subcmd__help"
                ;;
            focusforge__cli__subcmd__cache__subcmd__help,clear)
                cmd="focusforge__cli__subcmd__cache__subcmd__help__subcmd__clear"
                ;;
            focusforge__cli__subcmd__cache__subcmd__help,help)
                cmd="focusforge__cli__subcmd__cache__subcmd__help__subcmd__help"
                ;;
            focusforge__cli__subcmd__help,algorithms)
                cmd="focusforge__cli__subcmd__help__subcmd__algorithms"
                ;;
            focusforge__cli__subcmd__help,align)
                cmd="focusforge__cli__subcmd__help__subcmd__align"
                ;;
            focusforge__cli__subcmd__help,cache)
                cmd="focusforge__cli__subcmd__help__subcmd__cache"
                ;;
            focusforge__cli__subcmd__help,diagnostics)
                cmd="focusforge__cli__subcmd__help__subcmd__diagnostics"
                ;;
            focusforge__cli__subcmd__help,export)
                cmd="focusforge__cli__subcmd__help__subcmd__export"
                ;;
            focusforge__cli__subcmd__help,help)
                cmd="focusforge__cli__subcmd__help__subcmd__help"
                ;;
            focusforge__cli__subcmd__help,history)
                cmd="focusforge__cli__subcmd__help__subcmd__history"
                ;;
            focusforge__cli__subcmd__help,info)
                cmd="focusforge__cli__subcmd__help__subcmd__info"
                ;;
            focusforge__cli__subcmd__help,retouch)
                cmd="focusforge__cli__subcmd__help__subcmd__retouch"
                ;;
            focusforge__cli__subcmd__help,review)
                cmd="focusforge__cli__subcmd__help__subcmd__review"
                ;;
            focusforge__cli__subcmd__help,stack)
                cmd="focusforge__cli__subcmd__help__subcmd__stack"
                ;;
            focusforge__cli__subcmd__help__subcmd__cache,clear)
                cmd="focusforge__cli__subcmd__help__subcmd__cache__subcmd__clear"
                ;;
            focusforge__cli__subcmd__help__subcmd__history,compact)
                cmd="focusforge__cli__subcmd__help__subcmd__history__subcmd__compact"
                ;;
            focusforge__cli__subcmd__help__subcmd__history,delete)
                cmd="focusforge__cli__subcmd__help__subcmd__history__subcmd__delete"
                ;;
            focusforge__cli__subcmd__help__subcmd__history,list)
                cmd="focusforge__cli__subcmd__help__subcmd__history__subcmd__list"
                ;;
            focusforge__cli__subcmd__history,compact)
                cmd="focusforge__cli__subcmd__history__subcmd__compact"
                ;;
            focusforge__cli__subcmd__history,delete)
                cmd="focusforge__cli__subcmd__history__subcmd__delete"
                ;;
            focusforge__cli__subcmd__history,help)
                cmd="focusforge__cli__subcmd__history__subcmd__help"
                ;;
            focusforge__cli__subcmd__history,list)
                cmd="focusforge__cli__subcmd__history__subcmd__list"
                ;;
            focusforge__cli__subcmd__history__subcmd__help,compact)
                cmd="focusforge__cli__subcmd__history__subcmd__help__subcmd__compact"
                ;;
            focusforge__cli__subcmd__history__subcmd__help,delete)
                cmd="focusforge__cli__subcmd__history__subcmd__help__subcmd__delete"
                ;;
            focusforge__cli__subcmd__history__subcmd__help,help)
                cmd="focusforge__cli__subcmd__history__subcmd__help__subcmd__help"
                ;;
            focusforge__cli__subcmd__history__subcmd__help,list)
                cmd="focusforge__cli__subcmd__history__subcmd__help__subcmd__list"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        focusforge__cli)
            opts="-v -q -h -V --quiet --help --version info algorithms stack export retouch history align review cache diagnostics help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__algorithms)
            opts="-v -q -h --json --set --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__align)
            opts="-v -q -h --algorithm --json --strict --write-warped --set --max-host-memory --max-vram --no-progress --preset --write-preset --status --cpu --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --algorithm)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --write-warped)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-host-memory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-vram)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --preset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --write-preset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --status)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__cache)
            opts="-v -q -h --json --quiet --help clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__cache__subcmd__clear)
            opts="-v -q -h --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__cache__subcmd__help)
            opts="clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__cache__subcmd__help__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__cache__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__diagnostics)
            opts="-o -v -q -h --output --include-minidumps --force --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__export)
            opts="-o -v -q -h --output --zmap --checkpoint --format --tiff-bits --tiff-compression --png-bits --png-compression --jpeg-quality --jpeg-subsampling --exif --icc --xmp --iptc --frame --frame-output --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --zmap)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --checkpoint)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --format)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tiff-bits)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tiff-compression)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --png-bits)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --png-compression)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --jpeg-quality)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --jpeg-subsampling)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --exif)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --icc)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --xmp)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --iptc)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --frame)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --frame-output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help)
            opts="info algorithms stack export retouch history align review cache diagnostics help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__algorithms)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__align)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__cache)
            opts="clear"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__cache__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__diagnostics)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__history)
            opts="list delete compact"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__history__subcmd__compact)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__history__subcmd__delete)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__history__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__info)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__retouch)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__review)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__help__subcmd__stack)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history)
            opts="-v -q -h --quiet --help list delete compact help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__compact)
            opts="-v -q -h --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__delete)
            opts="-v -q -h --checkpoint --json --accept-changed-sources --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --checkpoint)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__help)
            opts="list delete compact help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__help__subcmd__compact)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__help__subcmd__delete)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__history__subcmd__list)
            opts="-v -q -h --json --accept-changed-sources --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__info)
            opts="-v -q -h --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__retouch)
            opts="-v -q -h --ops --from --name --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --ops)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --from)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__review)
            opts="-v -q -h --json --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        focusforge__cli__subcmd__stack)
            opts="-o -v -q -h --output --algorithm --json --set --groups --write-groups --only --no-align --no-crop --strict --align-algorithm --align-set --resume --no-progress --tile-size --max-host-memory --max-vram --preset --write-preset --status --dry-run --cpu --no-cache --quiet --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --output)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --algorithm)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --groups)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --write-groups)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --only)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --align-algorithm)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --align-set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tile-size)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-host-memory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-vram)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --preset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --write-preset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --status)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _focusforge__cli -o nosort -o bashdefault -o default focusforge-cli
else
    complete -F _focusforge__cli -o bashdefault -o default focusforge-cli
fi
