//! oxa-desk-core: the store and the view models of the desktop app.
//!
//! This crate is the only place where the desktop touches the oxa crates (session, config,
//! interactive, rpc, the CLI library). It never names a GPUI type, so everything here is tested
//! without a window. `tools/ci/desk_contract.py` checks both directions.
//!
//! Empty until the contract types land (build plan, section 1.4).
