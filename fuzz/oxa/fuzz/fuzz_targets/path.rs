#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| oxa_fuzz::path(data));
