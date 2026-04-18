"""
Self-reference for the single-head attention WASM crate.

Implements the SAME math as src/lib.rs::attention(), including the
SAME exp_approx polynomial in the SAME evaluation order, so the
host-side checksum can be compared bit-by-bit against what the JIT
produces on the Pi. (For a high-fidelity sanity check, use
reference_torch.py to compare against PyTorch's exact softmax.)

Key points:
  - All math runs in float32 (numpy.float32) to match WASM f32.
  - exp_approx uses k=5 range reduction (divide by 32, square 5
    times) with a degree-4 Taylor polynomial — identical to lib.rs.
  - Memory layout matches lib.rs offsets so we can reuse the same
    weight buffer when generating test data.

Usage:
    python reference.py                # print expected checksum
    python reference.py --dump-weights # write inputs/weights.bin
"""

import argparse
import struct
import numpy as np

S = 4
D = 4
INV_SQRT_D = np.float32(0.5)        # 1/sqrt(4)

# ── Test inputs / weights — same as the runner will send ──────────
INPUTS = np.array([
    [ 1.0,  0.5, -0.3,  0.8],
    [ 0.2, -0.7,  0.4,  0.1],
    [-0.5,  0.6,  0.9, -0.2],
    [ 0.3,  0.1, -0.4,  0.7],
], dtype=np.float32)

WQ = np.array([
    [ 0.5, -0.3,  0.8,  0.1],
    [-0.2,  0.7,  0.4, -0.6],
    [ 0.9, -0.1, -0.5,  0.3],
    [ 0.1,  0.4,  0.6, -0.2],
], dtype=np.float32)

WK = np.array([
    [ 0.3, -0.2,  0.5,  0.1],
    [-0.1,  0.4,  0.2, -0.3],
    [ 0.2, -0.5,  0.1,  0.4],
    [ 0.4,  0.1, -0.2,  0.3],
], dtype=np.float32)

WV = np.array([
    [ 0.5,  0.2, -0.3,  0.4],
    [-0.1,  0.3,  0.5,  0.2],
    [ 0.4, -0.2,  0.1, -0.5],
    [ 0.2,  0.5, -0.4,  0.3],
], dtype=np.float32)


def exp_approx(x):
    """Mirror of lib.rs::exp_approx — bit-exact in f32."""
    x = np.float32(x)
    if x < np.float32(-16.0):
        return np.float32(0.0)
    y  = x * np.float32(0.03125)
    y2 = y  * y
    y3 = y2 * y
    y4 = y2 * y2
    p = (np.float32(1.0)
         + y
         + y2 * np.float32(0.5)
         + y3 * np.float32(0.16666667)
         + y4 * np.float32(0.04166667))
    p2  = p   * p
    p4  = p2  * p2
    p8  = p4  * p4
    p16 = p8  * p8
    return p16 * p16


def attention():
    """Single-head attention matching lib.rs exactly.

    Uses scalar Python loops (NOT numpy vectorisation) so the
    f32 accumulation order matches the Rust loop order.
    """
    # ── Phase 1: fused Q/K/V projections (one X load per inner k) ──
    Q = np.zeros((S, D), dtype=np.float32)
    K = np.zeros((S, D), dtype=np.float32)
    V = np.zeros((S, D), dtype=np.float32)
    for s in range(S):
        for d_out in range(D):
            q_acc = np.float32(0.0)
            k_acc = np.float32(0.0)
            v_acc = np.float32(0.0)
            for k in range(D):
                x = INPUTS[s, k]
                q_acc = q_acc + x * WQ[k, d_out]
                k_acc = k_acc + x * WK[k, d_out]
                v_acc = v_acc + x * WV[k, d_out]
            Q[s, d_out] = q_acc
            K[s, d_out] = k_acc
            V[s, d_out] = v_acc

    # ── Phase 2: Scores = Q · K^T · (1/√D) ────────────────────────
    Scores = np.zeros((S, S), dtype=np.float32)
    for i in range(S):
        for j in range(S):
            acc = np.float32(0.0)
            for d in range(D):
                acc = acc + Q[i, d] * K[j, d]
            Scores[i, j] = acc * INV_SQRT_D

    # ── Phase 3+4+5: stable softmax row-by-row ────────────────────
    Probs = np.zeros((S, S), dtype=np.float32)
    for row in range(S):
        # Row max
        row_max = Scores[row, 0]
        for j in range(1, S):
            if Scores[row, j] > row_max:
                row_max = Scores[row, j]
        # Shifted exps
        sum_e = np.float32(0.0)
        for j in range(S):
            e = exp_approx(Scores[row, j] - row_max)
            Probs[row, j] = e
            sum_e = sum_e + e
        # Normalise (1/sum applied to each cell)
        inv = np.float32(1.0) / sum_e
        for j in range(S):
            Probs[row, j] = Probs[row, j] * inv

    # ── Phase 6: Output = Probs · V ───────────────────────────────
    Output = np.zeros((S, D), dtype=np.float32)
    for i in range(S):
        for d in range(D):
            acc = np.float32(0.0)
            for k in range(S):
                acc = acc + Probs[i, k] * V[k, d]
            Output[i, d] = acc

    # ── Checksum: Σ Output × 1000, truncated to i32 ───────────────
    s = np.float32(0.0)
    for cell in Output.flatten():
        s = s + cell
    return Output, Probs, Scores, int(np.trunc(s * np.float32(1000.0)))


def dump_weights(path):
    """Pack inputs+weights as the runner sends them via DATA frame."""
    buf = bytearray()
    for arr in (INPUTS, WQ, WK, WV):
        for v in arr.flatten():
            buf += struct.pack('<f', float(v))
    with open(path, 'wb') as f:
        f.write(buf)
    print(f"wrote {len(buf)} bytes to {path}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--dump-weights", metavar="PATH",
                    help="Write the host-side weight buffer to a file")
    ap.add_argument("--verbose", "-v", action="store_true")
    args = ap.parse_args()

    if args.dump_weights:
        dump_weights(args.dump_weights)

    Output, Probs, Scores, checksum = attention()

    if args.verbose:
        print("Scores:")
        print(Scores)
        print("Probs (softmax):")
        print(Probs)
        print("Output:")
        print(Output)
        print(f"Sum of output: {Output.sum():.6f}")

    print(f"Expected checksum (exit code): {checksum}")
