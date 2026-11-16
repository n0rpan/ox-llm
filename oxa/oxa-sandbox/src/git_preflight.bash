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
                    arg=${arg:1}
                    while [[ -n $arg ]]; do
                        case ${arg:0:1} in
                            m|M|c|C) break 2;;
                            # -t has an optional attached value; -u requires
                            # a value (attached or in the following argument).
                            # Neither value is a cluster of short options.
                            t) break;;
                            u)
                                if (( ${#arg} == 1 )); then ((i+=1)); fi
                                break;;
                        esac
                        arg=${arg:1}
                    done
                    ((i+=1)); continue;;
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
