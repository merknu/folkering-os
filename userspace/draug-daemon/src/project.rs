//! Phase C foundation — multi-file project abstraction over Synapse.
//!
//! Today's Draug autonomous loop emits one Rust file at a time
//! (`draug_latest.rs`). Phase C — "build hele multi-fil applikasjoner
//! autonomt om natten" — needs Draug to author and evolve groups of
//! files coherently: `main.rs` next to `lib.rs`, shared modules,
//! retiring obsolete files. This module gives her a `Project`
//! namespace on top of the existing Synapse file primitives so that
//! capability can grow without bolting more state into the daemon
//! itself.
//!
//! # Why a wrapper, not new IPC ops
//!
//! Synapse already exposes `write_file` (with in-place overwrite),
//! `read_file_by_name`, `read_file_chunk`, and `file_count` /
//! `file_by_index` listing. Composing them with a name-prefix
//! convention gives 90 % of multi-file project semantics with no
//! kernel/btree work. The remaining 10 % — actually evicting a file's
//! row from the SQLite `files` table — is filed as Issue #100 (need a
//! `sqlite_delete_file` btree primitive). Until then `delete()`
//! performs a soft-delete by overwriting with empty content, which
//! is what Draug's autonomous loop needs anyway: a tombstone makes
//! the file disappear from her enumeration.
//!
//! # Why this lives in draug-daemon, not libfolk
//!
//! libfolk is strictly heapless — its consumers (`shell`,
//! `intent-service`, etc.) are no_std crates with no global allocator.
//! `Project` uses `String` / `Vec` / `format!` for path qualification,
//! so it has to live somewhere `alloc` is already linked. draug-daemon
//! has that; the abstraction is also deliberately scoped to Draug's
//! authoring use case, so co-locating it with the daemon source is
//! both technically correct and a clean separation of concern.
//!
//! # Naming convention
//!
//! Every file is qualified as `proj/<project>/<file>`. The prefix is
//! deliberately short so it fits comfortably inside Synapse's name-
//! length budget. Slashes inside `<file>` are allowed — the
//! convention is path-shaped, not structurally enforced — so a Draug
//! project can mirror a real Cargo crate layout (`src/main.rs`,
//! `src/lib.rs`, `Cargo.toml`).
//!
//! # What this is NOT
//!
//! - Not a sandbox: files written here live in the OS-side Synapse,
//!   not the host-side `/root/draug-sandbox/` that the proxy compiles.
//!   Wiring the two together (so an OS-side project gets `cargo test`
//!   feedback through the proxy) is the next Phase C deliverable and
//!   lives outside this module.
//! - Not transactional: each `write` / `delete` is an independent
//!   IPC round-trip. Draug must guard her own consistency if a
//!   partial batch is observable.
//! - Not access-controlled: the prefix is convention, not enforcement.
//!   Adequate for an autonomous-agent context where Draug is the
//!   only writer; not adequate as a multi-tenant boundary.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libfolk::sys::synapse::{
    read_file_by_name, read_file_chunk, write_file, write_file_get_rowid,
    SynapseError, SynapseResult,
};

/// One file entry inside a project. `size == 0` is the tombstone
/// encoding (see module note + Issue #100).
#[derive(Debug, Clone)]
pub struct ProjectFile {
    /// File name *within* the project (no `proj/<project>/` prefix).
    pub name: String,
    /// Synapse rowid. `0` indicates an in-place overwrite that
    /// preserved the original rowid.
    pub rowid: u32,
    /// Size in bytes. `0` indicates a soft-deleted tombstone.
    pub size: u32,
}

impl ProjectFile {
    /// True if this entry is a soft-delete tombstone (zero-byte
    /// content). Convenience accessor so callers don't have to
    /// remember the `size == 0` convention.
    pub fn is_deleted(&self) -> bool {
        self.size == 0
    }
}

/// A virtual project namespace inside Synapse. `Project::new` is
/// pure — it constructs the prefix string but issues no IPC. Storage
/// is created lazily on the first `write` call.
#[derive(Debug, Clone)]
pub struct Project {
    name: String,
}

impl Project {
    /// Construct a project handle. No IPC is issued. The project
    /// "exists" implicitly once any file has been written to it.
    ///
    /// `name` is taken as-is — callers are responsible for keeping it
    /// ASCII-safe and free of slashes (slashes break the prefix-
    /// scoping invariant). A future revision may sanitise here.
    pub fn new(name: &str) -> Self {
        Self { name: String::from(name) }
    }

    /// Qualify a project-local file name with the project prefix.
    /// `qualify("src/main.rs")` for project "myapp" returns
    /// `"proj/myapp/src/main.rs"`.
    pub fn qualify(&self, file: &str) -> String {
        format!("proj/{}/{}", self.name, file)
    }

    /// Write `content` to `file` within this project. Returns the
    /// Synapse rowid (or `0` for an in-place overwrite). Empty
    /// `content` is permitted and is the soft-delete encoding —
    /// prefer `delete()` for clarity in callers.
    pub fn write(&self, file: &str, content: &[u8]) -> SynapseResult<u32> {
        write_file_get_rowid(&self.qualify(file), content)
    }

    /// Read the entire file body into a `Vec<u8>`. Goes through the
    /// 8-byte chunked path because libfolk's no_std runtime doesn't
    /// have a `read_file_full` helper today; for very large files
    /// (> ~16 KiB) callers should reach for
    /// `libfolk::sys::synapse::read_file_shmem` directly to avoid
    /// the per-chunk IPC cost.
    pub fn read(&self, file: &str) -> SynapseResult<Vec<u8>> {
        let info = read_file_by_name(&self.qualify(file))?;
        let mut out = Vec::with_capacity(info.size as usize);
        let mut offset = 0u32;
        while offset < info.size {
            let chunk = read_file_chunk(info.file_id, offset)?;
            // `read_file_chunk` packs 8 bytes into the u64 reply;
            // Synapse zero-pads the tail so trailing bytes are safe
            // to read but must be truncated by `info.size`.
            let bytes = chunk.to_le_bytes();
            let remaining = (info.size - offset) as usize;
            let take = remaining.min(8);
            out.extend_from_slice(&bytes[..take]);
            offset += take as u32;
        }
        Ok(out)
    }

    /// Soft-delete `file` by overwriting it with empty content. The
    /// row stays in Synapse's `files` table (real eviction needs the
    /// btree DELETE primitive tracked in Issue #100); enumeration
    /// callers filter tombstones via `ProjectFile::is_deleted`.
    ///
    /// Returns `Ok` even if the file did not exist — this matches
    /// Draug's "make sure it's gone" intent better than an error
    /// path callers would have to suppress anyway.
    pub fn delete(&self, file: &str) -> SynapseResult<()> {
        match write_file(&self.qualify(file), &[]) {
            Ok(()) => Ok(()),
            Err(SynapseError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// True iff `file` exists as a non-tombstone entry. Cheap — uses
    /// the size returned by `read_file_by_name`, no body read.
    pub fn exists(&self, file: &str) -> bool {
        match read_file_by_name(&self.qualify(file)) {
            Ok(info) => info.size > 0,
            Err(_) => false,
        }
    }

    /// The qualified prefix for this project, useful for callers that
    /// need to build a wire command directly (e.g. the `LIST_FILES_BY_PREFIX`
    /// op proposed in Issue #100, which would take this string verbatim).
    pub fn prefix(&self) -> String {
        format!("proj/{}/", self.name)
    }

    /// User-facing project name. Returned by reference so callers
    /// don't re-allocate just to read it back.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    // The full crate is no_std + custom target, so cargo test won't
    // run these in CI today (tracked in
    // `feedback_folkering_userspace_test_arch.md`). They serve as
    // compile-time documentation of the qualified-path contract;
    // future host-runnable harness will exercise them.

    use super::*;
    use alloc::string::ToString;

    #[test]
    fn qualify_builds_prefixed_path() {
        let p = Project::new("myapp");
        assert_eq!(p.qualify("src/main.rs"), "proj/myapp/src/main.rs".to_string());
    }

    #[test]
    fn prefix_includes_trailing_slash() {
        let p = Project::new("myapp");
        assert_eq!(p.prefix(), "proj/myapp/".to_string());
    }

    #[test]
    fn name_round_trips() {
        let p = Project::new("draug-overnight-2026-05-02");
        assert_eq!(p.name(), "draug-overnight-2026-05-02");
    }

    #[test]
    fn project_file_tombstone_check() {
        let live = ProjectFile { name: "main.rs".into(), rowid: 7, size: 42 };
        let dead = ProjectFile { name: "old.rs".into(), rowid: 8, size: 0 };
        assert!(!live.is_deleted());
        assert!(dead.is_deleted());
    }
}
