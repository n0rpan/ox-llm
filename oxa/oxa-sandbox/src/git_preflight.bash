# Convenience preflight, not a security boundary: absolute paths, aliases and
# `command git` can bypass a shell function. The read-only mounts remain the
# enforcement. Git renames/copies refs BEFORE rewriting branch configuration,
# so ordinary invocations need an early refusal to avoid a partially done job.
git() {
    local -a args=("$@")
    local i=0 arg
    while (( i < ${#args[@]} )); do
        arg=${args[i]}
        case "$arg" in
            -C|-c|--git-dir|--work-tree|--namespace|--config-env)
                ((i+=2));;
            --) ((i+=1)); break;;
            -*) ((i+=1));;
            *) break;;
        esac
    done
    if [[ ${args[i]-} == branch ]]; then
        ((i+=1))
        while (( i < ${#args[@]} )); do
            arg=${args[i]}
            case "$arg" in
                --) break;;
                --move|--mo|--mov|--copy|--cop) break;;
                --format|--sort) ((i+=2)); continue;;
                --*) ((i+=1)); continue;;
                -*)
                    if [[ $arg != *[mMcC]* ]]; then
                        ((i+=1)); continue
                    fi
                    break;;
                *) ((i+=1)); continue;;
            esac
        done
        if (( i < ${#args[@]} )) && [[ ${args[i]} != -- ]]; then
            printf '%s\n' 'oxa: git branch rename/copy refused before changing refs: Git rewrites the shared config after changing refs, but that config is read-only in the sandbox. Run this rename/copy outside oxa; branch creation and commits work here.' >&2
            return 1
        fi
    fi
    command git "$@"
}
# Also cover ordinary nested `bash -c` commands, without changing host startup files.
export -f git
