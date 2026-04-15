//! GGUF → ModelWeights mapping. Builds non-layer (`WeightsData`) and per-layer
//! (`LayerData` / `LayerDataVec`) structures from a parsed GGUF model.

use libfolk::println;
use libtensor::gguf::GgufModel;
use libtensor::transformer::{ModelConfig, OutputQuant};

/// Non-layer weight data extracted from GGUF.
/// All slices point into mmap'd data ('static lifetime).
pub struct WeightsData {
    pub token_embed: &'static [u8],
    pub final_norm: &'static [u8],
    pub output_weight: &'static [u8],
    /// Quantization format of output_weight
    pub output_quant: OutputQuant,
}

/// Per-layer weight data. All slices point into mmap'd data.
pub struct LayerData {
    pub attn_norm: &'static [u8],
    pub wq: &'static [u8],
    pub wk: &'static [u8],
    pub wv: &'static [u8],
    pub q_norm: &'static [u8],  // QK-norm (Qwen3): [head_dim] f32, empty if absent
    pub k_norm: &'static [u8],  // QK-norm (Qwen3): [head_dim] f32, empty if absent
    pub wo: &'static [u8],
    pub ffn_norm: &'static [u8],
    pub w_gate: &'static [u8],
    pub w_up: &'static [u8],
    pub w_down: &'static [u8],
    pub w_down_quant: OutputQuant,
}

/// Fixed-capacity Vec for layer data (avoids heap allocation).
pub struct LayerDataVec {
    /// Raw storage for up to 64 LayerData values
    storage: [core::mem::MaybeUninit<LayerData>; 64],
    count: usize,
}

impl LayerDataVec {
    pub fn new() -> Self {
        Self {
            // MaybeUninit doesn't require initialization
            storage: unsafe { core::mem::MaybeUninit::uninit().assume_init() },
            count: 0,
        }
    }

    pub fn push(&mut self, data: LayerData) -> bool {
        if self.count >= 64 { return false; }
        self.storage[self.count] = core::mem::MaybeUninit::new(data);
        self.count += 1;
        true
    }

    pub fn get(&self, idx: usize) -> &LayerData {
        debug_assert!(idx < self.count);
        unsafe { self.storage[idx].assume_init_ref() }
    }
}

/// Build tensor name like "blk.5.attn_q.weight" into a stack buffer.
fn tensor_name<'a>(buf: &'a mut [u8; 64], prefix: &str, layer: usize, suffix: &str) -> &'a str {
    let mut pos = 0;
    for b in prefix.bytes() {
        if pos >= 63 { break; }
        buf[pos] = b;
        pos += 1;
    }
    // Write layer number
    if layer >= 100 {
        buf[pos] = b'0' + (layer / 100) as u8; pos += 1;
        buf[pos] = b'0' + ((layer / 10) % 10) as u8; pos += 1;
        buf[pos] = b'0' + (layer % 10) as u8; pos += 1;
    } else if layer >= 10 {
        buf[pos] = b'0' + (layer / 10) as u8; pos += 1;
        buf[pos] = b'0' + (layer % 10) as u8; pos += 1;
    } else {
        buf[pos] = b'0' + layer as u8; pos += 1;
    }
    for b in suffix.bytes() {
        if pos >= 63 { break; }
        buf[pos] = b;
        pos += 1;
    }
    core::str::from_utf8(&buf[..pos]).unwrap_or("")
}

/// Build ModelWeights from parsed GGUF model.
///
/// The model's tensor data references point into mmap'd memory which lives
/// for the process lifetime, so we transmute to 'static.
pub fn build_model_weights(model: &GgufModel)
    -> Option<(ModelConfig, WeightsData, LayerDataVec)>
{
    let meta = &model.metadata;

    let config = ModelConfig {
        n_layers: meta.n_layers as usize,
        n_heads: meta.n_heads as usize,
        n_kv_heads: meta.n_kv_heads as usize,
        embed_dim: meta.embedding_dim as usize,
        head_dim: meta.head_dim as usize,
        intermediate_size: meta.intermediate_size as usize,
        vocab_size: meta.vocab_size as usize,
        max_seq_len: meta.context_length as usize,
        rope_base: meta.rope_base,
        rms_norm_eps: meta.rms_norm_eps,
    };

    // Find global tensors
    let token_embed = model.tensor("token_embd.weight")?;
    let final_norm = model.tensor("output_norm.weight")?;

    // output.weight may be tied to token_embd.weight
    let output_weight = model.tensor("output.weight")
        .unwrap_or(token_embed);

    println!("[INFERENCE]   token_embd: {:?} {:?}", token_embed.shape, token_embed.dtype);
    println!("[INFERENCE]   output_norm: {:?} {:?}", final_norm.shape, final_norm.dtype);
    println!("[INFERENCE]   output: {:?} {:?} {}", output_weight.shape, output_weight.dtype,
        if model.tensor("output.weight").is_none() { "(tied)" } else { "" });

    // Detect output weight quantization format
    let output_quant = match output_weight.dtype {
        libtensor::gguf::GgufDtype::Q8_0 => OutputQuant::Q8_0,
        libtensor::gguf::GgufDtype::Q6K => OutputQuant::Q6_K,
        _ => OutputQuant::Q4_0,
    };

    // Safety: tensor data points into mmap'd memory that lives for the entire process
    let weights_data = WeightsData {
        token_embed: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(token_embed.data) },
        final_norm: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(final_norm.data) },
        output_weight: unsafe { core::mem::transmute::<&[u8], &'static [u8]>(output_weight.data) },
        output_quant,
    };

    // Build per-layer weights
    let mut layer_data = LayerDataVec::new();
    for i in 0..config.n_layers {
        let mut buf = [0u8; 64];

        let attn_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_norm.weight"))?;
        let wq = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_q.weight"))?;
        let wk = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_k.weight"))?;
        let wv = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_v.weight"))?;
        // QK-norm is optional (Qwen3 has it, SmolLM2 doesn't)
        let q_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_q_norm.weight"));
        let k_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_k_norm.weight"));
        let wo = model.tensor(tensor_name(&mut buf, "blk.", i, ".attn_output.weight"))?;
        let ffn_norm = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_norm.weight"))?;
        let w_gate = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_gate.weight"))?;
        let w_up = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_up.weight"))?;
        let w_down = model.tensor(tensor_name(&mut buf, "blk.", i, ".ffn_down.weight"))?;

        if i <= 1 {
            // Log first 4 bytes of wq data (Q4_0 scale + first nibble)
            let d = wq.data;
            println!("[INFERENCE]   blk.{}.attn_q: {:?} {:?} len={} first=[{:02X},{:02X},{:02X},{:02X}]",
                i, wq.shape, wq.dtype, d.len(), d[0], d[1], d[2], d[3]);
        }

        // Safety: tensor data points into mmap'd memory (process lifetime)
        unsafe {
            let empty: &'static [u8] = &[];
            // Determine w_down quantization from GGUF dtype
            let w_down_quant = match w_down.dtype {
                libtensor::gguf::GgufDtype::Q4_1 => OutputQuant::Q4_1,
                libtensor::gguf::GgufDtype::Q8_0 => OutputQuant::Q8_0,
                _ => OutputQuant::Q4_0,
            };
            if i == 0 {
                println!("[INFERENCE]   blk.0.ffn_down: {:?} {:?} → {:?}",
                    w_down.shape, w_down.dtype, w_down_quant);
            }
            layer_data.push(LayerData {
                attn_norm: core::mem::transmute::<&[u8], &'static [u8]>(attn_norm.data),
                wq: core::mem::transmute::<&[u8], &'static [u8]>(wq.data),
                wk: core::mem::transmute::<&[u8], &'static [u8]>(wk.data),
                wv: core::mem::transmute::<&[u8], &'static [u8]>(wv.data),
                q_norm: q_norm.map_or(empty, |t| core::mem::transmute::<&[u8], &'static [u8]>(t.data)),
                k_norm: k_norm.map_or(empty, |t| core::mem::transmute::<&[u8], &'static [u8]>(t.data)),
                wo: core::mem::transmute::<&[u8], &'static [u8]>(wo.data),
                ffn_norm: core::mem::transmute::<&[u8], &'static [u8]>(ffn_norm.data),
                w_gate: core::mem::transmute::<&[u8], &'static [u8]>(w_gate.data),
                w_up: core::mem::transmute::<&[u8], &'static [u8]>(w_up.data),
                w_down: core::mem::transmute::<&[u8], &'static [u8]>(w_down.data),
                w_down_quant,
            });
        }
    }

    println!("[INFERENCE]   All {} layers mapped successfully", config.n_layers);

    Some((config, weights_data, layer_data))
}
