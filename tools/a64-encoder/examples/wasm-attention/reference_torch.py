"""
Compare our exp-approximation softmax to PyTorch's exact softmax.

The on-Pi result (2239) matches reference.py byte-for-byte because
both use the same polynomial exp. This script answers the harder
question: "how close is our cheap exp to the real one?"

If the gap is < 1% on the checksum we know our polynomial is good
enough for production attention. If it's larger, we know the
polynomial degree or range-reduction k needs to be raised.
"""

import numpy as np
import torch
import torch.nn.functional as F

import reference

S, D = reference.S, reference.D
INV = float(reference.INV_SQRT_D)

X  = torch.from_numpy(reference.INPUTS)   # [S, D]
Wq = torch.from_numpy(reference.WQ)
Wk = torch.from_numpy(reference.WK)
Wv = torch.from_numpy(reference.WV)

with torch.no_grad():
    Q = X @ Wq
    K = X @ Wk
    V = X @ Wv
    scores = (Q @ K.T) * INV
    probs  = F.softmax(scores, dim=-1)
    out    = probs @ V

torch_checksum = int(np.trunc(float(out.sum()) * 1000.0))

# Our reference (with polynomial exp)
_, _, _, ours_checksum = reference.attention()

diff = ours_checksum - torch_checksum
rel  = abs(diff) / max(abs(torch_checksum), 1) * 100

print(f"PyTorch (exact softmax):    {torch_checksum}")
print(f"Folkering (poly exp):       {ours_checksum}")
print(f"Difference:                 {diff:+d}  ({rel:.3f} % of PyTorch)")

# Cell-by-cell L∞ distance between the two output matrices.
torch_out = out.numpy()
ours_out, _, _, _ = reference.attention()
linf = float(np.max(np.abs(torch_out - ours_out)))
l1   = float(np.mean(np.abs(torch_out - ours_out)))
print(f"Linf error per cell:        {linf:.6f}")
print(f"Mean abs error per cell:    {l1:.6f}")
