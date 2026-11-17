//! Target-architecture contract of the kernel entry points, the same one `kernels-sys` keeps
//! (`ffi_decls!`): on aarch64 the C kernels built by `build.rs` answer; on any other target every
//! entry point is a stub that refuses instead of returning a fabricated result, because the
//! micro-kernels are NEON/SMMLA C and a silent zero would corrupt the numerics.
//!
//! Model-free and device-free: the entry points tested here have static contracts, so the tests run
//! in hosted CI on both target families (aarch64 kernels, x86_64 stubs).

#[cfg(target_arch = "aarch64")]
#[test]
fn arm_kernels_record_their_compile_arguments() {
    // The flags are part of the numerics (see build.rs): the +i8mm/+fp16 extensions and FMA
    // contraction decide the machine code, and +fp16fml is deliberately kept off the int8 kernels.
    // build.rs records what it passed to cc; asserting the exact string means a flag edit cannot
    // land silently, and that the supported ARM path stayed as it was.
    assert_eq!(
        crate::KERNEL_CFLAGS,
        "i8g_kernels+i8g_sinks: -O3 -DNDEBUG -std=gnu11 \
-march=armv8.6-a+dotprod+i8mm+fp16+nosve+nosme -ffp-contract=off -fno-fast-math; \
i8g_fattn: -O3 -DNDEBUG -std=gnu11 \
-march=armv8.6-a+dotprod+i8mm+fp16+fp16fml+nosve+nosme -ffp-contract=off -fno-fast-math"
    );
}

#[cfg(not(target_arch = "aarch64"))]
mod not_aarch64 {
    //! The release profile is `panic = "abort"` (engine/Cargo.toml), so a refusal is a hard process
    //! exit, not an unwind: each family of entry points runs in its own copy of this test binary
    //! (selected by an environment variable) and the parent requires that copy to die. A normal exit
    //! means an entry point answered on a target that has no kernels, and that fails the test.
    //!
    //! Every helper below calls one entry point with null operands: the stub refuses before it would
    //! touch one, so no pointer is dereferenced.

    use crate::*;

    const SELECT: &str = "INT8_GEMM_REFUSAL_TEST";

    fn selected() -> Option<String> {
        std::env::var(SELECT).ok()
    }

    fn dot8x8() {
        let x = std::ptr::null::<i8>();
        let o = std::ptr::null_mut::<i32>();
        // SAFETY: null operands, and the callee is the refusing stub on this target.
        unsafe { raw::i8g_dot8x8(x, x, 1, o) }
    }

    fn udot8x8() {
        let x = std::ptr::null::<u8>();
        let w = std::ptr::null::<i8>();
        let o = std::ptr::null_mut::<i32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_udot8x8(x, w, 1, o) }
    }

    fn tile8x8() {
        let p = std::ptr::null::<i8>();
        let f = std::ptr::null::<f32>();
        let y = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_tile8x8(p, p, 1, 1, f, f, y, 0, 0, 0) }
    }

    fn utile8x8() {
        let p = std::ptr::null::<u8>();
        let w = std::ptr::null::<i8>();
        let f = std::ptr::null::<f32>();
        let y = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_utile8x8(p, w, 1, 1, f, f, y, 0, 0) }
    }

    fn row_max() {
        let s = std::ptr::null::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_row_max(s, 4) };
    }

    fn softmax_pack() {
        let f = std::ptr::null::<f32>();
        let s = std::ptr::null_mut::<f32>();
        let c = std::ptr::null_mut::<u8>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_softmax_pack(s, 8, 0.0, f, c, 0, s, s) }
    }

    fn probe_sdot4x4() {
        let x = std::ptr::null::<i8>();
        let o = std::ptr::null_mut::<i32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_probe_sdot4x4(x, x, 1, o) }
    }

    fn rows1() {
        let x = std::ptr::null::<i8>();
        let f = std::ptr::null::<f32>();
        let y = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { raw::i8g_rows1(x, x, 1, 1, f, f, y, 0) }
    }

    fn kernel_abi() {
        // SAFETY: refusing stub.
        unsafe { raw::i8g_kernel_abi() };
    }

    fn quant_chunk_raw() {
        let x = std::ptr::null::<f32>();
        let dst = std::ptr::null_mut::<i8>();
        // SAFETY: null operands; refusing stub.
        unsafe { sinks::raw::i8g_quant_chunk(x, 8, dst, 8) };
    }

    fn silu_gate_raw() {
        let row = std::ptr::null::<f32>();
        let out = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { sinks::raw::i8g_silu_gate(row, 8, out) }
    }

    fn rope_scale_raw() {
        let f = std::ptr::null::<f32>();
        let out = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { sinks::raw::i8g_rope_scale(f, 8, 1.0, f, f, out) }
    }

    fn fattn_qk_f32() {
        let q = std::ptr::null::<f32>();
        let s = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_qk_f32(q, q, 8, 1, s, 8) }
    }

    fn fattn_qk_f16() {
        let q = std::ptr::null::<u16>();
        let s = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_qk_f16(q, q, 8, 1, s, 8) }
    }

    fn fattn_softmax_f32() {
        let s = std::ptr::null::<f32>();
        let p = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_softmax_f32(s, 8, 8, 0.0, p, 0) };
    }

    fn fattn_softmax_f16() {
        let s = std::ptr::null::<f32>();
        let p = std::ptr::null_mut::<u16>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_softmax_f16(s, 8, 8, 0.0, p, 0) };
    }

    fn fattn_pv_f32() {
        let p = std::ptr::null::<f32>();
        let o = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_pv_f32(p, p, 8, 8, 8, o) }
    }

    fn fattn_pv_f16() {
        let p = std::ptr::null::<u16>();
        let o = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_pv_f16(p, p, 8, 8, 8, o) }
    }

    fn fattn_to_f16() {
        let x = std::ptr::null::<f32>();
        let d = std::ptr::null_mut::<u16>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_to_f16(x, 8, d, 8) }
    }

    fn fattn_from_f16() {
        let s = std::ptr::null::<u16>();
        let x = std::ptr::null_mut::<f32>();
        // SAFETY: null operands; refusing stub.
        unsafe { fattn::raw::i8g_fattn_from_f16(s, 8, x) }
    }

    fn packed_from_f32() {
        // The safe API reaches the same stub; no block is fabricated.
        let rows = [1.0f32; 32];
        let _ = Packed::from_f32(&rows, 1, 32, 32);
    }

    fn safe_quant_chunk() {
        let x = [1.0f32; 8];
        let mut dst = [0i8; 8];
        let _ = sinks::quant_chunk(&x, &mut dst, 8);
    }

    /// The worker: reach every entry point of `family`; each call must abort the process.
    fn worker(family: &str) {
        match family {
            "gemm" => {
                dot8x8();
                udot8x8();
                tile8x8();
                utile8x8();
                row_max();
                softmax_pack();
                probe_sdot4x4();
                rows1();
                kernel_abi();
                packed_from_f32();
            }
            "sinks" => {
                quant_chunk_raw();
                silu_gate_raw();
                rope_scale_raw();
                safe_quant_chunk();
            }
            "fattn" => {
                fattn_qk_f32();
                fattn_qk_f16();
                fattn_softmax_f32();
                fattn_softmax_f16();
                fattn_pv_f32();
                fattn_pv_f16();
                fattn_to_f16();
                fattn_from_f16();
            }
            other => panic!("unknown refusal family {other}"),
        }
    }

    #[test]
    fn unsupported_target_refuses_every_entry_point() {
        assert!(
            KERNEL_CFLAGS.starts_with("none"),
            "build.rs must record that no kernel was built: {KERNEL_CFLAGS}"
        );
        // Worker copy: the first stub call ends the process.
        if let Some(family) = selected() {
            worker(&family);
            panic!("{family}: every entry point answered on a target that has no kernels");
        }
        // Parent: one copy per family, all of which must die without answering.
        let exe = std::env::current_exe().expect("current_exe");
        let name = "target_arch_tests::not_aarch64::unsupported_target_refuses_every_entry_point";
        for family in ["gemm", "sinks", "fattn"] {
            let out = std::process::Command::new(&exe)
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(SELECT, family)
                .output()
                .expect("run this test binary");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // The child must have run this test (a renamed test would select nothing and exit 0).
            assert!(
                text.contains("running 1 test"),
                "{family}: the child ran no test; the name in this file is stale:\n{text}"
            );
            assert!(
                !out.status.success(),
                "{family}: an entry point answered on a target that has no kernels:\n{text}"
            );
            assert!(
                text.contains("aarch64 kernel"),
                "{family}: the refusal message is missing from the child's output:\n{text}"
            );
        }
    }
}
