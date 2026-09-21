//! What a session actually loaded, and the seam a surface asks through.
//!
//! `/memory` and `/context` both answer the same question — "which
//! instruction and memory documents are in play for this cwd, and how big
//! are they" — and the answer spans the plugin boundary: the instruction
//! files come from [`crate::instruction_files`], the auto `MEMORY.md`
//! entrypoint comes from the `memory` feature plugin, which knows the
//! per-project switch that gates it. Neither the ACP server nor the memory
//! plugin may depend on the other, so the shape of the answer and the
//! kernel service that carries it live here, below both.
//!
//! Sizes come from `fs::metadata`, never from reading the file: the token
//! figure is a `bytes / 4` estimate, and one caller is an async request
//! handler where pulling every instruction file into a `String` just to
//! measure its length is waste.

use std::path::PathBuf;
use std::sync::Arc;

use rebon_kernel::Service;

/// JSON/typed name of the loaded-documents seam.
pub const LOADED_DOCUMENTS_SERVICE: &str = "loaded-documents";

/// One memory/instruction file that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMemoryFile {
    /// Path exactly as discovery recorded it — not canonicalized, so it stays
    /// recognizable to the user who wrote it (`@./included.md` keeps its shape).
    pub path: PathBuf,
    /// File size in bytes, from `fs::metadata`.
    pub bytes: u64,
}

impl LoadedMemoryFile {
    /// Approximate token count for this file: one token per four bytes.
    ///
    /// This is the estimate `/memory` and `/context` print. It is deliberately
    /// computed from the on-disk size rather than the post-frontmatter,
    /// post-comment-stripping content, because that is the number both
    /// surfaces have always shown.
    pub fn approx_tokens(&self) -> usize {
        (self.bytes / 4) as usize
    }
}

/// The one provider behind the seam.
pub trait LoadedDocuments: Send + Sync {
    /// The documents a session in `cwd` loaded, in load order, with sizes.
    fn loaded_files(&self, cwd: &str) -> Vec<LoadedMemoryFile>;
}

/// Typed definition for the kernel's `loaded-documents` seat.
pub struct LoadedDocumentsService;

impl Service for LoadedDocumentsService {
    type Interface = dyn LoadedDocuments;
    const NAME: &'static str = LOADED_DOCUMENTS_SERVICE;
}

/// The loaded documents for `cwd`, when the kernel scope `ctx` can see a
/// provider.
///
/// Without one the answer is empty rather than a partial list: the provider
/// is the only thing that knows whether auto-memory is on for this project,
/// and a surface that printed the instruction files alone would be claiming
/// a session loaded less than it did.
pub fn loaded_files(ctx: &rebon_kernel::Context, cwd: &str) -> Vec<LoadedMemoryFile> {
    match ctx.get::<LoadedDocumentsService>() {
        Some(provider) => provider.loaded_files(cwd),
        None => Vec::new(),
    }
}

/// Provide `documents` on `ctx`. The registration is an effect of the
/// context: when `ctx` is disposed (the plugin unloads) the seam goes empty
/// again.
pub fn provide(
    ctx: &rebon_kernel::Context,
    documents: Arc<dyn LoadedDocuments>,
) -> Result<(), rebon_kernel::KernelError> {
    ctx.provide::<LoadedDocumentsService>(documents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    struct Fixed(Vec<LoadedMemoryFile>);

    impl LoadedDocuments for Fixed {
        fn loaded_files(&self, _cwd: &str) -> Vec<LoadedMemoryFile> {
            self.0.clone()
        }
    }

    fn file(path: &str, bytes: u64) -> LoadedMemoryFile {
        LoadedMemoryFile {
            path: PathBuf::from(path),
            bytes,
        }
    }

    /// Four bytes to a token, rounded down — the number both `/memory` and
    /// `/context` have always printed.
    #[test]
    fn a_token_estimate_is_the_byte_count_over_four() {
        assert_eq!(file("a.md", 0).approx_tokens(), 0);
        assert_eq!(file("a.md", 3).approx_tokens(), 0);
        assert_eq!(file("a.md", 4).approx_tokens(), 1);
        assert_eq!(file("a.md", 4001).approx_tokens(), 1000);
    }

    /// A scope with no provider above it answers "nothing loaded" rather
    /// than failing the command.
    #[test]
    fn a_scope_without_a_provider_answers_empty() {
        let kernel = Kernel::new();
        assert!(loaded_files(kernel.context(), "/repo").is_empty());
    }

    /// The provider answers through the seam, and leaves it when its own
    /// scope is disposed.
    #[test]
    fn a_provider_answers_until_its_scope_is_disposed() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("memory");
        provide(&ctx, Arc::new(Fixed(vec![file("/repo/REBON.md", 40)]))).unwrap();

        let loaded = loaded_files(kernel.context(), "/repo");
        assert_eq!(loaded, vec![file("/repo/REBON.md", 40)]);
        assert_eq!(loaded[0].approx_tokens(), 10);

        ctx.dispose();
        assert!(loaded_files(kernel.context(), "/repo").is_empty());
    }
}
