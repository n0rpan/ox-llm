"""convert_dense_to_qwen35moe: synthetic dense GGUFs only (no model, no network).

Checks the exactness claims of lab/docs/TINY-REAL.md on dequantised values: gate/up slices are
bit-identical, down slices are exactly x2 (shared) / x E (experts), and the rewritten FFN
(router 1/E each, shared gate 1/2) equals the dense FFN up to float64 summation order.
"""
import importlib.util
import math
from pathlib import Path
import random
import struct
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    'convert_dense_to_qwen35moe', Path(__file__).resolve().parents[1] / 'tools/tiny_real/convert_dense_to_qwen35moe.py')
conv = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(conv)

F32, Q8_0, Q4_K, Q6_K, IQ4_XS = 0, 8, 12, 14, 23
N_EMBD, FF, E, N, FF_EXP, FF_SH = 256, 2304, 8, 16, 256, 256


def f16(v):
    return struct.pack('<e', v)


def rand_blocks(rng, ty, n_elems):
    _, blk, size = conv.type_info(ty)
    out = bytearray()
    for _ in range(n_elems // blk):
        b = bytearray(rng.randbytes(size))
        if ty == Q8_0:
            b[0:2] = f16(rng.uniform(0.002, 0.02))
        elif ty == Q4_K:
            b[0:2] = f16(rng.uniform(0.001, 0.01))
            b[2:4] = f16(rng.uniform(0.0005, 0.005))
        elif ty == Q6_K:
            b[208:210] = f16(rng.uniform(0.0005, 0.005))
        out += b
    return bytes(out)


# ---- reference dequantisers (ggml dequantize_row_*), float64

def dq_q8_0(b):
    out = []
    for o in range(0, len(b), 34):
        d = struct.unpack('<e', b[o:o + 2])[0]
        out += [d * q for q in struct.unpack('<32b', b[o + 2:o + 34])]
    return out


def _sm_k4(j, q):
    if j < 4:
        return q[j] & 63, q[j + 4] & 63
    return (q[j + 4] & 0xF) | ((q[j - 4] >> 6) << 4), (q[j + 4] >> 4) | ((q[j] >> 6) << 4)


def dq_q4_k(b):
    out = []
    for o in range(0, len(b), 144):
        d, dmin = struct.unpack('<ee', b[o:o + 4])
        sc = b[o + 4:o + 16]
        qs = b[o + 16:o + 144]
        for j in range(4):
            s1, m1 = _sm_k4(2 * j, sc)
            s2, m2 = _sm_k4(2 * j + 1, sc)
            q = qs[32 * j:32 * j + 32]
            out += [d * s1 * (x & 0xF) - dmin * m1 for x in q]
            out += [d * s2 * (x >> 4) - dmin * m2 for x in q]
    return out


def dq_q6_k(b):
    out = []
    for o in range(0, len(b), 210):
        ql, qh = b[o:o + 128], b[o + 128:o + 192]
        sc = struct.unpack('<16b', b[o + 192:o + 208])
        d = struct.unpack('<e', b[o + 208:o + 210])[0]
        y = [0.0] * 256
        for n in range(2):
            L, H, S, Y = ql[64 * n:], qh[32 * n:], sc[8 * n:], 128 * n
            for l in range(32):
                i = l // 16
                q1 = ((L[l] & 0xF) | ((H[l] & 3) << 4)) - 32
                q2 = ((L[l + 32] & 0xF) | (((H[l] >> 2) & 3) << 4)) - 32
                q3 = ((L[l] >> 4) | (((H[l] >> 4) & 3) << 4)) - 32
                q4 = ((L[l + 32] >> 4) | (((H[l] >> 6) & 3) << 4)) - 32
                y[Y + l] = d * S[i] * q1
                y[Y + l + 32] = d * S[i + 2] * q2
                y[Y + l + 64] = d * S[i + 4] * q3
                y[Y + l + 96] = d * S[i + 6] * q4
        out += y
    return out


DQ = {Q8_0: dq_q8_0, Q4_K: dq_q4_k, Q6_K: dq_q6_k, F32: lambda b: list(struct.unpack('<{}f'.format(len(b) // 4), b))}


def matrix(ty, dims, data):
    """[k, rows] tensor -> list of rows (float64)."""
    flat = DQ[ty](data)
    k = dims[0]
    return [flat[r * k:(r + 1) * k] for r in range(len(flat) // k)]


def kv(key, vt, v):
    return (key, vt, v)


def make_dense(path, rng, types=(Q4_K, Q8_0, Q6_K), arch='qwen35', tied=True, mtp=True):
    tg, tu, td = types
    meta = [kv('general.architecture', conv.STR, arch), kv('general.alignment', conv.U32, 32),
            kv(arch + '.block_count', conv.U32, 2 if mtp else 1), kv(arch + '.nextn_predict_layers', conv.U32, 1 if mtp else 0),
            kv(arch + '.embedding_length', conv.U32, N_EMBD), kv(arch + '.feed_forward_length', conv.U32, FF),
            kv(arch + '.rope.dimension_sections', conv.ARR, (conv.I32, [11, 11, 10, 0])),
            kv('tokenizer.ggml.pre', conv.STR, 'qwen35')]
    ts = [{'name': 'token_embd.weight', 'dims': [N_EMBD, 16], 'type': Q8_0, 'data': rand_blocks(rng, Q8_0, N_EMBD * 16)},
          {'name': 'output_norm.weight', 'dims': [N_EMBD], 'type': F32, 'data': struct.pack('<{}f'.format(N_EMBD), *[1.0] * N_EMBD)}]
    if not tied:
        ts.append({'name': 'output.weight', 'dims': [N_EMBD, 16], 'type': Q6_K, 'data': rand_blocks(rng, Q6_K, N_EMBD * 16)})
    for il in range(2 if mtp else 1):
        p = 'blk.{}.'.format(il)
        ts += [{'name': p + 'attn_norm.weight', 'dims': [N_EMBD], 'type': F32, 'data': bytes(4 * N_EMBD)},
               {'name': p + 'ffn_gate.weight', 'dims': [N_EMBD, FF], 'type': tg, 'data': rand_blocks(rng, tg, N_EMBD * FF)},
               {'name': p + 'ffn_up.weight', 'dims': [N_EMBD, FF], 'type': tu, 'data': rand_blocks(rng, tu, N_EMBD * FF)},
               {'name': p + 'ffn_down.weight', 'dims': [FF, N_EMBD], 'type': td, 'data': rand_blocks(rng, td, FF * N_EMBD)}]
        if il == 1:
            ts.append({'name': p + 'nextn.eh_proj.weight', 'dims': [2 * N_EMBD, N_EMBD], 'type': Q8_0,
                       'data': rand_blocks(rng, Q8_0, 2 * N_EMBD * N_EMBD)})
    conv.write_gguf(path, meta, ts, 32)


def tensors(path):
    h = conv.read_header(path)
    with open(path, 'rb') as f:
        def get(name):
            t = next(t for t in h['tensors'] if t['name'] == name)
            f.seek(h['data_start'] + t['offset'])
            return t, f.read(t['nbytes'])
        return h, {t['name']: get(t['name']) for t in h['tensors']}


def silu(v):
    return v / (1.0 + math.exp(-v))


def dense_ffn(src, il, x):
    tg, g = src['blk.{}.ffn_gate.weight'.format(il)]
    tu, u = src['blk.{}.ffn_up.weight'.format(il)]
    td, d = src['blk.{}.ffn_down.weight'.format(il)]
    G, U, D = matrix(tg['type'], tg['dims'], g), matrix(tu['type'], tu['dims'], u), matrix(td['type'], td['dims'], d)
    h = [silu(sum(a * b for a, b in zip(gr, x))) * sum(a * b for a, b in zip(ur, x)) for gr, ur in zip(G, U)]
    return [sum(a * b for a, b in zip(dr, h)) for dr in D]


def moe_ffn(out, il, x):
    p = 'blk.{}.'.format(il)
    tr, r = out[p + 'ffn_gate_inp.weight']
    logits = [sum(a * b for a, b in zip(row, x)) for row in matrix(F32, tr['dims'], r)]
    m = max(logits)
    ex = [math.exp(v - m) for v in logits]
    probs = [v / sum(ex) for v in ex]
    used = conv_used(out)
    top = sorted(range(len(probs)), key=lambda i: -probs[i])[:used]  # stable: ties keep index order
    w = [probs[i] / sum(probs[j] for j in top) for i in top]
    ts, s = out[p + 'ffn_gate_inp_shexp.weight']
    sg = 1.0 / (1.0 + math.exp(-sum(a * b for a, b in zip(DQ[F32](s), x))))

    def ffn(gn, un, dn, e=None):
        res = []
        for nm in (gn, un, dn):
            t, b = out[p + nm]
            if e is not None:
                n = len(b) // t['dims'][2]
                b, dims = b[e * n:(e + 1) * n], t['dims'][:2]
            else:
                dims = t['dims']
            res.append(matrix(t['type'], dims, b))
        G, U, D = res
        h = [silu(sum(a * b for a, b in zip(gr, x))) * sum(a * b for a, b in zip(ur, x)) for gr, ur in zip(G, U)]
        return [sum(a * b for a, b in zip(dr, h)) for dr in D]

    y = [v * sg for v in ffn('ffn_gate_shexp.weight', 'ffn_up_shexp.weight', 'ffn_down_shexp.weight')]
    for wi, e in zip(w, top):
        ye = ffn('ffn_gate_exps.weight', 'ffn_up_exps.weight', 'ffn_down_exps.weight', e)
        y = [a + wi * b for a, b in zip(y, ye)]
    return y, w, sg


def conv_used(out):
    return out['blk.0.ffn_gate_exps.weight'][0]['dims'][2] if '_used' not in out else out['_used']


class ConvertTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.rng = random.Random(1234)

    def tearDown(self):
        self.tmp.cleanup()

    def convert(self, **kw):
        src, dst = self.dir / 'dense.gguf', self.dir / 'moe.gguf'
        make_dense(src, self.rng, **kw)
        info = conv.convert(src, dst, n_expert=E)
        return src, dst, info

    def test_metadata_and_layout(self):
        _, dst, info = self.convert()
        self.assertEqual((info['ff_exp'], info['ff_shexp']), (FF_EXP, FF_SH))
        h, out = tensors(dst)
        self.assertEqual(info['n_expert'], N)
        self.assertEqual(conv.kv_get(h, 'general.architecture'), 'qwen35moe')
        self.assertFalse(any(k.startswith('qwen35.') for k, _, _ in h['kv']))
        for k, v in (('expert_count', N), ('expert_used_count', E), ('expert_feed_forward_length', FF_EXP),
                     ('expert_shared_feed_forward_length', FF_SH), ('embedding_length', N_EMBD)):
            self.assertEqual(conv.kv_get(h, 'qwen35moe.' + k), v, k)
        self.assertEqual(conv.kv_get(h, 'qwen35moe.rope.dimension_sections'), (conv.I32, [11, 11, 10, 0]))
        self.assertFalse(any(n.endswith(('ffn_gate.weight', 'ffn_up.weight', 'ffn_down.weight')) for n in out))
        for il in (0, 1):  # target block and MTP block
            p = 'blk.{}.'.format(il)
            self.assertEqual(out[p + 'ffn_gate_exps.weight'][0]['dims'], [N_EMBD, FF_EXP, N])
            self.assertEqual(out[p + 'ffn_down_exps.weight'][0]['dims'], [FF_EXP, N_EMBD, N])
            self.assertEqual(out[p + 'ffn_down_shexp.weight'][0]['dims'], [FF_SH, N_EMBD])
            self.assertEqual(out[p + 'ffn_gate_inp.weight'][1], bytes(4 * N_EMBD * N))
            for nm in ('ffn_gate_exps.weight', 'ffn_up_exps.weight', 'ffn_down_exps.weight'):
                data = out[p + nm][1]
                self.assertEqual(data[len(data) * E // N:], bytes(len(data) - len(data) * E // N), nm)  # zero padding
            self.assertEqual(out[p + 'ffn_gate_inp_shexp.weight'][1], bytes(4 * N_EMBD))
        self.assertIn('blk.1.nextn.eh_proj.weight', out)

    def test_tied_embeddings_get_a_byte_copy_head(self):
        _, dst, _ = self.convert()
        _, out = tensors(dst)
        self.assertEqual(out['output.weight'][1], out['token_embd.weight'][1])
        self.assertEqual(out['output.weight'][0]['type'], out['token_embd.weight'][0]['type'])

    def test_untied_head_is_kept(self):
        src, dst, _ = self.convert(tied=False)
        _, a = tensors(src)
        _, b = tensors(dst)
        self.assertEqual(a['output.weight'][1], b['output.weight'][1])

    def test_gate_up_are_byte_slices_and_down_is_exactly_rescaled(self):
        for types in ((Q4_K, Q8_0, Q6_K), (Q8_0, Q4_K, Q4_K), (Q6_K, Q6_K, Q8_0)):
            with self.subTest(types=[conv.type_name(t) for t in types]):
                src, dst, _ = self.convert(types=types)
                _, a = tensors(src)
                _, b = tensors(dst)
                tg, g = a['blk.0.ffn_gate.weight']
                rb = conv.row_bytes(tg['type'], N_EMBD)
                self.assertEqual(b['blk.0.ffn_gate_shexp.weight'][1], g[:FF_SH * rb])
                self.assertEqual(b['blk.0.ffn_gate_exps.weight'][1][:len(g) - FF_SH * rb], g[FF_SH * rb:])
                td, d = a['blk.0.ffn_down.weight']
                D = matrix(td['type'], td['dims'], d)
                ts, s = b['blk.0.ffn_down_shexp.weight']
                S = matrix(ts['type'], ts['dims'], s)
                te, x = b['blk.0.ffn_down_exps.weight']
                n = len(x) // N
                for r in range(N_EMBD):
                    self.assertEqual(S[r], [2.0 * v for v in D[r][:FF_SH]])
                for e in range(E):
                    X = matrix(te['type'], te['dims'][:2], x[e * n:(e + 1) * n])
                    lo = FF_SH + e * FF_EXP
                    for r in range(0, N_EMBD, 37):
                        self.assertEqual(X[r], [E * v for v in D[r][lo:lo + FF_EXP]])

    def test_rewritten_ffn_equals_dense_ffn(self):
        src, dst, _ = self.convert()
        _, a = tensors(src)
        _, b = tensors(dst)
        b['_used'] = E
        x = [self.rng.uniform(-1, 1) for _ in range(N_EMBD)]
        for il in (0, 1):
            yd = dense_ffn(a, il, x)
            ym, w, sg = moe_ffn(b, il, x)
            self.assertEqual(w, [1.0 / E] * E)
            self.assertEqual(sg, 0.5)
            scale = max(abs(v) for v in yd)
            self.assertLess(max(abs(p - q) for p, q in zip(yd, ym)) / scale, 1e-12)

    def test_negative_control_wrong_scale_is_detected(self):
        src, dst = self.dir / 'dense.gguf', self.dir / 'moe.gguf'
        make_dense(src, self.rng)
        real = conv._log2_exact
        conv._log2_exact = lambda n: real(n) - 1  # experts scaled x4 instead of x8
        try:
            conv.convert(src, dst, n_expert=E)
        finally:
            conv._log2_exact = real
        _, a = tensors(src)
        _, b = tensors(dst)
        b['_used'] = E
        x = [self.rng.uniform(-1, 1) for _ in range(N_EMBD)]
        yd = dense_ffn(a, 0, x)
        ym, _, _ = moe_ffn(b, 0, x)
        scale = max(abs(v) for v in yd)
        self.assertGreater(max(abs(p - q) for p, q in zip(yd, ym)) / scale, 1e-3)

    def test_unpadded_variant_and_bad_counts(self):
        src, dst = self.dir / 'dense.gguf', self.dir / 'moe.gguf'
        make_dense(src, self.rng)
        info = conv.convert(src, dst, n_expert=E, n_total=E)  # llama.cpp accepts 8 of 8; oxe does not
        self.assertEqual((info['n_expert'], info['n_expert_used']), (E, E))
        h, out = tensors(dst)
        self.assertEqual(conv.kv_get(h, 'qwen35moe.expert_count'), E)
        out['_used'] = E
        x = [self.rng.uniform(-1, 1) for _ in range(N_EMBD)]
        yd, (ym, w, _) = dense_ffn(tensors(src)[1], 0, x), moe_ffn(out, 0, x)
        self.assertEqual(w, [1.0 / E] * E)
        self.assertLess(max(abs(p - q) for p, q in zip(yd, ym)) / max(map(abs, yd)), 1e-12)
        for bad in (12, 4):
            with self.assertRaises(conv.ConvertError):
                conv.convert(src, dst, n_expert=E, n_total=bad)

    def test_refuses_types_without_exact_rescale(self):
        src = self.dir / 'dense.gguf'
        make_dense(src, self.rng, types=(Q4_K, Q4_K, IQ4_XS))
        with self.assertRaisesRegex(conv.ConvertError, 'IQ4_XS'):
            conv.convert(src, self.dir / 'moe.gguf', n_expert=E)
        self.assertFalse((self.dir / 'moe.gguf').exists())

    def test_refuses_non_qwen35_source(self):
        src = self.dir / 'dense.gguf'
        make_dense(src, self.rng, arch='qwen35moe')
        with self.assertRaisesRegex(conv.ConvertError, 'qwen35'):
            conv.convert(src, self.dir / 'moe.gguf', n_expert=E)

    def test_f16_scale_overflow_is_refused(self):
        blk = bytearray(34)
        blk[0:2] = f16(60000.0)
        with self.assertRaisesRegex(conv.ConvertError, 'overflow'):
            conv.scale_blocks(Q8_0, bytes(blk), 3)
        blk[0:2] = f16(2.0 ** -24)  # smallest subnormal: x8 stays exact
        out = conv.scale_blocks(Q8_0, bytes(blk), 3)
        self.assertEqual(struct.unpack('<e', out[0:2])[0], 2.0 ** -21)

    def test_bad_split_is_refused(self):
        src = self.dir / 'dense.gguf'
        make_dense(src, self.rng)
        with self.assertRaisesRegex(conv.ConvertError, 'multiples of 256'):
            conv.convert(src, self.dir / 'moe.gguf', n_expert=E, ff_shexp=FF - E * 128)
        with self.assertRaisesRegex(conv.ConvertError, 'power of two'):
            conv.convert(src, self.dir / 'moe.gguf', n_expert=6, ff_shexp=FF - 6 * 256)

    def test_inspect_reports_oxe_support(self):
        src = self.dir / 'dense.gguf'
        make_dense(src, self.rng, types=(Q4_K, Q4_K, IQ4_XS))
        r = conv.inspect(src)
        self.assertTrue(r['tied_embeddings'])
        self.assertIn('blk.0.ffn_down.weight IQ4_XS', r['problems'])
        self.assertEqual(r['by_type']['Q4_K'][0], 4)


if __name__ == '__main__':
    unittest.main()
