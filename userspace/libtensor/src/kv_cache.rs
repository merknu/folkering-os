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
    /// Key cache for sink tokens: [NUM_SINK_TOKENS × n_heads × head_dim] as f32
    sink_keys: *mut f32,
    /// Value cache for sink tokens
    sink_values: *mut f32,

    /// Key ring buffer: [window_size × n_heads × head_dim] as f32
    ring_keys: *mut f32,
    /// Value ring buffer
    ring_values: *mut f32,

    /// Number of attention heads
    n_heads: usize,
    /// Dimension per head
    head_dim: usize,
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

        let sink_size = NUM_SINK_TOKENS * n_heads * head_dim;
        let ring_size = window_size * n_heads * head_dim;

        let mut offset = 0usize;

        let sink_keys = base.add(offset) as *mut f32;
        offset += sink_size * 4;

        let sink_values = base.add(offset) as *mut f32;
        offset += sink_size * 4;

        let ring_keys = base.add(offset) as *mut f32;
        offset += ring_size * 4;

        let ring_values = base.add(offset) as *mut f32;
        // offset += ring_size * 4;

        // Zero-initialize
        core::ptr::write_bytes(base, 0, offset + ring_size * 4);

        Self {
            sink_keys,
            sink_values,
            ring_keys,
            ring_values,
            n_heads,
            head_dim,
            window_size,
            window_mask: window_size - 1,
            tokens_seen: 0,
            ring_pos: 0,
        }
    }

    /// Required bytes for this KV-cache configuration.
    pub fn required_bytes(n_heads: usize, head_dim: usize, window_size: usize) -> usize {
        let sink_size = NUM_SINK_TOKENS * n_heads * head_dim * 4; // f32
        let ring_size = window_size * n_heads * head_dim * 4;
        2 * (sink_size + ring_size) // keys + values
    }

    /// Store key/value for the current token position.
    ///
    /// `key` and `value` are [n_heads × head_dim] f32 vectors.
    pub fn store(&mut self, key: &[f32], value: &[f32]) {
        let vec_size = self.n_heads * self.head_dim;
        debug_assert!(key.len() >= vec_size);
        debug_assert!(value.len() >= vec_size);

        if self.tokens_seen < NUM_SINK_TOKENS {
            // Store in sink slots
            let offset = self.tokens_seen * vec_size;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    key.as_ptr(),
                    self.sink_keys.add(offset),
                    vec_size,
                );
                core::ptr::copy_nonoverlapping(
                    value.as_ptr(),
                    self.sink_values.add(offset),
                    vec_size,
                );
            }
        } else {
            // Store in ring buffer (overwrites oldest if full)
            let ring_offset = self.ring_pos * vec_size;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    key.as_ptr(),
                    self.ring_keys.add(ring_offset),
                    vec_size,
                );
                core::ptr::copy_nonoverlapping(
                    value.as_ptr(),
                    self.ring_values.add(ring_offset),
                    vec_size,
                );
            }
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

    /// Get key vector for logical position `pos` (0-indexed over active entries).
    ///
    /// Positions 0..NUM_SINK_TOKENS → sink slots.
    /// Positions NUM_SINK_TOKENS.. → ring buffer (modulo-addressed).
    pub fn get_key(&self, pos: usize, head: usize) -> &[f32] {
        let head_offset = head * self.head_dim;

        if pos < NUM_SINK_TOKENS {
            let offset = pos * self.n_heads * self.head_dim + head_offset;
            unsafe { core::slice::from_raw_parts(self.sink_keys.add(offset), self.head_dim) }
        } else {
            let ring_idx = self.ring_logical_to_physical(pos - NUM_SINK_TOKENS);
            let offset = ring_idx * self.n_heads * self.head_dim + head_offset;
            unsafe { core::slice::from_raw_parts(self.ring_keys.add(offset), self.head_dim) }
        }
    }

    /// Get value vector for logical position `pos`.
    pub fn get_value(&self, pos: usize, head: usize) -> &[f32] {
        let head_offset = head * self.head_dim;

        if pos < NUM_SINK_TOKENS {
            let offset = pos * self.n_heads * self.head_dim + head_offset;
            unsafe { core::slice::from_raw_parts(self.sink_values.add(offset), self.head_dim) }
        } else {
            let ring_idx = self.ring_logical_to_physical(pos - NUM_SINK_TOKENS);
            let offset = ring_idx * self.n_heads * self.head_dim + head_offset;
            unsafe { core::slice::from_raw_parts(self.ring_values.add(offset), self.head_dim) }
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
        use libfolk::sys::memory::{mmap, PROT_READ, PROT_WRITE};

        let per_layer = KvCache::required_bytes(n_heads, head_dim, window_size);
        let total_kv = n_layers * per_layer;
        let cache_structs_size = n_layers * core::mem::size_of::<KvCache>();
        let total = total_kv + cache_structs_size;

        let backing = mmap(total, PROT_READ | PROT_WRITE).map_err(|_| ())?;

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
