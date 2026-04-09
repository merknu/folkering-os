//! Utility functions for the compositor.
//! String formatting, app categorization, intent matching.

extern crate alloc;
use alloc::string::String;

/// Format a usize as a decimal string into buffer, return slice
pub fn format_usize(n: usize, buf: &mut [u8; 16]) -> &str {
    if n == 0 {
        buf[0] = b'0';
        return unsafe { core::str::from_utf8_unchecked(&buf[..1]) };
    }
    let mut val = n;
    let mut i = 0;
    // Write digits in reverse
    while val > 0 && i < 16 {
        buf[15 - i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    // Copy to start
    for j in 0..i {
        buf[j] = buf[16 - i + j];
    }
    unsafe { core::str::from_utf8_unchecked(&buf[..i]) }
}

pub fn format_arena_line<'a>(buf: &'a mut [u8; 32], kb: usize) -> &'a str {
    let prefix = b"Arena: ";
    let suffix = b"KB";
    buf[..7].copy_from_slice(prefix);
    let mut num_buf = [0u8; 16];
    let num_str = format_usize(kb, &mut num_buf);
    let num_bytes = num_str.as_bytes();
    buf[7..7 + num_bytes.len()].copy_from_slice(num_bytes);
    let end = 7 + num_bytes.len();
    buf[end..end + 2].copy_from_slice(suffix);
    unsafe { core::str::from_utf8_unchecked(&buf[..end + 2]) }
}

/// Auto-categorize an app name into a folder index (0-5)
pub fn categorize_app(name: &str) -> usize {
    let n = name.as_bytes();
    // System: monitor, clock, system, about, info, settings, status
    if find_ci(n, b"monitor") || find_ci(n, b"clock") || find_ci(n, b"system")
        || find_ci(n, b"about") || find_ci(n, b"info") || find_ci(n, b"setting")
        || find_ci(n, b"status") { return 0; }
    // Games: game, tetris, snake, pong, ball, bounce, breakout, chess, maze
    if find_ci(n, b"game") || find_ci(n, b"tetris") || find_ci(n, b"snake")
        || find_ci(n, b"pong") || find_ci(n, b"ball") || find_ci(n, b"bounce")
        || find_ci(n, b"breakout") || find_ci(n, b"chess") || find_ci(n, b"maze") { return 1; }
    // Creative: paint, draw, art, sketch, pixel, color, canvas, music
    if find_ci(n, b"paint") || find_ci(n, b"draw") || find_ci(n, b"art")
        || find_ci(n, b"sketch") || find_ci(n, b"pixel") || find_ci(n, b"color")
        || find_ci(n, b"canvas") || find_ci(n, b"music") { return 2; }
    // Tools: calc, timer, note, tool, convert, edit, text, writer
    if find_ci(n, b"calc") || find_ci(n, b"timer") || find_ci(n, b"note")
        || find_ci(n, b"tool") || find_ci(n, b"convert") || find_ci(n, b"edit")
        || find_ci(n, b"text") || find_ci(n, b"writer") { return 3; }
    // Demos: demo, gradient, test, screen, star, hello
    if find_ci(n, b"demo") || find_ci(n, b"gradient") || find_ci(n, b"test")
        || find_ci(n, b"screen") || find_ci(n, b"star") || find_ci(n, b"hello") { return 4; }
    5 // Other
}

/// Case-insensitive substring search in byte slices
pub fn find_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() { return false; }
    for i in 0..=(haystack.len() - needle.len()) {
        let mut ok = true;
        for j in 0..needle.len() {
            let a = if haystack[i + j] >= b'A' && haystack[i + j] <= b'Z' { haystack[i + j] + 32 } else { haystack[i + j] };
            let b = if needle[j] >= b'A' && needle[j] <= b'Z' { needle[j] + 32 } else { needle[j] };
            if a != b { ok = false; break; }
        }
        if ok { return true; }
    }
    false
}

pub fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    if haystack.len() < needle.len() { return false; }
    for (a, b) in haystack.bytes().zip(needle.bytes()) {
        let la = if a >= b'A' && a <= b'Z' { a + 32 } else { a };
        let lb = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
        if la != lb { return false; }
    }
    true
}

/// Flags parsed from agent command line
pub struct AgentFlags<'a> {
    pub force: bool,                        // --force: skip cache
    pub tweak_msg: Option<&'a str>,         // --tweak "modification": modify cached version
}

/// Parse agent flags from command string.
/// Returns (flags, remaining_prompt).
pub fn parse_agent_flags(input: &str) -> (AgentFlags<'_>, &str) {
    let mut force = false;
    let mut tweak_msg: Option<&str> = None;
    let mut rest = input;

    // Parse --force
    if rest.starts_with("--force ") {
        force = true;
        rest = rest[8..].trim_start();
    }

    // Parse --tweak "msg"
    if rest.starts_with("--tweak ") {
        let after = rest[8..].trim_start();
        if after.starts_with('"') {
            if let Some(end) = after[1..].find('"') {
                tweak_msg = Some(&after[1..1 + end]);
                rest = after[2 + end..].trim_start();
            }
        } else {
            // No quotes — take first word as tweak message
            let end = after.find(' ').unwrap_or(after.len());
            tweak_msg = Some(&after[..end]);
            rest = if end < after.len() { after[end..].trim_start() } else { "" };
        }
    }

    (AgentFlags { force, tweak_msg }, rest)
}

pub struct IntentEntry {
    pub app: &'static str,
    pub keywords: &'static [&'static str],
}

pub const INTENT_MAP: &[IntentEntry] = &[
    IntentEntry {
        app: "calc",
        keywords: &[
            "calc", "calculator", "kalkulator", "math", "matte",
            "regn", "beregn", "compute", "tax", "skatt",
            "add", "subtract", "multiply", "divide", "sum",
            "budget", "prosent", "percent",
        ],
    },
    IntentEntry {
        app: "greet",
        keywords: &[
            "greet", "greeter", "hello", "hei", "hilsen", "name",
        ],
    },
    IntentEntry {
        app: "folkpad",
        keywords: &[
            "note", "folkpad", "pad", "notat", "skriv", "memo",
        ],
    },
];

/// Sjekk om input matcher en apps intent-keywords.
/// Returnerer Some(app_name) ved match, None ellers.
///
/// Scoring:
/// - Eksakt match (case-insensitive): +10 poeng
/// - Prefix-match (word starts with kw, eller omvendt, kw.len()>=3): +kw.len() poeng
/// - Terskel: score >= 4 for å matche
pub fn try_intent_match(input: &str) -> Option<&'static str> {
    let mut best_app: Option<&'static str> = None;
    let mut best_score: usize = 0;

    for entry in INTENT_MAP {
        let mut score = 0usize;
        for word in input.split(|c: char| !c.is_ascii_alphanumeric()) {
            let w = word.trim();
            if w.is_empty() { continue; }
            for kw in entry.keywords {
                if w.len() == kw.len() && starts_with_ci(w, kw) {
                    // Eksakt match (case-insensitive)
                    score += 10;
                } else if kw.len() >= 3 && (starts_with_ci(w, kw) || starts_with_ci(kw, w)) {
                    // Prefix-match
                    score += kw.len();
                }
            }
        }
        if score > best_score {
            best_score = score;
            best_app = Some(entry.app);
        }
    }

    if best_score >= 4 { best_app } else { None }
}
