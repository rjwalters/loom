//! Work-origin detection. Loom no longer stamps provenance trailers or PR-body
//! markers (#11776); only [`origin`], which `pr_planning` and the dispatchers
//! read back, remains.
pub mod origin;
