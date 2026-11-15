"""Profile builders for retained runtime policy tests; no staging entrypoint."""

QUALIFIED_MODELS = {
    'ornith-35b-a3b-local': 'a7373f724cf3693e8a6a5f8209ae9a06ff8071ae2af60659bdbdf8ff6f6c10df',
    'qwen3.6-35b-a3b-local': '0b21525e972670ed59e1812e170b27c26355381f0656ecc4e25617ece7dac58b',
}

QUALIFIED_SERVER_SHA = '649ac80670e58d5a4806355543f67a8bce0ce6034cf63e380dbf0c2a3f3ccf69'

THREADS = 8

OXE_ARGS = ['--engine', 'oxe', '--model', '{model}', '--alias', '{alias}', '--ctx-size', '{context}',
            '--oxe-cpus', '0-7', '--oxe-prefill-cpus', '0-11', '--ubatch-size', '256',
            '--ctx-checkpoints', '4', '--reasoning', 'off', '--spec-type', 'none',
            '--prefix-cache', 'default']

def oxe_override(server_binary, server_sha256):
    return {'server_binary': server_binary, 'server_sha256': server_sha256, 'threads': THREADS,
            'oxe_args': list(OXE_ARGS), 'environment': {'OXE_ATTN_TILED_MIN_KV': '0'},
            'validation': {'kind': 'oxe-rust-server-qualification', 'engine_source': '707a0ec',
                           'server_sha256': server_sha256, 'report': 'docs/RUST-SERVER-UPGRADE.md'}}
