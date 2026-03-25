//! KV-Cache with Sink-Token Eviction (ULTRA 7, 14, 21)
//!
//! StreamingLLM-style attention cache:
//! - 4 "sink" tokens always retained (attention anchors)
//! - Rolling window of recent tokens (ring buffer, modulo-addressed)
//! - Q8_0 quantized keys/values for memory efficiency
//! - Zero-memmove eviction via overwrite at ring position

// Quantized KV-cache storage is planned for ULTRA 7 phase 2

/// Number of attention sink tokens to always retain.
pub const NUM_SINK_TOKENS: usize = 4;

/// KV-Cache for one transformer layer.
///
/// Memory layout:
/// - Sink tokens: [NUM_SINK_TOKENS × head_dim] stored separately, never evicted
/// - Ring buffer: [window_size × head_dim] circular buffer for recent tokens
///
/// Total active context = NUM_SINK_TOKENS + min(tokens_seen, window_size)
pub struct KvCache {
    /// Key cache for sink tokens: Q8_0 blocks [NUM_SINK_TOKENS × q8_vec_bytes]
    sink_keys: *mut u8,
    /// Value cache for sink tokens: Q8_0 blocks
    sink_values: *mut u8,

    /// Key ring buffer: Q8_0 blocks [window_size × q8_vec_bytes]
    ring_keys: *mut u8,
    /// Value ring buffer: Q8_0 blocks
    ring_values: *mut u8,

    /// Absolute position stored with each sink token (for RoPE)
    sink_positions: [u32; NUM_SINK_TOKENS],
    /// Absolute position for each ring buffer slot (for RoPE)
    ring_positions: *mut u32,

    /// Number of attention heads
    n_heads: usize,
    /// Dimension per head
    head_dim: usize,
    /// Q8_0 bytes per vector (n_heads * head_dim / 32 * 34)
    q8_vec_bytes: usize,
    /// Size of the rolling window (power of 2 for fast modulo)
    window_size: usize,
    /// Mask for modulo addressing: window_size - 1
    window_mask: usize,

    /// Total tokens seen (monotonically increasing)
    tokens_seen: usize,
    /// Next write position in ring buffer (tokens_seen % window_size)
    ring_pos: usize,
}

unsafe impl Send for KvCache {}
unsafe impl Sync for KvCache {}

impl KvCache {
    /// Create a new KV-cache. `window_size` MUST be a power of 2.
    ///
    /// Memory is allocated via the provided base pointers.
    /// Total memory per cache: 2 × (NUM_SINK_TOKENS + window_size) × n_heads × head_dim × 4 bytes
    ///
    /// # Safety
    /// `base` must point to a valid region of at least `Self::required_bytes()` bytes.
    pub unsafe fn new(
        base: *mut u8,
        n_heads: usize,
        head_dim: usize,
        window_size: usize,
    ) -> Self {
        debug_assert!(window_size.is_power_of_two(), "window_size must be power of 2");

        let q8_vec = Self::q8_vec_bytes(n_heads, head_dim);
        let sink_q8_size = NUM_SINK_TOKENS * q8_vec;
        let ring_q8_size = window_size * q8_vec;

        let mut offset = 0usize;

        let sink_keys = base.add(offset);
        offset += sink_q8_size;

        let sink_values = base.add(offset);
        offset += sink_q8_size;

        let ring_keys = base.add(offset);
        offset += ring_q8_size;

        let ring_values = base.add(offset);
        offset += ring_q8_size;

        let ring_positions = base.add(offset) as *mut u32;
        offset += window_size * 4;

        // Zero-initialize all buffers
        core::ptr::write_bytes(base, 0, offset);

        Self {
            sink_keys,
            sink_values,
            ring_keys,
            ring_values,
            sink_positions: [0; NUM_SINK_TOKENS],
            ring_positions,
            n_heads,
            head_dim,
            q8_vec_bytes: q8_vec,
            window_size,
            window_mask: window_size - 1,
            tokens_seen: 0,
            ring_pos: 0,
        }
    }

    /// Q8_0 bytes per KV vector (n_heads × head_dim values → Q8_0 blocks)
    fn q8_vec_bytes(n_heads: usize, head_dim: usize) -> usize {
        let n_values = n_heads * head_dim;
        let n_blocks = (n_values + 31) / 32; // ceiling division
        n_blocks * crate::quantize::Q8_0_BLOCK_SIZE
    }

    /// Required bytes for this KV-cache configuration (Q8_0 quantized).
    pub fn required_bytes(n_heads: usize, head_dim: usize, window_size: usize) -> usize {
        let q8_vec = Self::q8_vec_bytes(n_heads, head_dim);
        let sink_size = NUM_SINK_TOKENS * q8_vec;
        let ring_size = window_size * q8_vec;
        let ring_pos_size = window_size * 4; // u32 per ring slot
        2 * (sink_size + ring_size) + ring_pos_size
    }

    /// Store key/value for the current token at absolute position `abs_pos`.
    ///
    /// `key` and `value` are [n_heads × head_dim] f32 vectors.
    /// `abs_pos` is the absolute sequence position (used for RoPE alignment).
    pub fn store(&mut self, key: &[f32], value: &[f32], abs_pos: usize) {
        let vec_size = self.n_heads * self.head_dim;
        debug_assert!(key.len() >= vec_size);
        debug_assert!(value.len() >= vec_size);

        if self.tokens_seen < NUM_SINK_TOKENS {
            // Quantize and store in sink slots
            let offset = self.tokens_seen * self.q8_vec_bytes;
            let k_dst = unsafe { core::slice::from_raw_parts_mut(self.sink_keys.add(offset), self.q8_vec_bytes) };
            let v_dst = unsafe { core::slice::from_raw_parts_mut(self.sink_values.add(offset), self.q8_vec_bytes) };
            crate::quantize::quantize_f32_to_q8_0(&key[..vec_size], k_dst);
            crate::quantize::quantize_f32_to_q8_0(&value[..vec_size], v_dst);
            self.sink_positions[self.tokens_seen] = abs_pos as u32;
        } else {
            // Quantize and store in ring buffer
            let offset = self.ring_pos * self.q8_vec_bytes;
            let k_dst = unsafe { core::slice::from_raw_parts_mut(self.ring_keys.add(offset), self.q8_vec_bytes) };
            let v_dst = unsafe { core::slice::from_raw_parts_mut(self.ring_values.add(offset), self.q8_vec_bytes) };
            crate::quantize::quantize_f32_to_q8_0(&key[..vec_size], k_dst);
            crate::quantize::quantize_f32_to_q8_0(&value[..vec_size], v_dst);
            unsafe { *self.ring_positions.add(self.ring_pos) = abs_pos as u32; }
            self.ring_pos = (self.ring_pos + 1) & self.window_mask;
        }

        self.tokens_seen += 1;
    }

    /// Get the total number of valid KV entries for attention.
    pub fn active_length(&self) -> usize {
        if self.tokens_seen <= NUM_SINK_TOKENS {
            self.tokens_seen
        } else {
            let ring_used = core::cmp::min(
                self.tokens_seen - NUM_SINK_TOKENS,
                self.window_size,
            );
            NUM_SINK_TOKENS + ring_used
        }
    }

    /// Dequantize key vector for logical position `pos` into `out`.
    ///
    /// Positions 0..NUM_SINK_TOKENS → sink slots.
    /// Positions NUM_SINK_TOKENS.. → ring buffer (modulo-addressed).
    /// `out` must be at least `head_dim` elements.
    pub fn get_key(&self, pos: usize, head: usize, out: &mut [f32]) {
        self.dequant_vec(pos, head, true, out);
    }

    /// Dequantize value vector for logical position `pos` into `out`.
    pub fn get_value(&self, pos: usize, head: usize, out: &mut [f32]) {
        self.dequant_vec(pos, head, false, out);
    }

    /// Internal: dequantize one head's K or V vector from Q8_0 storage.
    fn dequant_vec(&self, pos: usize, head: usize, is_key: bool, out: &mut [f32]) {
        let slot_idx;
        let storage: *mut u8;

        if pos < NUM_SINK_TOKENS {
            slot_idx = pos;
            storage = if is_key { self.sink_keys } else { self.sink_values };
        } else {
            slot_idx = self.ring_logical_to_physical(pos - NUM_SINK_TOKENS);
            storage = if is_key { self.ring_keys } else { self.ring_values };
        }

        // Each slot stores n_heads × head_dim values as Q8_0 blocks.
        // We need the slice for just one head: head_dim values starting at head*head_dim.
        let slot_offset = slot_idx * self.q8_vec_bytes;
        let head_values_offset = head * self.head_dim;

        // Q8_0 blocks cover 32 values each. Find the starting block for this head.
        let block_start = head_values_offset / 32;
        let block_count = (self.head_dim + 31) / 32;

        for b in 0..block_count {
            let block_byte_offset = slot_offset + (block_start + b) * crate::quantize::Q8_0_BLOCK_SIZE;
            let block_data = unsafe {
                core::slice::from_raw_parts(storage.add(block_byte_offset), crate::quantize::Q8_0_BLOCK_SIZE)
            };
            let out_start = b * 32;
            let out_end = (out_start + 32).min(self.head_dim);
            let out_slice = &mut out[out_start..out_end];
            crate::quantize::dequantize_q8_0_block(block_data, out_slice);
        }
    }

    /// Convert ring-local logical index to physical ring buffer index.
    ///
    /// The ring buffer is written sequentially with wrap-around.
    /// Logical index 0 = oldest entry in the ring.
    fn ring_logical_to_physical(&self, logical: usize) -> usize {
        if self.tokens_seen - NUM_SINK_TOKENS <= self.window_size {
            // Ring not yet full — logical == physical
            logical
        } else {
            // Ring is full — oldest entry is at ring_pos
            (self.ring_pos + logical) & self.window_mask
        }
    }

    /// Get the absolute position for logical KV entry `pos`.
    /// Used by the transformer to retrieve the correct RoPE position.
    pub fn get_position(&self, pos: usize) -> usize {
        if pos < NUM_SINK_TOKENS {
            self.sink_positions[pos] as usize
        } else {
            let ring_idx = self.ring_logical_to_physical(pos - NUM_SINK_TOKENS);
            unsafe { *self.ring_positions.add(ring_idx) as usize }
        }
    }

    /// Reset the cache (start fresh conversation).
    pub fn reset(&mut self) {
        self.tokens_seen = 0;
        self.ring_pos = 0;
    }

    /// Number of tokens seen since last reset.
    pub fn tokens_seen(&self) -> usize {
        self.tokens_seen
    }
}

/// Multi-layer KV-cache manager.
///
/// Owns the backing memory for all layers' KV-caches.
pub struct KvCacheManager {
    /// Per-layer caches
    layers: *mut KvCache,
    n_layers: usize,
    /// Backing memory (from mmap)
    _backing: *mut u8,
    _backing_size: usize,
}

unsafe impl Send for KvCacheManager {}
unsafe impl Sync for KvCacheManager {}

impl KvCacheManager {
    /// Allocate and initialize KV-caches for all layers.
    ///
    /// # Safety
    /// Uses mmap for backing memory.
    pub unsafe fn new(
        n_layers: usize,
        n_heads: usize,
        head_dim: usize,
        window_size: usize,
    ) -> Result<Self, ()> {
        use libfolk::sys::memory::{mmap_at, PROT_READ, PROT_WRITE};

        let per_layer = KvCache::required_bytes(n_heads, head_dim, window_size);
        let total_kv = n_layers * per_layer;
        let cache_structs_size = n_layers * core::mem::size_of::<KvCache>();
        let total = total_kv + cache_structs_size;

        // Allocate in 16MB chunks (kernel mmap limit)
        const KV_MMAP_BASE: usize = 0x2_0000_0000; // 8GB offset, separate from model
        const MMAP_CHUNK: usize = 16 * 1024 * 1024;
        let mut mapped = 0usize;
        while mapped < total {
            let chunk = (total - mapped).min(MMAP_CHUNK);
            let addr = KV_MMAP_BASE + mapped;
            if mmap_at(addr, chunk, PROT_READ | PROT_WRITE).is_err() {
                return Err(());
            }
            mapped += chunk;
        }
        let backing = KV_MMAP_BASE as *mut u8;

        // Place KvCache structs at the beginning
        let layers = backing as *mut KvCache;
        let data_base = backing.add(cache_structs_size);

        for layer in 0..n_layers {
            let layer_base = data_base.add(layer * per_layer);
            let cache = KvCache::new(layer_base, n_heads, head_dim, window_size);
            core::ptr::write(layers.add(layer), cache);
        }

        Ok(Self {
            layers,
            n_layers,
            _backing: backing,
            _backing_size: total,
        })
    }

    /// Get mutable reference to a layer's KV-cache.
    pub fn layer_mut(&mut self, layer: usize) -> &mut KvCache {
        debug_assert!(layer < self.n_layers);
        unsafe { &mut *self.layers.add(layer) }
    }

    /// Get reference to a layer's KV-cache.
    pub fn layer(&self, layer: usize) -> &KvCache {
        debug_assert!(layer < self.n_layers);
        unsafe { &*self.layers.add(layer) }
    }

    /// Reset all layers.
    pub fn reset(&mut self) {
        for i in 0..self.n_layers {
            self.layer_mut(i).reset();
        }
    }

    /// Number of layers.
    pub fn n_layers(&self) -> usize {
        self.n_layers
    }
}
