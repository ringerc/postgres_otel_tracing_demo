//! Build script.
//!
//! Two responsibilities:
//!
//!  1. Run bindgen against `wrapper.h` to materialize Rust FFI
//!     declarations for `OtelSpan`, the rendezvous-published
//!     `OtelTracingApi`, and `find_rendezvous_variable`.
//!
//!  2. Tell rustc to allow unresolved symbols in the produced
//!     cdylib --- postgres-loadable modules link against `postgres.h`
//!     declarations whose definitions live in the postgres backend
//!     binary, NOT in any library we can name at link time.  The
//!     dynamic linker resolves them when postgres loads the module.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn pg_config(arg: &str) -> String {
    let out = Command::new(env::var("PG_CONFIG").unwrap_or_else(|_| "pg_config".into()))
        .arg(arg)
        .output()
        .expect("failed to run pg_config; ensure it is on PATH or set $PG_CONFIG");
    if !out.status.success() {
        panic!("pg_config {} failed", arg);
    }
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=PG_CONFIG");

    let server_inc = pg_config("--includedir-server");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        // -I.../server lets wrapper.h find postgres.h, fmgr.h, etc.
        // -I.../server/extension lets <otel_api/otel.h> resolve (other
        // contribs that install headers go in there too).
        .clang_arg(format!("-I{}", server_inc))
        .clang_arg(format!("-I{}/extension", server_inc))
        .clang_arg("-D_GNU_SOURCE")
        // Only emit Rust for the otel api surface, the postgres
        // helpers we call directly, and the magic-block plumbing
        // we use to expose Pg_magic_func from pure Rust (no C shim;
        // the pgrx-style approach).  Everything else stays out.
        .allowlist_type("Otel.*")
        .allowlist_var("OTEL_.*")
        .allowlist_type("Pg_magic_struct")
        .allowlist_type("Pg_abi_values")
        .allowlist_var("PG_VERSION_NUM")
        .allowlist_var("FUNC_MAX_ARGS")
        .allowlist_var("INDEX_MAX_KEYS")
        .allowlist_var("NAMEDATALEN")
        .allowlist_var("FLOAT8PASSBYVAL")
        .allowlist_var("FMGR_ABI_EXTRA")
        .allowlist_function("find_rendezvous_variable")
        // MarkGUCPrefixReserved("oteltracingdemo") --- we call this
        // from _PG_init to claim the "oteltracingdemo." GUC namespace
        // (currently unused but reserved for future demo-specific
        // GUCs; also gets us the typo-protection warning if anyone
        // sets an oteltracingdemo.* unknown).
        .allowlist_function("MarkGUCPrefixReserved")
        // Make the generated structs Copy/Clone-friendly for our
        // FFI translation glue.  (Default = true for POD types
        // bindgen recognizes; explicit for clarity.)
        .derive_default(true)
        .derive_debug(false)
        // Don't generate layout tests; they require building with
        // a C compiler in test mode which complicates packaging.
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to generate FFI for <otel_api/otel.h>");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("could not write bindings.rs");

    // Postgres-loadable-module convention: undefined references in
    // the cdylib are resolved by postgres at dlopen() time.  Linux's
    // `-shared` already tolerates undefined symbols by default; macOS
    // needs an explicit flag.
    if cfg!(target_os = "macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-undefined,dynamic_lookup");
    }
}
