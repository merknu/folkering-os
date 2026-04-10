//! Audio syscalls: PCM playback and beep tone via AC97.

pub fn syscall_audio_play(samples_ptr: u64, samples_count: u64) -> u64 {
    if samples_count == 0 || samples_count > 1_000_000 { return u64::MAX; }
    let samples = unsafe {
        core::slice::from_raw_parts(samples_ptr as *const i16, samples_count as usize)
    };
    if crate::drivers::ac97::play_pcm(samples) { 0 } else { u64::MAX }
}

pub fn syscall_audio_beep(duration_ms: u64) -> u64 {
    if crate::drivers::ac97::beep(duration_ms as u32) { 0 } else { u64::MAX }
}
