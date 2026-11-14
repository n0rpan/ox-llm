#!/usr/bin/env python3
"""Rewrite a dense Qwen3.5 GGUF (arch `qwen35`) as a mathematically equivalent `qwen35moe` GGUF.

Lab tool for the T1.5 "tiny-real" gate (handoff TEST-MODEL-EVAL.md option b1). oxe loads only
`qwen35moe`; Qwen3.5-2B / 0.8B are the same family (hybrid GDN + gated attention, IMRoPE, MTP)
with a dense FFN and tied embeddings. This tool changes the file, never the engine:

- metadata: `qwen35.*` keys become `qwen35moe.*`; adds expert_used_count = E (default 8),
  expert_count = N (default 2E = 16), expert_feed_forward_length = F_e,
  expert_shared_feed_forward_length = F_s, with F_s + E * F_e == the dense feed_forward_length
  (2B: 2048 + 8 x 512 = 6144). Experts E..N-1 are all-zero padding: oxe's top-k requires
  used < count (kernels-sys argsort_top_k asserts k < n), so "8 of 8" cannot load there.
- every FFN block (the target layers and the MTP block `blk.{n_layer}`):
    ffn_gate / ffn_up rows [0, F_s)                  -> ffn_gate_shexp / ffn_up_shexp (byte copy)
    ffn_gate / ffn_up rows [F_s + e F_e, +F_e)       -> expert e of ffn_gate_exps / ffn_up_exps (byte copy)
    ffn_down k-columns [0, F_s)                      -> ffn_down_shexp, block scales x 2
    ffn_down k-columns [F_s + e F_e, +F_e)           -> expert e of ffn_down_exps, block scales x E
    ffn_gate_inp       = zeros F32 [n_embd, N]   (all logits equal: softmax 1/N each; the top-E are
                         experts 0..E-1 by the std::sort tie order that llama.cpp's argsort and oxe's
                         tie fallback share (insertion sort for N <= 16, stable); normalised
                         weight (1/N) / (E/N) = 1/E exactly)
    ffn_gate_inp_shexp = zeros F32 [n_embd]      (sigmoid(0) = 1/2)
- `output.weight` = a byte copy of `token_embd.weight` when the file has tied embeddings.

Exactness (also in lab/docs/TINY-REAL.md):
- Byte copies are exact. The x2 / x E (power of two) scale changes are exact in the f16 block
  scales (d, dmin, m) unless one overflows f16 (checked: the tool refuses) and exact in every f32
  product the kernels form from them (no overflow / underflow at these magnitudes).
- Activation quantisation (Q8_K / Q8_0 of h) is per 256 / 32 elements; the slices are block
  aligned, so every activation block is the dense one, bit for bit.
- The router weights are exactly 1/E (softmax of zeros = 1/N, N a power of two; the normalising
  sum E/N is exact), the padding experts are never selected (tie order; if one were, its output
  would be exactly 0 and the missing real expert would show as a gross error), the shared
  gate exactly 1/2, so each part reproduces its share of the dense dot product exactly.
- NOT exact: the summation order. Dense down = one accumulation over all F blocks; the rewrite
  = (shared expert) + sum_e (expert e), each a separate f32 accumulation, then added. Expect
  last-bit differences in every FFN output (relative ~1e-7), compounding over layers: logits
  agree to cosine ~1 and top-k / greedy agree except on near ties. Bitwise identity holds only
  between engines running the SAME rewritten file (oxe vs llama.cpp), not vs the dense original.

Only types with a known block-scale layout are rewritten (F32, F16, BF16, Q4_0, Q4_1, Q5_0, Q5_1,
Q8_0, Q2_K..Q6_K); anything else in an FFN tensor (IQ*, TQ*, ...) is refused. Standard library
only. Usage:

  convert_dense_to_qwen35moe.py SRC.gguf OUT.gguf [--experts 8] [--shared-ff 2048] [--force]
  convert_dense_to_qwen35moe.py --inspect FILE.gguf [--json]    per-tensor types + oxe support
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import sys

TOOL_VERSION = 1
GGUF_MAGIC = b'GGUF'
DEFAULT_ALIGNMENT = 32

# ggml type id -> (name, block elements, block bytes, f16 scale-field byte offsets)
# The scale offsets are every per-block float that multiplies the dequantised value linearly
# (d, and the min/offset term for the "_1"/K types); scaling all of them by s scales the block by s.
TYPES = {
    0: ('F32', 1, 4, None),
    1: ('F16', 1, 2, None),
    2: ('Q4_0', 32, 18, (0,)),
    3: ('Q4_1', 32, 20, (0, 2)),
    6: ('Q5_0', 32, 22, (0,)),
    7: ('Q5_1', 32, 24, (0, 2)),
    8: ('Q8_0', 32, 34, (0,)),
    10: ('Q2_K', 256, 84, (80, 82)),
    11: ('Q3_K', 256, 110, (108,)),
    12: ('Q4_K', 256, 144, (0, 2)),
    13: ('Q5_K', 256, 176, (0, 2)),
    14: ('Q6_K', 256, 210, (208,)),
    30: ('BF16', 1, 2, None),
}
# block geometry of types we may copy but never rescale (non-FFN tensors pass through untouched)
COPY_ONLY = {
    9: ('Q8_1', 32, 36), 15: ('Q8_K', 256, 292), 16: ('IQ2_XXS', 256, 66), 17: ('IQ2_XS', 256, 74),
    18: ('IQ3_XXS', 256, 98), 19: ('IQ1_S', 256, 50), 20: ('IQ4_NL', 32, 18), 21: ('IQ3_S', 256, 110),
    22: ('IQ2_S', 256, 82), 23: ('IQ4_XS', 256, 136), 24: ('I8', 1, 1), 25: ('I16', 1, 2),
    26: ('I32', 1, 4), 27: ('I64', 1, 8), 28: ('F64', 1, 8), 29: ('IQ1_M', 256, 56),
    34: ('TQ1_0', 256, 54), 35: ('TQ2_0', 256, 66),
}
# what oxe's loader accepts (engine/crates/model/src/{desc,weights}.rs): matmul weights
OXE_MATMUL = {'Q3_K', 'Q4_K', 'Q5_K', 'Q6_K', 'Q8_0'}

# GGUF metadata value types
U8, I8, U16, I16, U32, I32, F32V, BOOL, STR, ARR, U64, I64, F64V = range(13)
SCALAR_FMT = {U8: '<B', I8: '<b', U16: '<H', I16: '<h', U32: '<I', I32: '<i', F32V: '<f',
              BOOL: '<?', U64: '<Q', I64: '<q', F64V: '<d'}


class ConvertError(Exception):
    pass


def type_info(ty):
    if ty in TYPES:
        name, blk, size, _ = TYPES[ty]
        return name, blk, size
    if ty in COPY_ONLY:
        return COPY_ONLY[ty]
    raise ConvertError('unknown ggml type id {}'.format(ty))


def type_name(ty):
    try:
        return type_info(ty)[0]
    except ConvertError:
        return 'type{}'.format(ty)


def tensor_nbytes(ty, dims):
    _, blk, size = type_info(ty)
    n = 1
    for d in dims:
        n *= d
    if dims[0] % blk:
        raise ConvertError('row length {} is not a multiple of the {} block {}'.format(dims[0], type_name(ty), blk))
    return n // blk * size


# ------------------------------------------------------------------ GGUF read / write

class Reader:
    def __init__(self, f):
        self.f = f

    def take(self, n):
        b = self.f.read(n)
        if len(b) != n:
            raise ConvertError('truncated GGUF header')
        return b

    def scalar(self, fmt):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))[0]

    def string(self):
        n = self.scalar('<Q')
        if n > 1 << 26:
            raise ConvertError('implausible string length {}'.format(n))
        return self.take(n).decode('utf-8')

    def value(self, vt):
        if vt == STR:
            return self.string()
        if vt == ARR:
            et = self.scalar('<I')
            n = self.scalar('<Q')
            if n > 1 << 28:
                raise ConvertError('implausible array length {}'.format(n))
            if et in SCALAR_FMT:
                fmt = SCALAR_FMT[et]
                size = struct.calcsize(fmt)
                raw = self.take(size * n)
                return (et, list(struct.unpack('<{}{}'.format(n, fmt[1]), raw)))
            return (et, [self.value(et) for _ in range(n)])
        if vt in SCALAR_FMT:
            return self.scalar(SCALAR_FMT[vt])
        raise ConvertError('unknown metadata value type {}'.format(vt))


def read_header(path):
    """Return {'version', 'kv': [(key, vtype, value)], 'tensors': [{name, dims, type, offset}],
    'alignment', 'data_start'}; array values are (elem_type, list)."""
    with open(path, 'rb') as f:
        r = Reader(f)
        if r.take(4) != GGUF_MAGIC:
            raise ConvertError('{}: not a GGUF file'.format(path))
        version = r.scalar('<I')
        if version not in (2, 3):
            raise ConvertError('GGUF version {} not supported'.format(version))
        n_tensors = r.scalar('<Q')
        n_kv = r.scalar('<Q')
        kv = []
        for _ in range(n_kv):
            key = r.string()
            vt = r.scalar('<I')
            kv.append((key, vt, r.value(vt)))
        tensors = []
        for _ in range(n_tensors):
            name = r.string()
            nd = r.scalar('<I')
            dims = [r.scalar('<Q') for _ in range(nd)]
            ty = r.scalar('<I')
            off = r.scalar('<Q')
            tensors.append({'name': name, 'dims': dims, 'type': ty, 'offset': off})
        align = DEFAULT_ALIGNMENT
        for k, vt, v in kv:
            if k == 'general.alignment':
                align = int(v)
        pos = f.tell()
        data_start = (pos + align - 1) // align * align
    for t in tensors:
        t['nbytes'] = tensor_nbytes(t['type'], t['dims'])
    return {'version': version, 'kv': kv, 'tensors': tensors, 'alignment': align, 'data_start': data_start}


def kv_get(h, key, default=None):
    for k, _, v in h['kv']:
        if k == key:
            return v
    return default


def enc_string(s):
    b = s.encode('utf-8')
    return struct.pack('<Q', len(b)) + b


def enc_value(vt, v):
    if vt == STR:
        return enc_string(v)
    if vt == ARR:
        et, items = v
        out = [struct.pack('<IQ', et, len(items))]
        if et in SCALAR_FMT:
            out.append(struct.pack('<{}{}'.format(len(items), SCALAR_FMT[et][1]), *items))
        else:
            out.extend(enc_value(et, x) for x in items)
        return b''.join(out)
    return struct.pack(SCALAR_FMT[vt], v)


def write_gguf(path, kv, tensors, alignment):
    """tensors: list of {name, dims, type, data: bytes | callable(write)}; data written in order."""
    head = [GGUF_MAGIC, struct.pack('<IQQ', 3, len(tensors), len(kv))]
    for k, vt, v in kv:
        head.append(enc_string(k) + struct.pack('<I', vt) + enc_value(vt, v))
    off = 0
    offsets = []
    for t in tensors:
        off = (off + alignment - 1) // alignment * alignment
        offsets.append(off)
        n = tensor_nbytes(t['type'], t['dims'])
        t['_nbytes'] = n
        head.append(enc_string(t['name']) + struct.pack('<I', len(t['dims'])) +
                    b''.join(struct.pack('<Q', d) for d in t['dims']) + struct.pack('<IQ', t['type'], off))
        off += n
    blob = b''.join(head)
    pad = (-len(blob)) % alignment
    tmp = Path(str(path) + '.part')
    with open(tmp, 'wb') as f:
        f.write(blob + b'\0' * pad)
        base = f.tell()
        for t, o in zip(tensors, offsets):
            gap = base + o - f.tell()
            if gap < 0:
                raise ConvertError('internal: tensor offsets overlap')
            f.write(b'\0' * gap)
            data = t['data']() if callable(t['data']) else t['data']
            if len(data) != t['_nbytes']:
                raise ConvertError('{}: {} data bytes, expected {}'.format(t['name'], len(data), t['_nbytes']))
            f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


# ------------------------------------------------------------------ exact power-of-two scaling

def _scale_f16_bits(bits, log2):
    """Exact f16 * 2**log2 (bits in, bits out); refuses overflow and precision loss."""
    v = struct.unpack('<e', struct.pack('<H', bits))[0]
    if v != v or v in (float('inf'), float('-inf')) or v == 0.0:
        return bits
    w = v * (2.0 ** log2)
    try:
        out = struct.unpack('<H', struct.pack('<e', w))[0]
    except (OverflowError, struct.error):
        raise ConvertError('f16 scale {} x 2^{} overflows f16'.format(v, log2))
    if struct.unpack('<e', struct.pack('<H', out))[0] != w:
        raise ConvertError('f16 scale {} x 2^{} is not exact'.format(v, log2))
    return out


def scale_blocks(ty, data, log2):
    """Multiply every value of a tensor (raw bytes of type `ty`) by 2**log2, exactly."""
    if log2 == 0:
        return bytes(data)
    if ty not in TYPES:
        raise ConvertError('{} has no known scale layout; cannot rescale it exactly'.format(type_name(ty)))
    name, _, size, offs = TYPES[ty]
    buf = bytearray(data)
    if name == 'F32':
        vals = struct.unpack('<{}f'.format(len(buf) // 4), buf)
        f = 2.0 ** log2
        out = [v * f for v in vals]
        for a, b in zip(vals, out):
            if a == a and abs(a) != float('inf') and a != 0.0 and (b / f != a or abs(b) == float('inf')):
                raise ConvertError('f32 value {} x 2^{} is not exact'.format(a, log2))
        struct.pack_into('<{}f'.format(len(out)), buf, 0, *out)
        return bytes(buf)
    if name == 'BF16':
        n = len(buf) // 2
        halves = struct.unpack('<{}H'.format(n), buf)
        out = []
        f = 2.0 ** log2
        for h in halves:
            v = struct.unpack('<f', struct.pack('<I', h << 16))[0]
            if v != v or v == 0.0 or abs(v) == float('inf'):
                out.append(h)
                continue
            w = v * f
            wb = struct.unpack('<I', struct.pack('<f', w))[0]
            if wb & 0xFFFF or abs(w) == float('inf'):
                raise ConvertError('bf16 value {} x 2^{} is not exact'.format(v, log2))
            out.append(wb >> 16)
        struct.pack_into('<{}H'.format(n), buf, 0, *out)
        return bytes(buf)
    if name == 'F16':
        n = len(buf) // 2
        out = [_scale_f16_bits(h, log2) for h in struct.unpack('<{}H'.format(n), buf)]
        struct.pack_into('<{}H'.format(n), buf, 0, *out)
        return bytes(buf)
    if len(buf) % size:
        raise ConvertError('{} data is not a whole number of blocks'.format(name))
    cache = {}
    for base in range(0, len(buf), size):
        for o in offs:
            p = base + o
            bits = buf[p] | buf[p + 1] << 8
            nb = cache.get(bits)
            if nb is None:
                nb = cache[bits] = _scale_f16_bits(bits, log2)
            buf[p] = nb & 0xFF
            buf[p + 1] = nb >> 8
    return bytes(buf)


# ------------------------------------------------------------------ slicing

def row_bytes(ty, k):
    _, blk, size = type_info(ty)
    if k % blk:
        raise ConvertError('k {} not a multiple of the {} block {}'.format(k, type_name(ty), blk))
    return k // blk * size


def slice_rows(ty, dims, data, r0, r1):
    """Rows [r0, r1) of a 2-D tensor [k, rows] (ggml: ne0 = k contiguous)."""
    rb = row_bytes(ty, dims[0])
    return bytes(data[r0 * rb:r1 * rb])


def slice_k(ty, dims, data, c0, c1):
    """k-columns [c0, c1) of every row of a 2-D tensor [k, rows]; must be block aligned."""
    _, blk, size = type_info(ty)
    if c0 % blk or c1 % blk:
        raise ConvertError('k slice [{}, {}) is not aligned to the {} block {}'.format(c0, c1, type_name(ty), blk))
    rb = row_bytes(ty, dims[0])
    a, b = c0 // blk * size, c1 // blk * size
    return b''.join(bytes(data[r * rb + a:r * rb + b]) for r in range(dims[1]))


# ------------------------------------------------------------------ conversion

def _log2_exact(n):
    if n < 1 or n & (n - 1):
        raise ConvertError('{} is not a power of two'.format(n))
    return n.bit_length() - 1


def plan_metadata(h, n_expert, ff_shexp, extra, n_total=None):
    n_total = n_total or n_expert
    src_arch = kv_get(h, 'general.architecture')
    if src_arch != 'qwen35':
        raise ConvertError("source architecture is {!r}, expected 'qwen35' (dense Qwen3.5)".format(src_arch))
    ff = kv_get(h, 'qwen35.feed_forward_length')
    if isinstance(ff, tuple):
        vals = set(ff[1])
        if len(vals) != 1:
            raise ConvertError('per-layer feed_forward_length {} not supported'.format(sorted(vals)))
        ff = vals.pop()
    if not ff:
        raise ConvertError('missing qwen35.feed_forward_length')
    if ff_shexp is None:
        # largest power-of-two-friendly split: shared = ff - n_expert * ff_exp, ff_exp a multiple of 256
        ff_exp = (ff // (n_expert + 4)) // 256 * 256 or 256
        ff_shexp = ff - n_expert * ff_exp
    else:
        if (ff - ff_shexp) % n_expert:
            raise ConvertError('feed_forward_length {} - shared {} is not divisible by {} experts'.format(ff, ff_shexp, n_expert))
        ff_exp = (ff - ff_shexp) // n_expert
    if ff_exp <= 0 or ff_shexp <= 0 or ff_exp % 256 or ff_shexp % 256:
        raise ConvertError('expert width {} / shared width {} must be positive multiples of 256 (oxe MoE units)'.format(ff_exp, ff_shexp))
    kv = []
    for k, vt, v in h['kv']:
        if k == 'general.architecture':
            v = 'qwen35moe'
        elif k.startswith('qwen35.'):
            k = 'qwen35moe.' + k[len('qwen35.'):]
        kv.append((k, vt, v))
    have = {k for k, _, _ in kv}
    for k, vt, v in (('qwen35moe.expert_count', U32, n_total),
                     ('qwen35moe.expert_used_count', U32, n_expert),
                     ('qwen35moe.expert_feed_forward_length', U32, ff_exp),
                     ('qwen35moe.expert_shared_feed_forward_length', U32, ff_shexp)):
        if k in have:
            raise ConvertError('source already has {}'.format(k))
        kv.append((k, vt, v))
    for k, v in extra:
        kv.append((k, STR, v))
    return kv, ff, ff_exp, ff_shexp


def convert(src, dst, n_expert=8, ff_shexp=None, source_sha256=None, log=None, n_total=None):
    """n_expert real experts, all used (expert_used_count = n_expert); n_total (default 2 x n_expert)
    is expert_count: the experts beyond n_expert are all-zero and never selected (see the module doc)."""
    log = log or (lambda *_: None)
    h = read_header(src)
    log2_e = _log2_exact(n_expert)
    n_total = 2 * n_expert if n_total is None else n_total
    _log2_exact(n_total)
    if n_total < n_expert:
        raise ConvertError('expert_count {} < used {}'.format(n_total, n_expert))
    extra = [('tiny_real.converter', 'convert_dense_to_qwen35moe v{}'.format(TOOL_VERSION))]
    if source_sha256:
        extra.append(('tiny_real.source_sha256', source_sha256))
    kv, ff, ff_exp, ff_shexp = plan_metadata(h, n_expert, ff_shexp, extra, n_total)
    by_name = {t['name']: t for t in h['tensors']}
    n_embd = kv_get(h, 'qwen35.embedding_length')
    with open(src, 'rb') as fh:
        return _convert_open(h, fh, dst, kv, by_name, n_embd, n_expert, n_total, log2_e, ff, ff_exp, ff_shexp, log)


def _convert_open(h, fh, dst, kv, by_name, n_embd, n_expert, n_total, log2_e, ff, ff_exp, ff_shexp, log):
    def raw(t):
        fh.seek(h['data_start'] + t['offset'])
        b = fh.read(t['nbytes'])
        if len(b) != t['nbytes']:
            raise ConvertError('{}: truncated tensor data'.format(t['name']))
        return b

    pad = n_total - n_expert
    out = []
    blocks = sorted({int(n.split('.')[1]) for n in by_name if n.startswith('blk.')})
    handled = set()
    for t in h['tensors']:
        name = t['name']
        parts = name.split('.')
        if parts[0] == 'blk' and len(parts) >= 3 and parts[2] in ('ffn_gate', 'ffn_up', 'ffn_down'):
            continue  # emitted per block below, at the position of ffn_gate
        out.append({'name': name, 'dims': list(t['dims']), 'type': t['type'], 'data': (lambda t=t: raw(t))})
        if name == 'token_embd.weight' and 'output.weight' not in by_name:
            out.append({'name': 'output.weight', 'dims': list(t['dims']), 'type': t['type'], 'data': (lambda t=t: raw(t))})
            log('tied embeddings: output.weight = byte copy of token_embd.weight ({})'.format(type_name(t['type'])))
    # FFN tensors per block (appended in block order; the GGUF tensor order is free)
    for il in blocks:
        p = 'blk.{}.'.format(il)
        g, u, d = (by_name.get(p + s + '.weight') for s in ('ffn_gate', 'ffn_up', 'ffn_down'))
        if g is None and u is None and d is None:
            continue
        if g is None or u is None or d is None:
            raise ConvertError('block {}: incomplete dense FFN'.format(il))
        for t, want in ((g, [n_embd, ff]), (u, [n_embd, ff]), (d, [ff, n_embd])):
            if list(t['dims']) != want:
                raise ConvertError('{}: shape {} != {}'.format(t['name'], t['dims'], want))
            if t['type'] not in TYPES:
                raise ConvertError('{}: type {} cannot be split/rescaled exactly'.format(t['name'], type_name(t['type'])))
        handled.update(x['name'] for x in (g, u, d))

        def gate_up(t, shared):
            def make():
                data = raw(t)
                if shared:
                    return slice_rows(t['type'], t['dims'], data, 0, ff_shexp)
                return b''.join(slice_rows(t['type'], t['dims'], data, ff_shexp + e * ff_exp, ff_shexp + (e + 1) * ff_exp)
                                for e in range(n_expert)) + bytes(pad * row_bytes(t['type'], n_embd) * ff_exp)
            return make

        def down(t, shared):
            def make():
                data = raw(t)
                if shared:
                    return scale_blocks(t['type'], slice_k(t['type'], t['dims'], data, 0, ff_shexp), 1)
                return b''.join(scale_blocks(t['type'], slice_k(t['type'], t['dims'], data, ff_shexp + e * ff_exp,
                                                                  ff_shexp + (e + 1) * ff_exp), log2_e)
                                for e in range(n_expert)) + bytes(pad * row_bytes(t['type'], ff_exp) * n_embd)
            return make

        out += [
            {'name': p + 'ffn_gate_inp.weight', 'dims': [n_embd, n_total], 'type': 0, 'data': bytes(4 * n_embd * n_total)},
            {'name': p + 'ffn_gate_inp_shexp.weight', 'dims': [n_embd], 'type': 0, 'data': bytes(4 * n_embd)},
            {'name': p + 'ffn_gate_exps.weight', 'dims': [n_embd, ff_exp, n_total], 'type': g['type'], 'data': gate_up(g, False)},
            {'name': p + 'ffn_up_exps.weight', 'dims': [n_embd, ff_exp, n_total], 'type': u['type'], 'data': gate_up(u, False)},
            {'name': p + 'ffn_down_exps.weight', 'dims': [ff_exp, n_embd, n_total], 'type': d['type'], 'data': down(d, False)},
            {'name': p + 'ffn_gate_shexp.weight', 'dims': [n_embd, ff_shexp], 'type': g['type'], 'data': gate_up(g, True)},
            {'name': p + 'ffn_up_shexp.weight', 'dims': [n_embd, ff_shexp], 'type': u['type'], 'data': gate_up(u, True)},
            {'name': p + 'ffn_down_shexp.weight', 'dims': [ff_shexp, n_embd], 'type': d['type'], 'data': down(d, True)},
        ]
    if not handled:
        raise ConvertError('no dense FFN tensors found')
    write_gguf(dst, kv, out, h['alignment'])
    return {'ff': ff, 'ff_exp': ff_exp, 'ff_shexp': ff_shexp, 'n_expert': n_total, 'n_expert_used': n_expert,
            'ffn_blocks': len(handled) // 3, 'tensors_out': len(out)}


# ------------------------------------------------------------------ inspection (TINY-REAL-1)

def inspect(path):
    h = read_header(path)
    arch = kv_get(h, 'general.architecture')
    by_type = {}
    rows = []
    problems = []
    for t in h['tensors']:
        tn = type_name(t['type'])
        by_type.setdefault(tn, [0, 0])
        by_type[tn][0] += 1
        by_type[tn][1] += t['nbytes']
        small = len(t['dims']) == 1 or t['name'].endswith(('ssm_a', 'ssm_conv1d.weight', 'ssm_dt.bias'))
        if tn == 'F32':
            ok = True
        elif tn == 'BF16':
            ok = t['name'].startswith('blk.') and '.nextn.' in t['name'] or small
        else:
            ok = tn in OXE_MATMUL and not (t['name'] == 'token_embd.weight' and tn == 'Q3_K')
        if not ok:
            problems.append('{} {}'.format(t['name'], tn))
        rows.append({'name': t['name'], 'type': tn, 'dims': t['dims'], 'oxe_ok': ok})
    meta = {k: (v if not isinstance(v, tuple) else '[{} items]'.format(len(v[1])))
            for k, _, v in h['kv'] if not k.startswith('tokenizer.ggml.') or k in ('tokenizer.ggml.model', 'tokenizer.ggml.pre')}
    return {'arch': arch, 'n_tensors': len(h['tensors']), 'by_type': by_type, 'problems': problems,
            'tied_embeddings': not any(r['name'] == 'output.weight' for r in rows), 'metadata': meta, 'tensors': rows}


def sha256_file(path):
    hs = hashlib.sha256()
    with open(path, 'rb') as f:
        for b in iter(lambda: f.read(1 << 22), b''):
            hs.update(b)
    return hs.hexdigest()


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('src')
    ap.add_argument('dst', nargs='?')
    ap.add_argument('--inspect', action='store_true', help='print per-tensor types and oxe support, no output file')
    ap.add_argument('--json', action='store_true')
    ap.add_argument('--experts', type=int, default=8, help='real experts = expert_used_count (power of two)')
    ap.add_argument('--expert-count', type=int, default=None,
                    help='expert_count (power of two, default 2 x --experts: zero padding experts, because oxe requires used < count)')
    ap.add_argument('--shared-ff', type=int, default=None, help='shared-expert width (default: ff - experts * ff_exp)')
    ap.add_argument('--no-sha', action='store_true', help='do not hash the source (faster)')
    ap.add_argument('--force', action='store_true', help='overwrite DST')
    a = ap.parse_args(argv)
    try:
        if a.inspect:
            r = inspect(a.src)
            if a.json:
                print(json.dumps(r, indent=1))
            else:
                print('arch {}  tensors {}  tied_embeddings {}'.format(r['arch'], r['n_tensors'], r['tied_embeddings']))
                for tn, (n, b) in sorted(r['by_type'].items()):
                    print('  {:6s} {:4d} tensors {:8.1f} MiB'.format(tn, n, b / 2 ** 20))
                print('oxe-unsupported tensors: {}'.format(', '.join(r['problems']) or 'none'))
            return 0
        if not a.dst:
            ap.error('DST is required unless --inspect')
        if Path(a.dst).exists() and not a.force:
            ap.error('{} exists (use --force)'.format(a.dst))
        src_sha = None if a.no_sha else sha256_file(a.src)
        r = convert(a.src, a.dst, a.experts, a.shared_ff, src_sha, log=lambda m: print(m, file=sys.stderr), n_total=a.expert_count)
        r['source_sha256'] = src_sha
        r['output_sha256'] = None if a.no_sha else sha256_file(a.dst)
        print(json.dumps(r))
        return 0
    except ConvertError as e:
        print('error: {}'.format(e), file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
