//! Re-exports of the MIR handle types the encoder and layout passes thread
//! through fixups.
//!
//! The instruction encoder ([`crate::encode`]) is otherwise free of MIR
//! knowledge, but a cross-function `call` records the *callee's* [`FnId`] in its
//! relocation so the program layout pass can later resolve the `rel32`. Pulling
//! the id type in through one small alias module keeps that single dependency
//! explicit and keeps `encode.rs` readable.

pub use k2_mir::FnId;
