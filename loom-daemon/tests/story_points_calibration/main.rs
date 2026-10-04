//! Story-points calibration contract tests (#9434, epic #9429). One binary,
//! three modules: `shared` fixtures, `static_contracts` (the committed
//! artifacts' shape), `execution` (the chain run over a synthetic fleet).
//! Split from a single file by the file-size ratchet; the tests are unchanged.

#[path = "../support/d1_sqlite.rs"]
mod d1_sqlite;
mod execution;
mod shared;
mod static_contracts;
